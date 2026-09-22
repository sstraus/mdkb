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

/// How long a nested-store discovery walk may be reused, in seconds.
///
/// The walk visits every directory under every known root and under the
/// workspace a client declared. That is bounded by the operator's choice of
/// root, not by mdkb: a normal repo is tens of directories, a container of
/// repositories measured 265,946 after the `.git`/`target`/`node_modules`
/// prunes — 12.7 s cold, and it was paid on every call.
///
/// A store the daemon itself registers drops the cache immediately, so this
/// bounds one thing only: how long a store created by ANOTHER process — an
/// `mdkb init` from the CLI, a `git clone` of a repo with a committed
/// `.mdkb/` — stays invisible. A minute of that, against one walk per minute
/// instead of one per call.
const DEFAULT_DISCOVERY_CACHE_SECS: u64 = 60;

/// Daemon-owned state: the set of repositories the daemon knows about.
const REPO_MAP_NAME: &str = "repos.json";

/// The directory a config file sits in, when it names one.
///
/// A bare file name has an empty parent, which would put daemon state in
/// whatever directory the process happens to be in; that counts as no
/// directory at all.
fn state_dir_of(config_path: &Path) -> Option<PathBuf> {
    config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Resolve the current user's home directory: `HOME`, then `USERPROFILE`, then
/// [`directories::BaseDirs`].
///
/// The environment comes first because `BaseDirs` on Windows calls
/// `SHGetKnownFolderPath(FOLDERID_Profile)` — a Win32 API that no variable can
/// override. Everything under `~/.mdkb` was therefore unaddressable there: an
/// operator could not point mdkb at another profile, and the test suite could
/// not isolate itself, so every spawned command wrote into the real user
/// profile while believing it was in a tempdir. `git::home_dir` already read
/// the environment in this order; the two now agree.
///
/// An empty value counts as absent so it cannot degrade into the filesystem
/// root. Returns an error rather than a CWD-relative path when nothing names a
/// home, so callers fail fast.
pub fn home_dir() -> Result<PathBuf> {
    named_home(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
    .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()))
    .ok_or_else(|| {
        Error::other("Cannot resolve home directory: HOME and USERPROFILE are unset or empty")
    })
}

/// The home directory the environment names, if either variable names one.
///
/// Split out from [`home_dir`] so the precedence can be tested without
/// `set_var`: `cargo test` runs tests as threads of one process, so mutating
/// the environment in a test corrupts whichever sibling reads it next. This
/// function is pure, and the platform fallback is the only untested line left.
/// An empty value is skipped rather than accepted, so `HOME=""` falls through
/// to `USERPROFILE` instead of resolving to the filesystem root.
fn named_home(
    home: Option<&std::ffi::OsStr>,
    user_profile: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    [home, user_profile]
        .into_iter()
        .flatten()
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
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

    /// Seconds a nested-store discovery walk may be reused.
    /// See [`DEFAULT_DISCOVERY_CACHE_SECS`]. `0` disables the cache.
    pub discovery_cache_secs: u64,

    /// Global `[priors]` layer applied as the base for every repo. The distiller
    /// (program/args/model) is a machine-wide choice, so it belongs here — set it
    /// once instead of per-repo. A repo's `.mdkb/config.toml` `[priors]` overrides
    /// this field-by-field. Kept as a raw table so a repo can override individual
    /// keys without restating the whole section.
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub priors: toml::Table,

    /// Directory holding daemon-owned state, next to the `daemon.toml` this was
    /// loaded from. Never read from or written to that file — it names where
    /// the file lives, so it cannot live inside it.
    ///
    /// `None` for a config built in memory: nothing on disk backs it, so
    /// nothing may be persisted on its behalf. That is what keeps a
    /// `DaemonConfig::default()` in a test from writing into the real
    /// `~/.mdkb`.
    #[serde(skip)]
    pub state_dir: Option<PathBuf>,
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
            discovery_cache_secs: DEFAULT_DISCOVERY_CACHE_SECS,
            priors: toml::Table::new(),
            state_dir: None,
        }
    }
}

