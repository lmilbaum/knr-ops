//! knr-bootstrap – One-time imperative bootstrap for the management cluster.
//! Everything after this program runs is driven by GitOps (Flux).
//!
//! Rust port of `bootstrap.sh`. Unlike the script, reruns are safe by
//! default: an existing healthy 'mgmt' cluster is reused and every step is
//! idempotent, so a partially failed bootstrap can be resumed by rerunning.
//! Pass --recreate to delete and rebuild the cluster instead.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const GIT_BRANCH: &str = "main";
const REGISTRY_NAME: &str = "knr-registry";
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Profile {
    #[value(name = "local-host")]
    LocalHost,
    #[value(name = "aws")]
    Aws,
}

impl Profile {
    /// Parse a profile name exactly as the script's `case` accepts it.
    fn parse_value(value: &str) -> Result<Profile> {
        Profile::value_variants()
            .iter()
            .copied()
            .find(|p| {
                p.to_possible_value()
                    .is_some_and(|pv| pv.matches(value, false))
            })
            .with_context(|| {
                format!("unsupported profile '{value}' (expected 'local-host' or 'aws')")
            })
    }
}

/// Resolve the active profile the way bootstrap.sh does
/// (`PROFILE="${KNR_OPS_PROFILE:-${1:-aws}}"`): a non-empty
/// KNR_OPS_PROFILE wins over the positional argument, then aws. clap
/// resolves positional-over-env (the opposite), so the env var is read
/// manually instead of via #[arg(env)].
fn resolve_profile(env: Option<&str>, positional: Option<Profile>) -> Result<Profile> {
    if let Some(value) = env.filter(|v| !v.is_empty()) {
        return Profile::parse_value(value);
    }
    Ok(positional.unwrap_or(Profile::Aws))
}

impl std::fmt::Display for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Profile::LocalHost => write!(f, "local-host"),
            Profile::Aws => write!(f, "aws"),
        }
    }
}

/// One-time imperative bootstrap for the knr-ops management cluster.
///
/// Reruns are safe: an existing healthy 'mgmt' cluster is reused and all
/// steps are idempotent, so a partial failure can be resumed by rerunning.
#[derive(Parser, Debug)]
#[command(name = "knr-bootstrap", version, about)]
struct Cli {
    /// Deployment profile (a non-empty KNR_OPS_PROFILE takes precedence
    /// over this argument, matching bootstrap.sh; default: aws)
    #[arg(value_enum)]
    profile: Option<Profile>,

    /// Delete and recreate an existing 'mgmt' cluster instead of reusing it.
    /// Required when the profile or the kind/registry configuration changed
    /// since the cluster was created (the reuse path does not detect drift).
    #[arg(long)]
    recreate: bool,

    /// Host port for the local container registry (local-host profile)
    #[arg(long, env = "REGISTRY_PORT", default_value_t = 5001)]
    registry_port: u16,

    /// Retries (1s apart) while waiting for the local registry API
    #[arg(long, env = "REGISTRY_READY_RETRIES", default_value_t = 120)]
    registry_ready_retries: u32,

    /// kubectl-style timeout for the management node Ready wait (e.g. 120s)
    #[arg(long, env = "NODE_READY_TIMEOUT", default_value = "120s")]
    node_ready_timeout: String,

    /// kubectl-style timeout for local reconciliation waits (e.g. 15m)
    #[arg(long, env = "LOCAL_RECONCILE_TIMEOUT", default_value = "15m")]
    local_reconcile_timeout: String,

    /// Container engine to use (docker or podman); auto-detected when omitted
    #[arg(long, env = "CONTAINER_ENGINE")]
    container_engine: Option<String>,

    /// HTTPS GitHub repository URL (required for the aws profile)
    #[arg(long, env = "GIT_REPO_URL")]
    git_repo_url: Option<String>,

    /// GitHub PAT with read access to the repo (required for the aws profile)
    #[arg(long, env = "GITHUB_TOKEN", hide_env_values = true)]
    github_token: Option<String>,

    /// Username for the Flux GitHub secret used to clone the repo
    #[arg(long, env = "GITHUB_USER", default_value = "git")]
    github_user: String,

    /// Path to the SOPS age key file (aws profile)
    #[arg(long, env = "AGE_KEY_FILE", default_value = "age.agekey")]
    age_key_file: PathBuf,

    /// Override the age public key instead of parsing it from the key file
    #[arg(long, env = "AGE_PUBLIC_KEY")]
    age_public_key: Option<String>,

    /// OCI repository name for the local-host artifact. Caveat: the
    /// workload FluxInstance still pins knr-ops:latest in Git, so a
    /// non-default value only serves the management instance until #92
    /// templates it.
    #[arg(long, env = "OCI_REPOSITORY", default_value = "knr-ops")]
    oci_repository: String,

    /// OCI tag for the local-host artifact. Same caveat as
    /// --oci-repository: the workload FluxInstance pins latest in Git.
    #[arg(long, env = "OCI_TAG", default_value = "latest")]
    oci_tag: String,
}

