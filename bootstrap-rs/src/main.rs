//! knr-bootstrap – One-time imperative bootstrap for the management cluster.
//! Everything after this program runs is driven by GitOps (Flux).
//!
//! Rust port of `bootstrap.sh`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

const GIT_BRANCH: &str = "main";
const REGISTRY_NAME: &str = "knr-registry";

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Profile {
    #[value(name = "local-host")]
    LocalHost,
    #[value(name = "aws")]
    Aws,
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
#[derive(Parser, Debug)]
#[command(name = "knr-bootstrap", version, about)]
struct Cli {
    /// Deployment profile
    #[arg(value_enum, env = "KNR_OPS_PROFILE", default_value = "aws")]
    profile: Profile,

    /// Host port for the local container registry (local-host profile)
    #[arg(long, env = "REGISTRY_PORT", default_value_t = 5001)]
    registry_port: u16,

    /// Retries (1s apart) while waiting for the local registry API
    #[arg(long, env = "REGISTRY_READY_RETRIES", default_value_t = 120)]
    registry_ready_retries: u32,

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

    /// OCI repository name for the local-host artifact
    #[arg(long, env = "OCI_REPOSITORY", default_value = "knr-ops")]
    oci_repository: String,

    /// OCI tag for the local-host artifact
    #[arg(long, env = "OCI_TAG", default_value = "latest")]
    oci_tag: String,
}

// ── Process helpers ───────────────────────────────────────────────────────────

/// Is `cmd` an executable file somewhere on PATH?
fn command_exists(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(cmd);
        is_executable_file(&candidate)
    })
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
    age_key_file: PathBuf,
    age_pubkey: String,
}

struct Preflight {
    engine: String,
    engine_sock: String,
    aws: Option<AwsContext>,
}

