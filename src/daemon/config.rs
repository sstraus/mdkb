//! Daemon configuration for ~/.mdkb/daemon.toml.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Default socket path relative to daemon home.
const DEFAULT_SOCKET_NAME: &str = "daemon.sock";

/// Default PID file name.
const DEFAULT_PID_NAME: &str = "daemon.pid";

/// Default maximum number of concurrently active repo handles.
const DEFAULT_MAX_ACTIVE_REPOS: usize = 5;

/// Resolve the current user's home directory via [`directories::BaseDirs`].
///
/// Returns `Ok(PathBuf)` on success. Returns an error with a clear message
/// when the home directory cannot be resolved (e.g. HOME unset or empty),
/// so callers fail fast instead of silently producing paths relative to CWD.
pub fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .ok_or_else(|| Error::other("Cannot resolve home directory: HOME is unset or empty"))
}

/// Daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DaemonConfig {
    /// Unix socket path. Default: ~/.mdkb/daemon.sock
    pub socket_path: Option<String>,

    /// Maximum concurrently active repo handles (LRU eviction beyond this).
    pub max_active_repos: usize,

    /// Directories under which lazy registration is allowed.
    /// Repos outside these dirs are rejected with a clear error.
    pub whitelist_dirs: Vec<String>,

    /// Pre-registered repositories.
    #[serde(default)]
    pub repos: Vec<RepoEntry>,

    /// Global `[priors]` layer applied as the base for every repo. The distiller
    /// (program/args/model) is a machine-wide choice, so it belongs here — set it
    /// once instead of per-repo. A repo's `.mdkb/config.toml` `[priors]` overrides
    /// this field-by-field. Kept as a raw table so a repo can override individual
    /// keys without restating the whole section.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub priors: toml::Table,
}

/// A pre-registered repository entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoEntry {
    /// Absolute path to the repository root.
    pub root: String,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: None,
            max_active_repos: DEFAULT_MAX_ACTIVE_REPOS,
            whitelist_dirs: Vec::new(),
            repos: Vec::new(),
            priors: toml::Table::new(),
        }
    }
}

impl DaemonConfig {
    /// Load config from a TOML file, or return default if the file doesn't exist.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| Error::other(format!("Failed to read daemon config: {e}")))?;
            toml::from_str(&content)
                .map_err(|e| Error::config(format!("Failed to parse daemon config: {e}")))
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to a TOML file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::other(format!("Failed to create daemon config dir: {e}")))?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| Error::config(format!("Failed to serialize daemon config: {e}")))?;
        std::fs::write(path, content)
            .map_err(|e| Error::other(format!("Failed to write daemon config: {e}")))
    }

    /// Resolve the daemon home directory (~/.mdkb/).
    ///
    /// Falls back to `/tmp/.mdkb` only when the home directory is genuinely
    /// unresolvable (e.g. in a containerised environment without a home).
    pub fn daemon_home() -> PathBuf {
        home_dir()
            .map(|h| h.join(".mdkb"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/.mdkb"))
    }

    /// Resolve the config file path.
    pub fn config_path() -> PathBuf {
        Self::daemon_home().join("daemon.toml")
    }

    /// Resolve the socket path.
    pub fn socket_path(&self) -> PathBuf {
        match &self.socket_path {
            Some(p) => expand_tilde(p),
            None => Self::daemon_home().join(DEFAULT_SOCKET_NAME),
        }
    }

    /// Resolve the PID file path.
    pub fn pid_path(&self) -> PathBuf {
        Self::daemon_home().join(DEFAULT_PID_NAME)
    }

    /// Check if a path is under one of the whitelisted directories.
    ///
    /// Both the candidate and whitelist paths are canonicalized before comparison
    /// to handle symlinks, trailing slashes, and ~ expansion.
    ///
    /// Default-deny (SEC-3): an empty `whitelist_dirs` confines the daemon to the
    /// user's home directory rather than allowing any path on the system. In
    /// `--global` mode the daemon auto-creates `.mdkb/` (DB + config) and spawns a
    /// file watcher at any allowed root, so allow-all let a client point the daemon
    /// at arbitrary directories. Explicit `whitelist_dirs` override the default.
    /// This path is only reached in daemon/global mode; single-repo local usage
    /// opens its own `Context` and never consults the whitelist.
    ///
    /// Returns Ok(()) if allowed, or an error with a model-friendly message.
    pub fn check_whitelist(&self, path: &Path) -> Result<()> {
        let effective: Vec<PathBuf> = if self.whitelist_dirs.is_empty() {
            match home_dir() {
                Ok(home) => vec![home],
                Err(_) => {
                    return Err(Error::config(
                        "Daemon root whitelist is empty and the home directory could not be \
                         determined; refusing to open an arbitrary root. Set whitelist_dirs in \
                         ~/.mdkb/daemon.toml.",
                    ));
                }
            }
        } else {
            self.whitelist_dirs
                .iter()
                .map(|d| expand_tilde(d))
                .collect()
        };

        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };

        for whitelist_path in &effective {
            let canonical_whitelist = match whitelist_path.canonicalize() {
                Ok(p) => p,
                Err(_) => whitelist_path.clone(),
            };
            if canonical.starts_with(&canonical_whitelist) {
                return Ok(());
            }
        }

        let shown = if self.whitelist_dirs.is_empty() {
            "<home> (default-deny; set whitelist_dirs to widen)".to_string()
        } else {
            self.whitelist_dirs.join(", ")
        };
        Err(Error::config(format!(
            "Repo at {} is not in the daemon whitelist. \
             Add one of its parent directories to whitelist_dirs in ~/.mdkb/daemon.toml. \
             Current whitelist: [{shown}]",
            path.display(),
        )))
    }
}