impl Cli {
    /// The active profile, resolved the way bootstrap.sh resolves it:
    /// non-empty KNR_OPS_PROFILE over the positional argument, aws last.
    fn profile(&self) -> Result<Profile> {
        resolve_profile(
            std::env::var("KNR_OPS_PROFILE").ok().as_deref(),
            self.profile,
        )
    }
}

// ── Pure helpers (unit-tested) ────────────────────────────────────────────────

/// Parse `owner/repo` out of an HTTPS GitHub repository URL.
fn parse_github_repo(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("https://github.com/")?;
    let repo = rest.trim_end_matches('/').trim_end_matches(".git");
    let (owner, name) = repo.split_once('/')?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        return None;
    }
    Some(repo)
}

/// Validate an age key file's three required fields; returns missing field names.
fn validate_age_key(content: &str) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !content.lines().any(|l| l.starts_with("# created:")) {
        missing.push("# created: header");
    }
    if !content.lines().any(|l| l.starts_with("# public key:")) {
        missing.push("# public key: comment");
    }
    if !content.lines().any(|l| l.starts_with("AGE-SECRET-KEY-")) {
        missing.push("AGE-SECRET-KEY- line");
    }
    missing
}

/// Extract the public key from a validated age key file.
fn extract_age_pubkey(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|l| l.strip_prefix("# public key: "))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Extract the numeric host port from `<engine> port` output ("0.0.0.0:32771").
fn extract_workload_port(port_output: &str) -> Option<String> {
    port_output
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string)
}

/// Render the kind cluster config, mirroring the script's heredoc.
fn render_kind_config(profile: Profile, registry_port: u16, engine_sock: &str) -> String {
    let registry_patch = if profile == Profile::LocalHost {
        format!(
            "containerdConfigPatches:\n  - |-\n    [plugins.\"io.containerd.grpc.v1.cri\".registry.mirrors.\"localhost:{registry_port}\"]\n      endpoint = [\"http://{REGISTRY_NAME}:5000\"]\n"
        )
    } else {
        String::new()
    };
    format!(
        "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n{registry_patch}nodes:\n  - role: control-plane\n    extraMounts:\n      - hostPath: {engine_sock}\n        containerPath: /var/run/docker.sock\n"
    )
}

/// Tools required on PATH for the given profile.
fn required_tools(profile: Profile) -> Vec<&'static str> {
    // The binary owns the HTTP checks the script used curl for. curl stays
    // required for local-host anyway: the mise oci-push task shells out to
    // curl for its registry availability check. flux and clusterctl are
    // exercised by the local-host reconciliation watch (Step 5).
    let mut tools = vec!["kind", "helm", "kubectl"];
    if profile == Profile::LocalHost {
        tools.extend(["mise", "flux", "clusterctl", "curl"]);
    }
    tools
}

// ── Process helpers ───────────────────────────────────────────────────────────

/// Is `cmd` an executable file somewhere on PATH?
fn command_exists(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(cmd)))
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &Path) -> bool {
    p.is_file()
}

/// Run a command with inherited stdio; error if it exits nonzero.
///
/// Secret safety: no secret material is ever passed on argv anywhere in this
/// program (secrets travel via stdin manifests), so command lines are safe to
/// echo verbatim in errors.
async fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .await
        .with_context(|| format!("failed to spawn '{cmd}'"))?;
    if !status.success() {
        bail!("'{cmd} {}' failed with {status}", args.join(" "));
    }
    Ok(())
}

