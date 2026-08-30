//! Repository-owned bootstrap configuration (`bootstrap.toml`, issue #98).
//!
//! Everything knr-ops-specific the binary needs is declared in this file;
//! the binary itself is a generic bootstrap engine. The environment knobs
//! (`Config::from_env`, see main.rs) keep their precedence over these
//! values at runtime. The file is located via `BOOTSTRAP_CONFIG` or
//! defaults to `./bootstrap.toml` (per the #98 decisions).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use indexmap::IndexMap;
use serde::Deserialize;

/// Root of the parsed `bootstrap.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapConfig {
    pub bootstrap: BootstrapSection,
    pub charts: IndexMap<String, String>,
    /// Environments keyed by section name, in file order (error messages
    /// list environments in file order, e.g. 'local-host' before 'aws').
    pub environments: IndexMap<String, Environment>,
}

/// Global names carried in `[bootstrap]`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct BootstrapSection {
    pub default_environment: String,
    pub git_branch: String,
    pub kind_cluster: String,
    pub kind_context: String,
    pub registry_name: String,
    pub flux_namespace: String,
    pub github_pat_secret: String,
    pub sops_age_secret: String,
    pub mgmt_namespace: String,
    pub mgmt_context: String,
}

/// One `[environments.<name>]` section.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Environment {
    pub kind: String,
    pub sync_path: String,
    pub mgmt_cluster: String,
    pub mgmt_ready_timeout: String,
    pub infra_provider_namespace: String,
    pub infra_provider_name: String,
    pub provider_manifests: Vec<String>,
    #[serde(default)]
    pub move_fallbacks: Vec<MoveFallback>,
}

/// One `[[environments.<name>.move-fallbacks]]` entry.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveFallback {
    pub resource: String,
    pub name: String,
    pub manifest: String,
}

impl BootstrapConfig {
    /// Locate and parse the repository config file. `BOOTSTRAP_CONFIG`
    /// overrides the default `./bootstrap.toml` location.
    pub fn locate_and_load() -> Result<Self> {
        let path = Self::locate();
        Self::load_from(&path)
    }

    /// The config path: `BOOTSTRAP_CONFIG` when set (non-empty), else
    /// `./bootstrap.toml` in the working directory.
    fn locate() -> PathBuf {
        std::env::var_os("BOOTSTRAP_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("bootstrap.toml"))
    }

    /// Parse a config file from disk, with validation beyond deserialization.
    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read bootstrap config '{path:?}'"))?;
        let config: BootstrapConfig = toml::from_str(&raw)
            .with_context(|| format!("failed to parse bootstrap config '{path:?}'"))?;
        config.validate()?;
        Ok(config)
    }

    /// Cross-field validation done after parsing (the Python cross-check
    /// covers repo-side consistency; this covers config-internal rules).
    fn validate(&self) -> Result<()> {
        if self.environments.is_empty() {
            bail!("no [environments.*] sections in bootstrap config");
        }
        for (name, env) in &self.environments {
            if env.kind != name.as_str() {
                bail!(
                    "environments.{name} kind '{}' must match its section name",
                    env.kind
                );
            }
        }
        let default = &self.bootstrap.default_environment;
        if !self.environments.contains_key(default) {
            bail!("bootstrap.default-environment '{default}' is not an [environments.*] section");
        }
        Ok(())
    }

    /// Resolve an environment by name (KNR_OPS_PROFILE value or positional
    /// argument); the error lists the valid names in file order.
    pub fn environment(&self, name: &str) -> Result<&Environment> {
        self.environments
            .get(name)
            .with_context(|| self.unsupported_message(name))
    }