async fn preflight_checks(cli: &Cli, http: &reqwest::Client) -> Result<Preflight> {
    for cmd in ["curl", "kind", "helm", "kubectl"] {
        if !command_exists(cmd) {
            bail!("{cmd} not found in PATH");
        }
    }

    if cli.profile == Profile::LocalHost && !command_exists("mise") {
        bail!("mise not found in PATH (required to publish the initial OCI artifact)");
    }

    let aws = if cli.profile == Profile::Aws {
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
                eprintln!(">>> WARNING: Could not detect the podman API socket path; assuming {sock}");
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
    let git_repo_url = cli.git_repo_url.clone().context("GIT_REPO_URL must be set")?;

    // GITHUB_USER is used in the Flux GitHub secret for repo clone authentication.
    let github_user = cli.github_user.clone();

    let repo = git_repo_url
        .strip_prefix("https://github.com/")
        .filter(|rest| rest.trim_end_matches('/').trim_end_matches(".git").contains('/'))
        .context("GIT_REPO_URL must be an HTTPS GitHub repository URL")?;
    let github_repo = repo.trim_end_matches('/').trim_end_matches(".git");

    let branch_path = GIT_BRANCH.replace('/', "%2F");
    let url =
        format!("https://api.github.com/repos/{github_repo}/branches/{branch_path}");
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

    let age_key_file = cli.age_key_file.clone();
    if !age_key_file.is_file() {
        bail!(
            "age key file not found at '{}'.\n       Generate one with:  mise run sops-keygen\n       and add its PUBLIC key to .sops.yaml. See docs/secrets.md.",
            age_key_file.display()
        );
    }

    // Validate age key file format first (before attempting to extract the
    // public key). This avoids silently proceeding with a malformed file.
    let age_content = std::fs::read_to_string(&age_key_file)
        .with_context(|| format!("failed to read '{}'", age_key_file.display()))?;
    let mut missing_fields: Vec<&str> = Vec::new();
    if !age_content.lines().any(|l| l.starts_with("# created:")) {
        missing_fields.push("# created: header");
    }
    if !age_content.lines().any(|l| l.starts_with("# public key:")) {
        missing_fields.push("# public key: comment");
    }
    if !age_content.lines().any(|l| l.starts_with("AGE-SECRET-KEY-")) {
        missing_fields.push("AGE-SECRET-KEY- line");
    }
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
        .or_else(|| {
            age_content
                .lines()
                .find_map(|l| l.strip_prefix("# public key: "))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
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
        age_key_file,
        age_pubkey,
    })
}

// ── Steps ─────────────────────────────────────────────────────────────────────

async fn create_kind_cluster(cli: &Cli, engine_sock: &str) -> Result<()> {
    println!(">>> Creating kind cluster 'mgmt'...");

    // Check if the cluster already exists and delete it (idempotent).
    let clusters = capture_lossy("kind", &["get", "clusters"]).await;
    if clusters.lines().any(|l| l.trim() == "mgmt") {
        println!(">>> Cluster 'mgmt' already exists – recreating...");
        run("kind", &["delete", "cluster", "--name", "mgmt"]).await?;
    }

    // Mount the host's container engine socket into the kind node at the
    // standard Docker socket path so in-cluster components can reach a
    // Docker-compatible API whether the backend is Docker or Podman.
    let registry_patch = if cli.profile == Profile::LocalHost {
        format!(
            "containerdConfigPatches:\n  - |-\n    [plugins.\"io.containerd.grpc.v1.cri\".registry.mirrors.\"localhost:{port}\"]\n      endpoint = [\"http://{REGISTRY_NAME}:5000\"]\n",
            port = cli.registry_port
        )
    } else {
        String::new()
    };

    let kind_config = format!(
        "kind: Cluster\napiVersion: kind.x-k8s.io/v1alpha4\n{registry_patch}nodes:\n  - role: control-plane\n    extraMounts:\n      - hostPath: {engine_sock}\n        containerPath: /var/run/docker.sock\n"
    );

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
        &["wait", "--for=condition=Ready", "node", "--all", "--timeout=120s"],
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
        &["ps", "-a", "--filter", &name_filter, "--format", "{{.Names}}"],
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
                "run", "-d", "--name", REGISTRY_NAME, "--network", "kind", "-p", &publish,
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

    // Tell the cluster about the local registry.
    let registry_literal = format!("registry-url={REGISTRY_NAME}:5000");
    run(
        "kubectl",
        &[
            "create", "configmap", "local-registry-config",
            "--from-literal", &registry_literal,
            "--namespace", "kube-system",
        ],
    )
    .await?;

    println!(">>> Publishing initial OCI artifact from the local Git checkout...");
    run("mise", &["-E", "local-host", "run", "oci-push"]).await?;
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
    run(
        "helm",
        &[
            "install", "flux-operator",
            "oci://ghcr.io/controlplaneio-fluxcd/charts/flux-operator",
            "--namespace", "flux-system",
            "--create-namespace",
            "--wait",
            "--registry-config", &cfg,
        ],
    )
    .await
}

async fn create_aws_secrets(aws: &AwsContext) -> Result<()> {
    // Basic-auth secret consumed by Flux's source-controller to clone the repo.
    println!(">>> Creating GitHub PAT credentials secret in flux-system...");
    let username = format!("username={}", aws.github_user);
    let password = format!("password={}", aws.github_token);
    run(
        "kubectl",
        &[
            "create", "secret", "generic", "flux-github-pat",
            "--namespace", "flux-system",
            "--from-literal", &username,
            "--from-literal", &password,
        ],
    )
    .await?;

    // Flux's kustomize-controller uses this key to decrypt *.sops.yaml
    // manifests during reconciliation. Flux scans the Secret for keys matching
    // `keys.<public-key>.agekey`.
    println!(">>> Creating sops-age decryption secret in flux-system...");
    // Remove any existing sops-age secret to avoid stale keys from previous runs.
    run(
        "kubectl",
        &[
            "delete", "secret", "sops-age", "-n", "flux-system", "--ignore-not-found",
        ],
    )
    .await?;
    let from_file = format!(
        "keys.{}.agekey={}",
        aws.age_pubkey,
        aws.age_key_file.display()
    );
    let manifest = capture(
        "kubectl",
        &[
            "create", "secret", "generic", "sops-age",
            "--namespace", "flux-system",
            "--from-file", &from_file,
            "--dry-run=client", "-o", "yaml",
        ],
    )
    .await?;
    run_with_stdin("kubectl", &["apply", "-f", "-"], &manifest).await
}

async fn install_flux_instance(
    cli: &Cli,
    aws: Option<&AwsContext>,
    registry_config: &Path,
) -> Result<()> {
    println!(">>> Installing FluxInstance via Helm...");
    let cfg = registry_config.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec![
        "upgrade".into(), "--install".into(), "flux".into(),
        "oci://ghcr.io/controlplaneio-fluxcd/charts/flux-instance".into(),
        "--namespace".into(), "flux-system".into(),
        // Helm 4's watcher strategy treats the FluxInstance's transient
        // InProgress condition as a terminal failure. Use the legacy
        // chart-resource wait here, then wait explicitly for the
        // operator-owned Ready condition below.
        "--wait=legacy".into(),
        "--timeout".into(), "10m".into(),
        "--set".into(), "instance.cluster.type=kubernetes".into(),
        "--set".into(), "instance.cluster.size=small".into(),
        "--set".into(), "instance.cluster.multitenant=false".into(),
        "--set".into(), "instance.cluster.networkPolicy=true".into(),
        "--set".into(), "instance.cluster.domain=cluster.local".into(),
        "--registry-config".into(), cfg,
    ];

    match aws {
        Some(aws) => {
            args.extend([
                "--set".into(), "instance.sync.kind=GitRepository".into(),
                "--set".into(), format!("instance.sync.url={}", aws.git_repo_url),
                "--set".into(), "instance.sync.ref=refs/heads/main".into(),
                "--set".into(), "instance.sync.path=mgmt/aws".into(),
                "--set".into(), "instance.sync.pullSecret=flux-github-pat".into(),
            ]);
        }
        None => {
            args.extend([
                "--set".into(), "instance.sync.kind=OCIRepository".into(),
                "--set".into(),
                format!(
                    "instance.sync.url=oci://{REGISTRY_NAME}:5000/{}",
                    cli.oci_repository
                ),
                "--set".into(), format!("instance.sync.ref={}", cli.oci_tag),
                "--set".into(), "instance.sync.path=mgmt/local-host".into(),
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
            "wait", "fluxinstance/flux",
            "--namespace", "flux-system",
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
            "wait", "--namespace", "flux-system", "--for=condition=ready", "pod",
            "--selector=app.kubernetes.io/part-of=flux",
            "--timeout=90s",
        ],
    )
    .await; // `|| true` in the original script
    Ok(())
}

/// Poll `kubectl get <args>` until it succeeds, up to `attempts` tries 2s apart.
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
            "get", "kustomization", "flux-apps", "--namespace", "flux-system",
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
            "wait", "kustomization/flux-apps",
            "--namespace", "flux-system",
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
    let workload_kubeconfig = tempfile::NamedTempFile::new()
        .context("failed to create temp kubeconfig")?;
    let kubeconfig_path = workload_kubeconfig.path().to_string_lossy().into_owned();

    let kubeconfig_content =
        capture("clusterctl", &["get", "kubeconfig", "local-workload"]).await?;
    std::fs::write(workload_kubeconfig.path(), kubeconfig_content)?;

    let port_output = capture(engine, &["port", "local-workload-lb", "6443/tcp"]).await?;
    let workload_port = port_output
        .lines()
        .next()
        .and_then(|l| l.rsplit(':').next())
        .map(str::trim)
        .filter(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
        .map(str::to_string);
    let Some(workload_port) = workload_port else {
        bail!("cannot determine the local-workload API server port");
    };

    let server = format!("https://127.0.0.1:{workload_port}");
    let kubeconfig_flag = format!("--kubeconfig={kubeconfig_path}");
    capture(
        "kubectl",
        &[
            "config", "set-cluster", "local-workload",
            &format!("--server={server}"),
            &kubeconfig_flag,
        ],
    )
    .await?;

    if !wait_for_resource(
        &[
            "--kubeconfig", &kubeconfig_path,
            "get", "kustomization", "flux-system",
            "--namespace", "flux-system",
        ],
        60,
    )
    .await
    {
        eprintln!("ERROR: workload Flux Kustomization was not created within 2 minutes");
        let _ = run(
            "kubectl",
            &[
                "--kubeconfig", &kubeconfig_path,
                "get", "pods", "--namespace", "flux-system",
            ],
        )
        .await;
        bail!("workload Flux Kustomization missing");
    }

    println!(">>> Waiting for workload Flux controllers to be ready...");
    run(
        "kubectl",
        &[
            "--kubeconfig", &kubeconfig_path,
            "wait", "pod",
            "--namespace", "flux-system",
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
            "--kubeconfig", &kubeconfig_path,
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
            "--kubeconfig", &kubeconfig_path,
            "wait", "kustomization/flux-system",
            "--namespace", "flux-system",
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
                "get", "kustomizations",
                "--kubeconfig", &kubeconfig_path,
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
    let http = reqwest::Client::builder()
        .user_agent(concat!("knr-bootstrap/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build HTTP client")?;

    let preflight = preflight_checks(&cli, &http).await?;
    println!(
        ">>> Using container engine: {} (socket: {})",
        preflight.engine, preflight.engine_sock
    );

    // Step 1: create the kind management cluster.
    create_kind_cluster(&cli, &preflight.engine_sock).await?;

    // Step 1.5: bootstrap the local container registry (local-host only).
    if cli.profile == Profile::LocalHost {
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
    if cli.profile == Profile::LocalHost {
        watch_local_reconciliation(&cli, &preflight.engine).await?;
    }

    // Done. Everything else is driven by GitOps.
    println!();
    match cli.profile {
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
            println!(">>> Local-host profile complete: Flux is reconciling from the local OCI artifact");
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