/// Run a command silently; report only whether it succeeded.
async fn run_quiet(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run a command and capture stdout (stderr inherited); error on nonzero exit.
async fn capture(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .stderr(Stdio::inherit())
        .output()
        .await
        .with_context(|| format!("failed to spawn '{cmd}'"))?;
    if !out.status.success() {
        bail!("'{cmd} {}' failed with {}", args.join(" "), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command and capture stdout, ignoring the exit status (like `cmd || true`).
async fn capture_lossy(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .await
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Run a command with `input` piped to stdin and stdio otherwise inherited.
/// Error messages include argv only, never stdin content, so manifests
/// containing secret material are safe to pass here.
async fn run_with_stdin(cmd: &str, args: &[&str], input: &str) -> Result<()> {
    let mut child = Command::new(cmd)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn '{cmd}'"))?;
    child
        .stdin
        .take()
        .context("child stdin unavailable")?
        .write_all(input.as_bytes())
        .await?;
    let status = child.wait().await?;
    if !status.success() {
        bail!("'{cmd} {}' failed with {status}", args.join(" "));
    }
    Ok(())
}

/// Apply a Kubernetes manifest (JSON) via `kubectl apply -f -`. Idempotent,
/// and keeps secret values off argv.
async fn kubectl_apply(manifest: &serde_json::Value) -> Result<()> {
    run_with_stdin("kubectl", &["apply", "-f", "-"], &manifest.to_string()).await
}

/// Kills the wrapped child process when dropped (best-effort).
struct ChildGuard(tokio::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.start_kill();
    }
}

// ── Preflight ─────────────────────────────────────────────────────────────────

struct AwsContext {
    git_repo_url: String,
    github_user: String,
    github_token: String,
    age_key_content: String,
    age_pubkey: String,
}

struct Preflight {
    engine: String,
    engine_sock: String,
    aws: Option<AwsContext>,
}

async fn preflight_checks(cli: &Cli, http: &reqwest::Client) -> Result<Preflight> {
    let profile = cli.profile()?;
    // Report every missing tool at once instead of failing on the first.
    let missing: Vec<&str> = required_tools(profile)
        .into_iter()
        .filter(|t| !command_exists(t))
        .collect();
    if !missing.is_empty() {
        bail!("missing required tools in PATH: {}", missing.join(", "));
    }

    let aws = if profile == Profile::Aws {
        Some(preflight_aws(cli, http).await?)
    } else {
        None
    };

    // Detect and select a running container engine. Note: when using podman via
    // the docker CLI shim (e.g., on macOS), `docker --version` reports podman;
    // we check for that case first.
    let engine = match cli.container_engine.clone() {
        Some(e) => e,
        None => {
            if command_exists("docker") && run_quiet("docker", &["info"]).await {
                let version = capture_lossy("docker", &["--version"]).await;
                if version.to_lowercase().contains("podman") {
                    "podman".to_string()
                } else {
                    "docker".to_string()
                }
            } else if command_exists("podman") && run_quiet("podman", &["info"]).await {
                "podman".to_string()
            } else {
                bail!("No running container engine found (tried docker and podman)");
            }
        }
    };

    let engine_sock = match engine.as_str() {
        "docker" => {
            if !run_quiet("docker", &["info"]).await {
                bail!("Docker daemon not running");
            }
            "/var/run/docker.sock".to_string()
        }
        "podman" => {
            if !run_quiet("podman", &["info"]).await {
                bail!("Podman is not running (is 'podman machine' started?)");
            }
            std::env::set_var("KIND_EXPERIMENTAL_PROVIDER", "podman");
            let mut sock = capture_lossy(
                "podman",
                &["info", "--format", "{{.Host.RemoteSocket.Path}}"],
            )
            .await
            .trim()
            .trim_start_matches("unix://")
            .to_string();
            if sock.is_empty() {
                sock = "/run/podman/podman.sock".to_string();
                eprintln!(
                    ">>> WARNING: Could not detect the podman API socket path; assuming {sock}"
                );
            }
            sock
        }
        other => bail!("Unsupported CONTAINER_ENGINE '{other}' (expected 'docker' or 'podman')"),
    };

    Ok(Preflight {
        engine,
        engine_sock,
        aws,
    })
}

async fn preflight_aws(cli: &Cli, http: &reqwest::Client) -> Result<AwsContext> {
    let github_token = cli
        .github_token
        .clone()
        .context("GITHUB_TOKEN must be set (a PAT with read access to the repo)")?;
    let git_repo_url = cli
        .git_repo_url
        .clone()
        .context("GIT_REPO_URL must be set")?;

    // GITHUB_USER is used in the Flux GitHub secret for repo clone authentication.
    let github_user = cli.github_user.clone();

    let github_repo = parse_github_repo(&git_repo_url)
        .context("GIT_REPO_URL must be an HTTPS GitHub repository URL")?;

    let branch_path = GIT_BRANCH.replace('/', "%2F");
    let url = format!("https://api.github.com/repos/{github_repo}/branches/{branch_path}");
    let status = http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("Authorization", format!("Bearer {github_token}"))
        .send()
        .await
        .map(|r| r.status().as_u16())
        .unwrap_or(0);
    if status != 200 {
        bail!(
            "GitHub repository or branch '{GIT_BRANCH}' is unavailable at '{git_repo_url}' (HTTP {status})"
        );
    }

    let age_key_file = &cli.age_key_file;
    if !age_key_file.is_file() {
        bail!(
            "age key file not found at '{}'.\n       Generate one with:  mise run sops-keygen\n       and add its PUBLIC key to .sops.yaml. See docs/secrets.md.",
            age_key_file.display()
        );
    }

    // Validate age key file format first (before attempting to extract the
    // public key). This avoids silently proceeding with a malformed file.
    let age_key_content = std::fs::read_to_string(age_key_file)
        .with_context(|| format!("failed to read '{}'", age_key_file.display()))?;
    let missing_fields = validate_age_key(&age_key_content);
    if !missing_fields.is_empty() {
        bail!(
            "'{}' is not a valid age key file.\n       Missing: {}",
            age_key_file.display(),
            missing_fields.join(", ")
        );
    }

    // Now safely extract the public key (validation already passed).
    let age_pubkey = cli
        .age_public_key
        .clone()
        .filter(|k| !k.is_empty())
        .or_else(|| extract_age_pubkey(&age_key_content))
        .with_context(|| {
            format!(
                "Cannot determine age public key from '{}' or from AGE_PUBLIC_KEY env var.\n       Set AGE_PUBLIC_KEY in .env, or regenerate the key with: mise run sops-keygen",
                age_key_file.display()
            )
        })?;

    Ok(AwsContext {
        git_repo_url,
        github_user,
        github_token,
        age_key_content,
        age_pubkey,
    })
}

// ── Steps ─────────────────────────────────────────────────────────────────────

/// Ensure the kind 'mgmt' cluster exists and is healthy.
///
/// Default: reuse an existing cluster after validating that its context is
/// reachable and all nodes go Ready; this makes reruns non-destructive and
/// lets a partially failed bootstrap resume. With --recreate (or when no
/// cluster exists) the cluster is (re)built from the rendered config.
async fn ensure_kind_cluster(cli: &Cli, engine_sock: &str) -> Result<()> {
    let clusters = capture_lossy("kind", &["get", "clusters"]).await;
    let exists = clusters.lines().any(|l| l.trim() == "mgmt");
    let node_ready_timeout = format!("--timeout={}", cli.node_ready_timeout);

    if exists && !cli.recreate {
        println!(">>> Reusing existing kind cluster 'mgmt' (pass --recreate to replace it)...");
        println!(">>> Validating existing cluster health...");
        if run_quiet("kubectl", &["config", "use-context", "kind-mgmt"]).await
            && run_quiet(
                "kubectl",
                &[
                    "wait",
                    "--for=condition=Ready",
                    "node",
                    "--all",
                    &node_ready_timeout,
                ],
            )
            .await
        {
            println!(">>> Existing cluster 'mgmt' is healthy; continuing.");
            return Ok(());
        }
        bail!(
            "existing kind cluster 'mgmt' is not healthy (context unreachable or nodes not Ready within {}); rerun with --recreate to replace it",
            cli.node_ready_timeout
        );
    }

    if exists {
        println!(">>> Cluster 'mgmt' exists and --recreate was given – recreating...");
        run("kind", &["delete", "cluster", "--name", "mgmt"]).await?;
    }

    println!(">>> Creating kind cluster 'mgmt'...");
    // Mount the host's container engine socket into the kind node at the
    // standard Docker socket path so in-cluster components can reach a
    // Docker-compatible API whether the backend is Docker or Podman.
    let kind_config = render_kind_config(cli.profile()?, cli.registry_port, engine_sock);
    run_with_stdin(
        "kind",
        &["create", "cluster", "--name", "mgmt", "--config", "-"],
        &kind_config,
    )
    .await?;

    println!(">>> Waiting for cluster node to be ready...");
    // Explicitly switch kubectl to use the kind cluster context.
    run("kubectl", &["config", "use-context", "kind-mgmt"]).await?;
    run(
        "kubectl",
        &[
            "wait",
            "--for=condition=Ready",
            "node",
            "--all",
            &node_ready_timeout,
        ],
    )
    .await?;
    Ok(())
}

async fn bootstrap_local_registry(cli: &Cli, engine: &str, http: &reqwest::Client) -> Result<()> {
    println!(">>> Bootstrapping local container registry...");
    let port = cli.registry_port;
    let name_filter = format!("name=^{REGISTRY_NAME}$");

    let exists = capture_lossy(
        engine,
        &[
            "ps",
            "-a",
            "--filter",
            &name_filter,
            "--format",
            "{{.Names}}",
        ],
    )
    .await
    .lines()
    .any(|l| l.trim() == REGISTRY_NAME);

    if !exists {
        println!("    Creating registry container '{REGISTRY_NAME}'...");
        let publish = format!("127.0.0.1:{port}:5000");
        capture(
            engine,
            &[
                "run",
                "-d",
                "--name",
                REGISTRY_NAME,
                "--network",
                "kind",
                "-p",
                &publish,
                "registry:2",
            ],
        )
        .await?;
        println!("    Registry created and running: localhost:{port}");
    } else {
        let running = capture_lossy(
            engine,
            &["ps", "--filter", &name_filter, "--format", "{{.Names}}"],
        )
        .await
        .lines()
        .any(|l| l.trim() == REGISTRY_NAME);
        if !running {
            println!("    Restarting stopped registry...");
            capture(engine, &["start", REGISTRY_NAME]).await?;
            println!("    Registry restarted: localhost:{port}");
        } else {
            println!("    Registry already running: localhost:{port}");
        }
    }

    println!(">>> Waiting for local registry API at localhost:{port}...");
    let registry_url = format!("http://localhost:{port}/v2/");
    let mut ready = false;
    for attempt in 0..=cli.registry_ready_retries {
        match http.get(&registry_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                ready = true;
                break;
            }
            _ => {}
        }
        if attempt < cli.registry_ready_retries {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    if !ready {
        bail!("local registry did not become ready at localhost:{port}");
    }

    // Tell the cluster about the local registry (apply = rerun-safe).
    kubectl_apply(&json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "local-registry-config",
            "namespace": "kube-system",
        },
        "data": { "registry-url": format!("{REGISTRY_NAME}:5000") },
    }))
    .await?;

    println!(">>> Publishing initial OCI artifact from the local Git checkout...");
    // Forward the resolved values: the mise oci-push task reads them from
    // its own environment, so CLI flags must not stop at this boundary.
    let status = Command::new("mise")
        .args(["-E", "local-host", "run", "oci-push"])
        .env("REGISTRY_PORT", cli.registry_port.to_string())
        .env("OCI_REPOSITORY", &cli.oci_repository)
        .env("OCI_TAG", &cli.oci_tag)
        .status()
        .await
        .with_context(|| "failed to spawn 'mise'")?;
    if !status.success() {
        bail!("'mise -E local-host run oci-push' failed with {status}");
    }
    println!(
        ">>> Initial OCI artifact is available at oci://localhost:{port}/{repo}:{tag}",
        repo = cli.oci_repository,
        tag = cli.oci_tag
    );
    Ok(())
}

async fn install_flux_operator(registry_config: &Path) -> Result<()> {
    println!(">>> Installing Flux Operator...");
    let cfg = registry_config.to_string_lossy();
    // upgrade --install (script used install): rerun-safe after a partial failure.
    run(
        "helm",
        &[
            "upgrade",
            "--install",
            "flux-operator",
            "oci://ghcr.io/controlplaneio-fluxcd/charts/flux-operator",
            "--namespace",
            "flux-system",
            "--create-namespace",
            "--wait",
            "--timeout",
            "10m",
            "--registry-config",
            &cfg,
        ],
    )
    .await
}

async fn create_aws_secrets(aws: &AwsContext) -> Result<()> {
    // Both secrets are applied as manifests on stdin: idempotent on rerun,
    // and no secret material ever appears on argv or in error messages.

    // Basic-auth secret consumed by Flux's source-controller to clone the repo.
    println!(">>> Creating GitHub PAT credentials secret in flux-system...");
    kubectl_apply(&json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": "flux-github-pat", "namespace": "flux-system" },
        "type": "Opaque",
        "stringData": {
            "username": aws.github_user,
            "password": aws.github_token,
        },
    }))
    .await?;

    // Flux's kustomize-controller uses this key to decrypt *.sops.yaml
    // manifests during reconciliation. Flux scans the Secret for keys matching
    // `keys.<public-key>.agekey`.
    println!(">>> Creating sops-age decryption secret in flux-system...");
    // Remove any existing sops-age secret to avoid stale keys from previous
    // bootstrap runs (apply alone would merge old key entries).
    run(
        "kubectl",
        &[
            "delete",
            "secret",
            "sops-age",
            "-n",
            "flux-system",
            "--ignore-not-found",
        ],
    )
    .await?;
    kubectl_apply(&json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": { "name": "sops-age", "namespace": "flux-system" },
        "type": "Opaque",
        "stringData": {
            format!("keys.{}.agekey", aws.age_pubkey): aws.age_key_content,
        },
    }))
    .await
}

