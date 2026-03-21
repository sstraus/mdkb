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
        }
    }
}

impl DaemonConfig {
    /// Load config from a TOML file, or return default if the file doesn't exist.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path).map_err(|e| {
                Error::other(format!("Failed to read daemon config: {e}"))
            })?;
            toml::from_str(&content).map_err(|e| {
                Error::config(format!("Failed to parse daemon config: {e}"))
            })
        } else {
            Ok(Self::default())
        }
    }

    /// Save config to a TOML file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::other(format!("Failed to create daemon config dir: {e}"))
            })?;
        }
        let content = toml::to_string_pretty(self).map_err(|e| {
            Error::config(format!("Failed to serialize daemon config: {e}"))
        })?;
        std::fs::write(path, content).map_err(|e| {
            Error::other(format!("Failed to write daemon config: {e}"))
        })
    }

    /// Resolve the daemon home directory (~/.mdkb/).
    pub fn daemon_home() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".mdkb")
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
    /// Returns Ok(()) if allowed, or an error with a model-friendly message.
    pub fn check_whitelist(&self, path: &Path) -> Result<()> {
        if self.whitelist_dirs.is_empty() {
            return Ok(());
        }

        let canonical = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };

        for dir in &self.whitelist_dirs {
            let whitelist_path = expand_tilde(dir);
            let canonical_whitelist = match whitelist_path.canonicalize() {
                Ok(p) => p,
                Err(_) => whitelist_path,
            };
            if canonical.starts_with(&canonical_whitelist) {
                return Ok(());
            }
        }

        Err(Error::config(format!(
            "Repo at {} is not in the daemon whitelist. \
             Add one of its parent directories to whitelist_dirs in ~/.mdkb/daemon.toml. \
             Current whitelist: [{}]",
            path.display(),
            self.whitelist_dirs.join(", ")
        )))
    }
}

/// Expand ~ at the start of a path to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(rest)
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
                RepoEntry { root: "/Users/me/Gits/projectA".to_string() },
                RepoEntry { root: "/Users/me/Gits/projectB".to_string() },
            ],
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: DaemonConfig = toml::from_str(&toml_str).unwrap();

        assert_eq!(parsed.max_active_repos, 10);
        assert_eq!(parsed.socket_path.as_deref(), Some("~/.mdkb/daemon.sock"));
        assert_eq!(parsed.whitelist_dirs.len(), 2);
        assert_eq!(parsed.repos.len(), 2);
        assert_eq!(parsed.repos[0].root, "/Users/me/Gits/projectA");
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
            repos: vec![RepoEntry { root: "/foo/bar".to_string() }],
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
    fn test_daemon_whitelist_empty_allows_all() {
        let config = DaemonConfig::default();
        assert!(config.check_whitelist(Path::new("/any/path")).is_ok());
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
        assert!(err_msg.contains("not in the daemon whitelist"), "Error: {err_msg}");
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
}