impl DaemonConfig {
    /// Load config from a TOML file, or return default if the file doesn't exist.
    ///
    /// Either way the result carries the directory `path` sits in as its
    /// [`state_dir`](Self::state_dir): a daemon that has a config path has a
    /// home to keep its own state in, whether or not an operator has written a
    /// `daemon.toml` there yet.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        let mut config = if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| Error::other(format!("Failed to read daemon config: {e}")))?;
            toml::from_str(&content)
                .map_err(|e| Error::config(format!("Failed to parse daemon config: {e}")))?
        } else {
            Self::default()
        };
        config.state_dir = state_dir_of(path);
        Ok(config)
    }

    /// Where the persisted repo map lives, or `None` when no directory backs
    /// this config.
    pub fn repo_map_path(&self) -> Option<PathBuf> {
        self.state_dir.as_ref().map(|d| d.join(REPO_MAP_NAME))
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

    /// The point of the whole function: an operator, and this suite, must be
    /// able to say where home is. `BaseDirs` alone answers a Win32 API that
    /// takes no instruction.
    ///
    /// Tested through `named_home` rather than by setting the variables:
    /// `cargo test` runs tests as threads of ONE process, so a `set_var` here
    /// reaches whichever sibling reads the environment next. An earlier draft
    /// of this test did exactly that and broke two unrelated config tests.
    #[test]
    fn the_environment_names_home_before_the_platform_does() {
        let home = std::ffi::OsStr::new("/home/me");
        let profile = std::ffi::OsStr::new("C:/Users/me");

        assert_eq!(
            named_home(Some(home), Some(profile)),
            Some(PathBuf::from("/home/me")),
            "HOME wins when both are named"
        );
        assert_eq!(
            named_home(None, Some(profile)),
            Some(PathBuf::from("C:/Users/me")),
            "the Windows spelling is honoured everywhere, so the platforms \
             cannot drift into different resolution orders"
        );
        assert_eq!(named_home(None, None), None, "nothing named, nothing found");
    }

    /// An empty value must not resolve to the filesystem root, which is where
    /// `PathBuf::from("")` joined with `.mdkb` would put the store.
    #[test]
    fn an_empty_value_is_not_a_home() {
        let empty = std::ffi::OsStr::new("");
        let profile = std::ffi::OsStr::new("C:/Users/me");

        assert_eq!(named_home(Some(empty), Some(empty)), None);
        assert_eq!(
            named_home(Some(empty), Some(profile)),
            Some(PathBuf::from("C:/Users/me")),
            "an empty HOME falls through rather than winning"
        );
    }

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
            discovery_cache_secs: 90,
            priors: toml::from_str("mining_enabled = true\ndistiller_program = \"codex\"").unwrap(),
            state_dir: Some(PathBuf::from("/Users/me/.mdkb")),
        };

        let toml_str = toml::to_string_pretty(&config).unwrap();
        let parsed: DaemonConfig = toml::from_str(&toml_str).unwrap();

        // `state_dir` says where this file lives, so it never appears in it.
        assert!(
            !toml_str.contains("state_dir"),
            "runtime state leaked into the operator's config: {toml_str}"
        );
        assert_eq!(parsed.state_dir, None);
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
            discovery_cache_secs: DEFAULT_DISCOVERY_CACHE_SECS,
            priors: toml::Table::new(),
            state_dir: None,
        };
        config.save(&path).unwrap();

        let loaded = DaemonConfig::load_or_default(&path).unwrap();
        assert_eq!(loaded.max_active_repos, 7);
        assert_eq!(loaded.whitelist_dirs, vec!["~/Gits"]);
        assert_eq!(loaded.repos.len(), 1);
    }

    /// A config loaded from a file knows the directory it came from, so the
    /// daemon can keep its own state beside it — with or without a
    /// `daemon.toml` already written there.
    #[test]
    fn a_loaded_config_knows_where_its_state_lives() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("daemon.toml");

        let absent = DaemonConfig::load_or_default(&path).unwrap();
        assert_eq!(absent.state_dir.as_deref(), Some(tmp.path()));
        assert_eq!(absent.repo_map_path(), Some(tmp.path().join("repos.json")));

        std::fs::write(&path, "max_active_repos = 3\n").unwrap();
        let present = DaemonConfig::load_or_default(&path).unwrap();
        assert_eq!(present.max_active_repos, 3);
        assert_eq!(present.repo_map_path(), Some(tmp.path().join("repos.json")));
    }

    /// A config nothing on disk backs has nowhere to put state, and must not
    /// invent one: a bare file name would make it the process working
    /// directory.
    #[test]
    fn a_config_with_no_directory_behind_it_has_no_state_path() {
        assert_eq!(DaemonConfig::default().repo_map_path(), None);
        assert_eq!(state_dir_of(Path::new("daemon.toml")), None);
        assert_eq!(
            state_dir_of(Path::new("/etc/mdkb/daemon.toml")),
            Some(PathBuf::from("/etc/mdkb"))
        );
    }

    #[test]
    fn test_daemon_socket_path_default() {
        let config = DaemonConfig::default();
        let path = config.socket_path();
        assert!(path.ends_with(".mdkb/daemon.sock"));
    }

    #[test]
    fn test_daemon_socket_path_custom() {
        let config = DaemonConfig {
            socket_path: Some("~/custom/mdkb.sock".to_string()),
            ..Default::default()
        };
        let path = config.socket_path();
        assert!(path.ends_with("custom/mdkb.sock"));
    }

    #[test]
    fn test_daemon_pid_path() {
        let config = DaemonConfig::default();
        let path = config.pid_path();
        assert!(path.ends_with(".mdkb/daemon.pid"));
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