async fn install_flux_instance(
    cli: &Cli,
    aws: Option<&AwsContext>,
    registry_config: &Path,
) -> Result<()> {
    println!(">>> Installing FluxInstance via Helm...");
    let cfg = registry_config.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec![
        "upgrade".into(),
        "--install".into(),
        "flux".into(),
        "oci://ghcr.io/controlplaneio-fluxcd/charts/flux-instance".into(),
        "--namespace".into(),
        "flux-system".into(),
        // Helm 4's watcher strategy treats the FluxInstance's transient
        // InProgress condition as a terminal failure. Use the legacy
        // chart-resource wait here, then wait explicitly for the
        // operator-owned Ready condition below.
        "--wait=legacy".into(),
        "--timeout".into(),
        "10m".into(),
        "--set".into(),
        "instance.cluster.type=kubernetes".into(),
        "--set".into(),
        "instance.cluster.size=small".into(),
        "--set".into(),
        "instance.cluster.multitenant=false".into(),
        "--set".into(),
        "instance.cluster.networkPolicy=true".into(),
        "--set".into(),
        "instance.cluster.domain=cluster.local".into(),
        "--registry-config".into(),
        cfg,
    ];

    match aws {
        Some(aws) => {
            args.extend([
                "--set".into(),
                "instance.sync.kind=GitRepository".into(),
                "--set".into(),
                format!("instance.sync.url={}", aws.git_repo_url),
                "--set".into(),
                "instance.sync.ref=refs/heads/main".into(),
                "--set".into(),
                "instance.sync.path=mgmt/aws".into(),
                "--set".into(),
                "instance.sync.pullSecret=flux-github-pat".into(),
            ]);
        }
        None => {
            args.extend([
                "--set".into(),
                "instance.sync.kind=OCIRepository".into(),
                "--set".into(),
                format!(
                    "instance.sync.url=oci://{REGISTRY_NAME}:5000/{}",
                    cli.oci_repository
                ),
                "--set".into(),
                format!("instance.sync.ref={}", cli.oci_tag),
                "--set".into(),
                "instance.sync.path=mgmt/local-host".into(),
                "--set-json".into(),
                r#"instance.kustomize.patches=[{"patch":"- op: add\n  path: /spec/insecure\n  value: true","target":{"kind":"OCIRepository"}}]"#.into(),
            ]);
        }
    }

    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run("helm", &arg_refs).await?;

    println!(">>> Waiting for FluxInstance reconciliation to complete...");
    run(
        "kubectl",
        &[
            "wait",
            "fluxinstance/flux",
            "--namespace",
            "flux-system",
            "--for=condition=Ready",
            "--timeout=10m",
        ],
    )
    .await?;

    // Verify the Flux controllers are running before declaring success.
    println!(">>> Waiting for Flux controllers to be ready...");
    let _ = run(
        "kubectl",
        &[
            "wait",
            "--namespace",
            "flux-system",
            "--for=condition=ready",
            "pod",
            "--selector=app.kubernetes.io/part-of=flux",
            "--timeout=90s",
        ],
    )
    .await; // `|| true` in the original script
    Ok(())
}

