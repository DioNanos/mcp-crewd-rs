//! Daemon configuration (SPEC §15, hardening §17.3). The config path is
//! supplied explicitly via `--config`; it is **never** derived from the current
//! working directory (SPEC §17.3: socket path not from cwd). All runtime paths
//! (socket, db, audit, tokens) live under `runtime_dir`.
//!
//! `quota` is wired in T10b once the quota module is integrated; it is omitted
//! here so this task does not depend on Task 6.
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CrewdConfig {
    pub runtime_dir: PathBuf,
    pub acl_path: PathBuf,
    pub lease_secs: u64,
    pub max_attempts: u32,
    pub backoff_base_secs: u64,
    pub backoff_cap_secs: u64,
    /// Path to the keys file (e.g. `~/.config/keys/ai.env`) forwarded to the
    /// engine adapters for Z.AI profiles (`zai-a`/`zai-p`). `None` → those
    /// profiles fail `E_ENGINE_DOWN`. Threaded into `EngineSpawnCfg` via the
    /// scheduler (`Scheduler::with_keys_env_path`).
    pub keys_env_path: Option<String>,
}

#[derive(Debug)]
pub enum ConfigError {
    Read(String),
    Parse(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Read(m) => write!(f, "config read: {m}"),
            ConfigError::Parse(m) => write!(f, "config parse: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl CrewdConfig {
    /// Socket = `runtime_dir/crewd.sock`.
    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join("crewd.sock")
    }
    /// DB = `runtime_dir/crewd.db`.
    pub fn db_path(&self) -> PathBuf {
        self.runtime_dir.join("crewd.db")
    }
    /// Audit chain = `runtime_dir/audit.jsonl`.
    pub fn audit_path(&self) -> PathBuf {
        self.runtime_dir.join("audit.jsonl")
    }
    /// Per-cell token dir = `runtime_dir/tokens/` (mode 0700).
    pub fn tokens_dir(&self) -> PathBuf {
        self.runtime_dir.join("tokens")
    }

    /// Parse `crewd.toml` from an explicit path (never cwd-derived).
    pub fn from_toml_file(config_path: &Path) -> Result<Self, ConfigError> {
        let text =
            std::fs::read_to_string(config_path).map_err(|e| ConfigError::Read(format!("{e}")))?;
        Self::from_toml(&text)
    }

    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        #[derive(serde::Deserialize)]
        struct Raw {
            runtime_dir: String,
            acl_path: String,
            #[serde(default = "default_lease")]
            lease_secs: u64,
            #[serde(default = "default_attempts")]
            max_attempts: u32,
            #[serde(default = "default_backoff_base")]
            backoff_base_secs: u64,
            #[serde(default = "default_backoff_cap")]
            backoff_cap_secs: u64,
            #[serde(default)]
            keys_env_path: Option<String>,
        }
        let r: Raw = toml::from_str(text).map_err(|e| ConfigError::Parse(format!("{e}")))?;
        Ok(CrewdConfig {
            runtime_dir: PathBuf::from(r.runtime_dir),
            acl_path: PathBuf::from(r.acl_path),
            lease_secs: r.lease_secs,
            max_attempts: r.max_attempts,
            backoff_base_secs: r.backoff_base_secs,
            backoff_cap_secs: r.backoff_cap_secs,
            keys_env_path: r.keys_env_path,
        })
    }
}

fn default_lease() -> u64 {
    30
}
fn default_attempts() -> u32 {
    10
}
fn default_backoff_base() -> u64 {
    1
}
fn default_backoff_cap() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_toml_parses_keys_env_path() {
        let toml = "\
runtime_dir = \"/tmp/rt\"
acl_path = \"/tmp/acl.toml\"
keys_env_path = \"/etc/crewd/keys.env\"
";
        let cfg = CrewdConfig::from_toml(toml).unwrap();
        assert_eq!(cfg.keys_env_path.as_deref(), Some("/etc/crewd/keys.env"));
    }

    #[test]
    fn from_toml_keys_env_path_defaults_to_none() {
        let toml = "\
runtime_dir = \"/tmp/rt\"
acl_path = \"/tmp/acl.toml\"
";
        let cfg = CrewdConfig::from_toml(toml).unwrap();
        assert_eq!(cfg.keys_env_path, None);
    }
}