    /// The error message for an unknown environment name; the script's
    /// wording, listing valid names in file order.
    fn unsupported_message(&self, name: &str) -> String {
        let names: Vec<String> = self.environments.keys().map(|n| format!("'{n}'")).collect();
        format!(
            "unsupported profile '{name}' (expected {})",
            names.join(" or ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r##"
[bootstrap]
default-environment = "aws"
git-branch = "main"
kind-cluster = "mgmt"
kind-context = "kind-mgmt"
registry-name = "knr-registry"
flux-namespace = "flux-system"
github-pat-secret = "flux-github-pat"
sops-age-secret = "sops-age"
mgmt-namespace = "default"
mgmt-context = "knr-ops-mgmt"

[charts]
flux-operator = "0.58.0"

[environments.local-host]
kind = "local-host"
sync-path = "mgmt/local-host"
mgmt-cluster = "local-management"
mgmt-ready-timeout = "15m"
infra-provider-namespace = "capd-system"
infra-provider-name = "docker"
provider-manifests = []

[environments.aws]
kind = "aws"
sync-path = "mgmt/aws"
mgmt-cluster = "eu-north-1-management"
mgmt-ready-timeout = "40m"
infra-provider-namespace = "capa-system"
infra-provider-name = "aws"
provider-manifests = []

[[environments.aws.move-fallbacks]]
resource = "awsclustercontrolleridentities.infrastructure.cluster.x-k8s.io"
name = "default"
manifest = "mgmt/aws/infrastructure/aws-identity/identity.yaml"
"##;

    fn parse(text: &str) -> Result<BootstrapConfig> {
        let config: BootstrapConfig = toml::from_str(text).unwrap();
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn parses_and_resolves_environments() {
        let config = parse(MINIMAL).unwrap();
        assert_eq!(config.bootstrap.git_branch, "main");
        assert_eq!(config.bootstrap.default_environment, "aws");
        assert_eq!(config.charts["flux-operator"], "0.58.0");

        let aws = config.environment("aws").unwrap();
        assert_eq!(aws.mgmt_cluster, "eu-north-1-management");
        assert_eq!(aws.move_fallbacks.len(), 1);
        assert_eq!(aws.move_fallbacks[0].name, "default");

        let local = config.environment("local-host").unwrap();
        assert!(local.move_fallbacks.is_empty());
        assert_eq!(
            (
                local.infra_provider_namespace.as_str(),
                local.infra_provider_name.as_str()
            ),
            ("capd-system", "docker")
        );
    }

    #[test]
    fn environment_order_preserved_from_file() {
        // The PR 1 contract: error messages list environments in file order
        // ('local-host' before 'aws'), matching bootstrap.sh's wording.
        let config = parse(MINIMAL).unwrap();
        let msg = config.unsupported_message("bogus");
        assert_eq!(
            msg,
            "unsupported profile 'bogus' (expected 'local-host' or 'aws')"
        );
    }

    #[test]
    fn unknown_environment_error_lists_file_order() {
        let config = parse(MINIMAL).unwrap();
        let err = config.environment("talos-local").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported profile 'talos-local' (expected 'local-host' or 'aws')"
        );
    }

    #[test]
    fn rejects_unknown_field() {
        let bad = MINIMAL.replace("git-branch", "git_branch_typo");
        assert!(toml::from_str::<BootstrapConfig>(&bad).is_err());
    }

    #[test]
    fn rejects_kind_section_mismatch() {
        let bad = MINIMAL.replace(
            "[environments.aws]\nkind = \"aws\"",
            "[environments.aws]\nkind = \"local-host\"",
        );
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn rejects_unknown_default_environment() {
        let bad = MINIMAL.replace(
            "default-environment = \"aws\"",
            "default-environment = \"gcp\"",
        );
        assert!(parse(&bad).is_err());
    }

    #[test]
    fn rejects_empty_environments() {
        let bad = MINIMAL.split("[environments.local-host]").next().unwrap();
        // Deserialization itself rejects a missing environments table.
        assert!(toml::from_str::<BootstrapConfig>(bad).is_err());
        // An explicitly empty one is caught by validate().
        let empty = format!("{bad}[environments]\n");
        let config: BootstrapConfig = toml::from_str(&empty).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn loads_the_repository_config() {
        // The checked-in file must parse and validate as shipped.
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let config = BootstrapConfig::load_from(&root.join("../bootstrap.toml")).unwrap();
        assert!(config.environments.contains_key("aws"));
        assert!(config.environments.contains_key("local-host"));
        assert!(config.charts.len() >= 3);
        let aws = config.environment("aws").unwrap();
        assert_eq!(aws.provider_manifests.len(), 6);
        assert_eq!(aws.move_fallbacks.len(), 1);
    }

    #[test]
    fn missing_config_file_is_a_clean_error() {
        let err = BootstrapConfig::load_from(Path::new("/nonexistent/bootstrap.toml")).unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }
}