/// Poll `kubectl <args>` until it succeeds, up to `attempts` tries 2s apart.
async fn wait_for_resource(args: &[&str], attempts: u32) -> bool {
    for _ in 0..attempts {
        if run_quiet("kubectl", args).await {
            return true;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

async fn watch_local_reconciliation(cli: &Cli, engine: &str) -> Result<()> {
    println!();
    println!(">>> Step 5: Flux reconciliation progress");

    // The final Kustomization is created by the OCI root, so wait for it to
    // appear before asking kubectl to wait for readiness.
    if !wait_for_resource(
        &[
            "get",
            "kustomization",
            "flux-apps",
            "--namespace",
            "flux-system",
        ],
        60,
    )
    .await
    {
        eprintln!("ERROR: flux-apps Kustomization was not created within 2 minutes");
        let _ = run("flux", &["get", "kustomizations"]).await;
        bail!("flux-apps Kustomization missing");
    }

    println!(">>> Waiting until the local workload cluster and Flux addons are ready...");
    let timeout_arg = format!("--timeout={}", cli.local_reconcile_timeout);
    if run(
        "kubectl",
        &[
            "wait",
            "kustomization/flux-apps",
            "--namespace",
            "flux-system",
            "--for=condition=Ready",
            &timeout_arg,
        ],
    )
    .await
    .is_err()
    {
        eprintln!(
            "ERROR: local-host reconciliation did not complete within {}",
            cli.local_reconcile_timeout
        );
        let _ = run("flux", &["get", "kustomizations"]).await;
        bail!("local-host reconciliation timed out");
    }

    println!();
    println!(">>> Workload cluster Flux reconciliation errors");
    let workload_kubeconfig =
        tempfile::NamedTempFile::new().context("failed to create temp kubeconfig")?;
    let kubeconfig_path = workload_kubeconfig.path().to_string_lossy().into_owned();

    let kubeconfig_content =
        capture("clusterctl", &["get", "kubeconfig", "local-workload"]).await?;
    std::fs::write(workload_kubeconfig.path(), kubeconfig_content)?;

    let port_output = capture(engine, &["port", "local-workload-lb", "6443/tcp"]).await?;
    let Some(workload_port) = extract_workload_port(&port_output) else {
        bail!("cannot determine the local-workload API server port");
    };

    let server = format!("https://127.0.0.1:{workload_port}");
    let kubeconfig_flag = format!("--kubeconfig={kubeconfig_path}");
    capture(
        "kubectl",
        &[
            "config",
            "set-cluster",
            "local-workload",
            &format!("--server={server}"),
            &kubeconfig_flag,
        ],
    )
    .await?;

    if !wait_for_resource(
        &[
            "--kubeconfig",
            &kubeconfig_path,
            "get",
            "kustomization",
            "flux-system",
            "--namespace",
            "flux-system",
        ],
        60,
    )
    .await
    {
        eprintln!("ERROR: workload Flux Kustomization was not created within 2 minutes");
        let _ = run(
            "kubectl",
            &[
                "--kubeconfig",
                &kubeconfig_path,
                "get",
                "pods",
                "--namespace",
                "flux-system",
            ],
        )
        .await;
        bail!("workload Flux Kustomization missing");
    }

    println!(">>> Waiting for workload Flux controllers to be ready...");
    run(
        "kubectl",
        &[
            "--kubeconfig",
            &kubeconfig_path,
            "wait",
            "pod",
            "--namespace",
            "flux-system",
            "--selector=app.kubernetes.io/part-of=flux",
            "--for=condition=Ready",
            &timeout_arg,
        ],
    )
    .await?;

    // Stream workload Flux errors in the bootstrap terminal while we wait.
    let flux_logs = Command::new("flux")
        .args([
            "logs",
            "--kubeconfig",
            &kubeconfig_path,
            "--all-namespaces",
            "--follow",
            "--level=error",
            "--since=10m",
        ])
        .spawn()
        .context("failed to spawn 'flux logs'")?;
    let _log_guard = ChildGuard(flux_logs);

    if run(
        "kubectl",
        &[
            "--kubeconfig",
            &kubeconfig_path,
            "wait",
            "kustomization/flux-system",
            "--namespace",
            "flux-system",
            "--for=condition=Ready",
            &timeout_arg,
        ],
    )
    .await
    .is_err()
    {
        eprintln!(
            "ERROR: workload reconciliation did not complete within {}",
            cli.local_reconcile_timeout
        );
        let _ = run(
            "flux",
            &[
                "get",
                "kustomizations",
                "--kubeconfig",
                &kubeconfig_path,
                "--all-namespaces",
            ],
        )
        .await;
        bail!("workload reconciliation timed out");
    }
    // _log_guard drops here: the flux logs follower is killed and the temp
    // kubeconfig is removed when workload_kubeconfig drops.
    Ok(())
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let profile = cli.profile()?;
    let http = reqwest::Client::builder()
        .user_agent(concat!("knr-bootstrap/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(HTTP_CONNECT_TIMEOUT)
        .timeout(HTTP_REQUEST_TIMEOUT)
        .build()
        .context("failed to build HTTP client")?;

    let preflight = preflight_checks(&cli, &http).await?;
    println!(
        ">>> Using container engine: {} (socket: {})",
        preflight.engine, preflight.engine_sock
    );

    // Step 1: ensure the kind management cluster (reuse by default; --recreate replaces).
    ensure_kind_cluster(&cli, &preflight.engine_sock).await?;

    // Step 1.5: bootstrap the local container registry (local-host only).
    if profile == Profile::LocalHost {
        bootstrap_local_registry(&cli, &preflight.engine, &http).await?;
    }

    // Anonymous registry config shared by both helm installs; the temp file is
    // removed automatically when it drops at the end of main.
    let mut registry_config =
        tempfile::NamedTempFile::new().context("failed to create temp registry config")?;
    {
        use std::io::Write;
        writeln!(registry_config, "{{}}")?;
        registry_config.flush()?;
    }

    // Step 2: install the Flux Operator.
    install_flux_operator(registry_config.path()).await?;

    // Step 3: GitHub PAT + SOPS age secrets (aws only).
    if let Some(aws) = preflight.aws.as_ref() {
        create_aws_secrets(aws).await?;
    }

    // Step 4: install the FluxInstance via Helm.
    install_flux_instance(&cli, preflight.aws.as_ref(), registry_config.path()).await?;

    // Step 5: watch local-host reconciliation.
    if profile == Profile::LocalHost {
        watch_local_reconciliation(&cli, &preflight.engine).await?;
    }

    // Done. Everything else is driven by GitOps.
    println!();
    match profile {
        Profile::Aws => {
            let url = preflight
                .aws
                .as_ref()
                .map(|a| a.git_repo_url.as_str())
                .unwrap_or_default();
            println!(">>> Bootstrap complete! Flux is now reconciling from {url}");
            println!(">>> Watch progress with: flux get kustomizations --watch");
        }
        Profile::LocalHost => {
            println!(
                ">>> Local-host profile complete: Flux is reconciling from the local OCI artifact"
            );
            println!(
                ">>> Local registry: localhost:{port} (cluster endpoint: {REGISTRY_NAME}:5000)",
                port = cli.registry_port
            );
            println!(
                ">>> OCI source: oci://{REGISTRY_NAME}:5000/{repo}:{tag} (path: mgmt/local-host)",
                repo = cli.oci_repository,
                tag = cli.oci_tag
            );
            println!(">>> Watch progress with: flux get sources oci --watch");
            println!(">>> No AWS resources were provisioned");
        }
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    const VALID_AGE_KEY: &str = "# created: 2026-01-01T00:00:00+02:00\n# public key: age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq\nAGE-SECRET-KEY-1SECRETSECRETSECRET\n";

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn cli_rejects_unknown_profile() {
        assert!(Cli::try_parse_from(["knr-bootstrap", "bogus"]).is_err());
    }

    #[test]
    fn cli_defaults_match_script() {
        let cli = Cli::try_parse_from(["knr-bootstrap", "local-host"]).unwrap();
        assert_eq!(cli.profile, Some(Profile::LocalHost));
        assert!(!cli.recreate);
        assert_eq!(cli.registry_port, 5001);
        assert_eq!(cli.registry_ready_retries, 120);
        assert_eq!(cli.node_ready_timeout, "120s");
        assert_eq!(cli.local_reconcile_timeout, "15m");
        assert_eq!(cli.github_user, "git");
        assert_eq!(cli.age_key_file, PathBuf::from("age.agekey"));
        assert_eq!(cli.oci_repository, "knr-ops");
        assert_eq!(cli.oci_tag, "latest");
    }

    #[test]
    fn profile_resolution_matches_script_precedence() {
        use Profile::{Aws, LocalHost};
        // Non-empty env wins over the positional, like ${KNR_OPS_PROFILE:-${1:-aws}}.
        assert_eq!(resolve_profile(Some("aws"), Some(LocalHost)).unwrap(), Aws);
        assert_eq!(
            resolve_profile(Some("local-host"), Some(Aws)).unwrap(),
            LocalHost
        );
        // Empty env falls through to the positional, then aws.
        assert_eq!(
            resolve_profile(Some(""), Some(LocalHost)).unwrap(),
            LocalHost
        );
        assert_eq!(resolve_profile(Some(""), None).unwrap(), Aws);
        assert_eq!(resolve_profile(None, Some(Aws)).unwrap(), Aws);
        assert_eq!(resolve_profile(None, None).unwrap(), Aws);
        // Unknown env values fail with the script's error message.
        assert_eq!(
            resolve_profile(Some("bogus"), Some(LocalHost))
                .unwrap_err()
                .to_string(),
            "unsupported profile 'bogus' (expected 'local-host' or 'aws')"
        );
    }

    #[test]
    fn cli_accepts_recreate_flag() {
        let cli = Cli::try_parse_from(["knr-bootstrap", "aws", "--recreate"]).unwrap();
        assert!(cli.recreate);
    }

    #[test]
    fn parse_github_repo_accepts_https_urls() {
        for url in [
            "https://github.com/polarsquad/knr-ops",
            "https://github.com/polarsquad/knr-ops/",
            "https://github.com/polarsquad/knr-ops.git",
        ] {
            assert_eq!(parse_github_repo(url), Some("polarsquad/knr-ops"), "{url}");
        }
    }

    #[test]
    fn parse_github_repo_rejects_bad_urls() {
        for url in [
            "git@github.com:polarsquad/knr-ops.git",
            "https://gitlab.com/polarsquad/knr-ops",
            "https://github.com/polarsquad",
            "https://github.com//knr-ops",
            "https://github.com/polarsquad/",
            "",
        ] {
            assert_eq!(parse_github_repo(url), None, "{url}");
        }
    }

    #[test]
    fn age_key_validation_passes_valid_file() {
        assert!(validate_age_key(VALID_AGE_KEY).is_empty());
        assert_eq!(
            extract_age_pubkey(VALID_AGE_KEY).as_deref(),
            Some("age1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq")
        );
    }

    #[test]
    fn age_key_validation_reports_all_missing_fields() {
        let missing = validate_age_key("garbage\n");
        assert_eq!(
            missing,
            vec![
                "# created: header",
                "# public key: comment",
                "AGE-SECRET-KEY- line"
            ]
        );
        let missing = validate_age_key("# created: now\nAGE-SECRET-KEY-1X\n");
        assert_eq!(missing, vec!["# public key: comment"]);
    }

    #[test]
    fn extract_workload_port_parses_engine_output() {
        assert_eq!(
            extract_workload_port("0.0.0.0:32771\n[::]:32771\n").as_deref(),
            Some("32771")
        );
        assert_eq!(
            extract_workload_port("127.0.0.1:6443\n").as_deref(),
            Some("6443")
        );
        assert_eq!(extract_workload_port(""), None);
        assert_eq!(extract_workload_port("garbage\n"), None);
        assert_eq!(extract_workload_port("0.0.0.0:\n"), None);
    }

    #[test]
    fn kind_config_includes_registry_patch_for_local_host_only() {
        let local = render_kind_config(Profile::LocalHost, 5001, "/var/run/docker.sock");
        assert!(local.contains("containerdConfigPatches"));
        assert!(local.contains("localhost:5001"));
        assert!(local.contains("http://knr-registry:5000"));
        assert!(local.contains("hostPath: /var/run/docker.sock"));

        let aws = render_kind_config(Profile::Aws, 5001, "/var/run/docker.sock");
        assert!(!aws.contains("containerdConfigPatches"));
        assert!(aws.contains("kind: Cluster"));
        assert!(aws.contains("role: control-plane"));
    }

    #[test]
    fn required_tools_cover_every_invoked_binary() {
        let aws = required_tools(Profile::Aws);
        assert_eq!(aws, vec!["kind", "helm", "kubectl"]);
        let local = required_tools(Profile::LocalHost);
        assert_eq!(
            local,
            vec![
                "kind",
                "helm",
                "kubectl",
                "mise",
                "flux",
                "clusterctl",
                "curl"
            ]
        );
    }

    #[test]
    fn secret_manifests_keep_secrets_off_argv() {
        // The secret travels inside the JSON manifest (stdin), and the
        // manifest serializes without shell-visible arguments.
        let manifest = json!({
            "apiVersion": "v1",
            "kind": "Secret",
            "metadata": { "name": "flux-github-pat", "namespace": "flux-system" },
            "type": "Opaque",
            "stringData": { "username": "git", "password": "ghp_secret\"with'quotes" },
        });
        let rendered = manifest.to_string();
        assert!(rendered.contains("ghp_secret\\\"with'quotes"));
        // Round-trips as valid JSON despite embedded quotes.
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["stringData"]["password"], "ghp_secret\"with'quotes");
    }

    #[test]
    fn sops_age_secret_key_name_embeds_pubkey() {
        let pubkey = extract_age_pubkey(VALID_AGE_KEY).unwrap();
        let manifest = json!({
            "stringData": { format!("keys.{pubkey}.agekey"): VALID_AGE_KEY },
        });
        assert!(manifest["stringData"]
            .as_object()
            .unwrap()
            .contains_key(&format!("keys.{pubkey}.agekey")));
    }
}