/// Expand ~ at the start of a path to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|_| PathBuf::from("/tmp").join(rest))
    } else {
        PathBuf::from(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_daemon_config() {
        let config = DaemonConfig::default();
        assert_eq!(config.max_active_repos, 5);
        assert!(config.socket_path.is_none());
        assert!(config.whitelist_dirs.is_empty());
        assert!(config.repos.is_empty());
    }

    #[test]
    fn test_daemon_config_serialization_roundtrip() {
        let config = DaemonConfig {
            socket_path: Some("~/.mdkb/daemon.sock".to_string()),
            max_active_repos: 10,
            whitelist_dirs: vec!["~/Gits".to_string(), "~/Projects".to_string()],
            repos: vec![
                RepoEntry {
                    root: "/Users/me/Gits/projectA".to_string(),
                },
                RepoEntry {
                    root: "/Users/me/Gits/projectB".to_string(),
                },
            ],
            priors: toml::from_str("mining_enabled = true\ndistiller_program = \"codex\"").unwrap(),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: DaemonConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.max_active_repos, 10);
        assert_eq!(parsed.socket_path.as_deref(), Some("~/.mdkb/daemon.sock"));
        assert_eq!(parsed.whitelist_dirs.len(), 2);
        assert_eq!(parsed.repos.len(), 2);
        assert_eq!(parsed.repos[0].root, "/Users/me/Gits/projectA");
        // The global priors table survives a serialize→parse roundtrip (proves it
        // is emitted in a valid position relative to the array-of-tables `repos`).
        assert_eq!(
            parsed.priors.get("distiller_program"),
            Some(&toml::Value::String("codex".to_string()))
        );
    }

    #[test]
    fn test_daemon_config_deserialization_minimal() {
        let toml_str = r#"
max_active_repos = 3
whitelist_dirs = ["~/Code"]
"#;
        let config: DaemonConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_active_repos, 3);
        assert_eq!(config.whitelist_dirs, vec!["~/Code"]);
        assert!(config.repos.is_empty());
        assert!(config.socket_path.is_none());
    }

    #[test]
    fn test_daemon_config_deserialization_empty() {
        let config: DaemonConfig = toml::from_str("").unwrap();
        assert_eq!(config.max_active_repos, DEFAULT_MAX_ACTIVE_REPOS);
        assert!(config.whitelist_dirs.is_empty());
    }

    #[test]
    fn test_daemon_config_load_missing_file() {
        let config = DaemonConfig::load_or_default(Path::new("/nonexistent/daemon.toml")).unwrap();
        assert_eq!(config.max_active_repos, DEFAULT_MAX_ACTIVE_REPOS);
    }

    #[test]
    fn test_daemon_config_save_and_load() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.toml");

        let config = DaemonConfig {
            socket_path: None,
            max_active_repos: 7,
            whitelist_dirs: vec!["~/Gits".to_string()],
            repos: vec![RepoEntry {
                root: "/foo/bar".to_string(),
            }],
            priors: toml::Table::new(),
        };
        config.save(&path).unwrap();

        let loaded = DaemonConfig::load_or_default(&path).unwrap();
        assert_eq!(loaded.max_active_repos, 7);
        assert_eq!(loaded.whitelist_dirs, vec!["~/Gits"]);
        assert_eq!(loaded.repos.len(), 1);
    }

    #[test]
    fn test_daemon_socket_path_default() {
        let config = DaemonConfig::default();
        let path = config.socket_path();
        assert!(path.to_string_lossy().ends_with(".mdkb/daemon.sock"));
    }

    #[test]
    fn test_daemon_socket_path_custom() {
        let config = DaemonConfig {
            socket_path: Some("~/custom/mdkb.sock".to_string()),
            ..Default::default()
        };
        let path = config.socket_path();
        assert!(path.to_string_lossy().ends_with("custom/mdkb.sock"));
    }

    #[test]
    fn test_daemon_pid_path() {
        let config = DaemonConfig::default();
        let path = config.pid_path();
        assert!(path.to_string_lossy().ends_with(".mdkb/daemon.pid"));
    }

    #[test]
    fn test_daemon_whitelist_empty_defaults_to_home() {
        // SEC-3: an empty whitelist is default-deny (confined to home), NOT
        // allow-all. A path under home is permitted; arbitrary paths are not.
        let config = DaemonConfig::default();
        let home = home_dir().expect("home dir");
        assert!(config.check_whitelist(&home).is_ok());
        assert!(config.check_whitelist(Path::new("/any/path")).is_err());
    }

    #[test]
    fn test_daemon_whitelist_allows_subdir() {
        let tmp = TempDir::new().unwrap();
        let subdir = tmp.path().join("project");
        std::fs::create_dir_all(&subdir).unwrap();

        let config = DaemonConfig {
            whitelist_dirs: vec![tmp.path().to_string_lossy().to_string()],
            ..Default::default()
        };
        assert!(config.check_whitelist(&subdir).is_ok());
    }

    #[test]
    fn test_daemon_whitelist_rejects_outside() {
        let tmp = TempDir::new().unwrap();
        let config = DaemonConfig {
            whitelist_dirs: vec![tmp.path().join("allowed").to_string_lossy().to_string()],
            ..Default::default()
        };

        let result = config.check_whitelist(Path::new("/somewhere/else"));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not in the daemon whitelist"),
            "Error: {err_msg}"
        );
        assert!(err_msg.contains("whitelist_dirs"), "Error: {err_msg}");
    }

    #[test]
    fn test_daemon_whitelist_error_model_friendly() {
        let config = DaemonConfig {
            whitelist_dirs: vec!["~/Gits".to_string(), "~/Projects".to_string()],
            ..Default::default()
        };

        let result = config.check_whitelist(Path::new("/outside/repo"));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("Add one of its parent directories"));
        assert!(err_msg.contains("~/Gits, ~/Projects"));
    }

    #[test]
    fn test_expand_tilde_expands() {
        let expanded = expand_tilde("~/foo/bar");
        assert!(!expanded.to_string_lossy().starts_with('~'));
        assert!(expanded.to_string_lossy().ends_with("foo/bar"));
    }

    #[test]
    fn test_expand_tilde_absolute_unchanged() {
        let expanded = expand_tilde("/absolute/path");
        assert_eq!(expanded, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn test_home_dir_returns_ok() {
        // In normal CI/dev environments HOME is always set, so this must succeed.
        let result = home_dir();
        assert!(result.is_ok(), "home_dir() failed: {:?}", result);
        let path = result.unwrap();
        assert!(
            path.is_absolute(),
            "home_dir() returned a relative path: {path:?}"
        );
    }
}
