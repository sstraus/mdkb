//! Per-repo state management and concurrent registry with LRU eviction.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::cli::handlers::Context;
use crate::code::indexing::IndexFacade;
use crate::config::Config;
use crate::error::{Error, Result};

use super::config::DaemonConfig;

/// Per-repo state: wraps all resources needed to serve MCP tools for one repository.
pub struct RepoHandle {
    /// Canonical absolute path to the repository root.
    pub root: PathBuf,
    /// Database context (SQLite connection + paths). Behind Mutex because rusqlite::Connection is !Sync.
    pub ctx: Arc<Mutex<Option<Context>>>,
    /// Code intelligence index (separate SQLite + Tantivy).
    pub code_index: Arc<Mutex<Option<IndexFacade>>>,
    /// Per-repo config loaded from {root}/.mdkb/config.toml.
    pub config: Config,
    /// Glob patterns to exclude from code indexing.
    pub code_ignore_patterns: Vec<String>,
    /// Unix timestamp of last access (for LRU eviction).
    pub last_access: AtomicI64,
    /// True while startup doc/session reindex holds ctx.
    pub doc_reindex_active: AtomicBool,
    /// True while startup code reindex is in progress.
    pub code_reindex_active: AtomicBool,
}

impl std::fmt::Debug for RepoHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoHandle")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl RepoHandle {
    /// Open (or create) a repo handle for the given root path.
    pub fn open(root: &Path) -> Result<Self> {
        let root = canonicalize_root(root)?;
        let config_path = root.join(".mdkb/config.toml");
        let config = if config_path.exists() {
            match Config::load(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to load config for {}, using defaults: {e}", root.display());
                    Config::default()
                }
            }
        } else {
            Config::default()
        };
        let code_ignore_patterns = config.code.indexing.ignore_patterns.clone();

        Ok(Self {
            root,
            ctx: Arc::new(Mutex::new(None)),
            code_index: Arc::new(Mutex::new(None)),
            config,
            code_ignore_patterns,
            last_access: AtomicI64::new(now_unix()),
            doc_reindex_active: AtomicBool::new(false),
            code_reindex_active: AtomicBool::new(false),
        })
    }

    /// Touch the last_access timestamp (called on each tool invocation).
    pub fn touch(&self) {
        self.last_access.store(now_unix(), Ordering::Relaxed);
    }

    /// Get the last_access timestamp.
    pub fn last_access_time(&self) -> i64 {
        self.last_access.load(Ordering::Relaxed)
    }
}

/// Registry managing multiple repo handles with LRU eviction.
pub struct RepoRegistry {
    handles: DashMap<PathBuf, Arc<RepoHandle>>,
    max_active: usize,
    daemon_config: DaemonConfig,
}

impl std::fmt::Debug for RepoRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoRegistry")
            .field("active", &self.handles.len())
            .field("max_active", &self.max_active)
            .finish()
    }
}

impl RepoRegistry {
    /// Create a new registry from daemon config.
    pub fn new(config: DaemonConfig) -> Self {
        let max_active = config.max_active_repos;
        Self {
            handles: DashMap::new(),
            max_active,
            daemon_config: config,
        }
    }

    /// Get or open a repo handle, applying whitelist check and LRU eviction.
    pub fn get_or_open(&self, root: &Path) -> Result<Arc<RepoHandle>> {
        let canonical = canonicalize_root(root)?;

        // Fast path: already open
        if let Some(handle) = self.handles.get(&canonical) {
            handle.touch();
            return Ok(Arc::clone(&handle));
        }

        // Whitelist check before opening
        self.daemon_config.check_whitelist(&canonical)?;

        // Evict if at capacity
        if self.handles.len() >= self.max_active {
            self.evict_lru();
        }

        // Open new handle
        let handle = Arc::new(RepoHandle::open(&canonical)?);
        self.handles.insert(canonical.clone(), Arc::clone(&handle));
        tracing::info!("Registered repo: {}", canonical.display());

        Ok(handle)
    }

    /// Get an existing handle without opening (returns None if not registered).
    pub fn get(&self, root: &Path) -> Option<Arc<RepoHandle>> {
        let canonical = canonicalize_root(root).ok()?;
        self.handles.get(&canonical).map(|h| {
            h.touch();
            Arc::clone(&h)
        })
    }

    /// List all registered repo roots with their last access time.
    pub fn list(&self) -> Vec<(PathBuf, i64)> {
        self.handles
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().last_access_time()))
            .collect()
    }

    /// Number of currently active repo handles.
    pub fn active_count(&self) -> usize {
        self.handles.len()
    }

    /// Get all active repo handles (for cross-repo operations).
    pub fn all_handles(&self) -> Vec<Arc<RepoHandle>> {
        self.handles.iter().map(|entry| Arc::clone(entry.value())).collect()
    }

    /// Evict the least recently used repo handle.
    fn evict_lru(&self) {
        let lru_key = self
            .handles
            .iter()
            .min_by_key(|entry| entry.value().last_access_time())
            .map(|entry| entry.key().clone());

        if let Some(key) = lru_key {
            if let Some((path, _handle)) = self.handles.remove(&key) {
                tracing::info!("Evicted repo (LRU): {}", path.display());
                // RepoHandle drop releases SQLite connections, Tantivy readers.
                // ONNX Session (if held by IndexFacade) is dropped here too.
                // Caller should trigger mi_collect(true) after eviction batch.
            }
        }
    }
}

/// Canonicalize a root path, resolving symlinks and normalizing.
fn canonicalize_root(root: &Path) -> Result<PathBuf> {
    root.canonicalize().map_err(|e| {
        Error::other(format!(
            "Failed to resolve repo path {}: {e}",
            root.display()
        ))
    })
}

/// Current unix timestamp in seconds.
fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_repo(tmp: &TempDir) -> PathBuf {
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        root
    }

    #[test]
    fn test_repo_handle_open() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let handle = RepoHandle::open(&root).unwrap();
        assert_eq!(handle.root, root.canonicalize().unwrap());
    }

    #[test]
    fn test_repo_handle_touch() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let handle = RepoHandle::open(&root).unwrap();
        let t1 = handle.last_access_time();
        handle.touch();
        let t2 = handle.last_access_time();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_registry_get_or_open() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let config = DaemonConfig::default(); // empty whitelist = allow all
        let registry = RepoRegistry::new(config);

        let handle = registry.get_or_open(&root).unwrap();
        assert_eq!(handle.root, root.canonicalize().unwrap());
        assert_eq!(registry.active_count(), 1);

        // Second call returns same handle
        let handle2 = registry.get_or_open(&root).unwrap();
        assert_eq!(Arc::as_ptr(&handle), Arc::as_ptr(&handle2));
    }

    #[test]
    fn test_registry_whitelist_rejects() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let config = DaemonConfig {
            whitelist_dirs: vec!["/nonexistent/allowed".to_string()],
            ..Default::default()
        };
        let registry = RepoRegistry::new(config);

        let result = registry.get_or_open(&root);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("whitelist"));
    }

    #[test]
    fn test_registry_whitelist_allows() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let config = DaemonConfig {
            whitelist_dirs: vec![tmp.path().parent().unwrap().to_string_lossy().to_string()],
            ..Default::default()
        };
        let registry = RepoRegistry::new(config);

        assert!(registry.get_or_open(&root).is_ok());
    }

    #[test]
    fn test_registry_lru_eviction() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let tmp3 = TempDir::new().unwrap();
        let root1 = make_repo(&tmp1);
        let root2 = make_repo(&tmp2);
        let root3 = make_repo(&tmp3);

        let config = DaemonConfig {
            max_active_repos: 2,
            ..Default::default()
        };
        let registry = RepoRegistry::new(config);

        // Open 2 repos (at capacity)
        let h1 = registry.get_or_open(&root1).unwrap();
        let h2 = registry.get_or_open(&root2).unwrap();
        assert_eq!(registry.active_count(), 2);

        // Force h1 to be older (LRU) by setting timestamps explicitly
        h1.last_access.store(100, Ordering::Relaxed);
        h2.last_access.store(200, Ordering::Relaxed);

        // Open 3rd triggers eviction of h1 (LRU)
        let _h3 = registry.get_or_open(&root3).unwrap();
        assert_eq!(registry.active_count(), 2);

        // h1 should be evicted
        assert!(registry.get(&root1).is_none());
        // h2 and h3 should still be there
        assert!(registry.get(&root2).is_some());
        assert!(registry.get(&root3).is_some());
    }

    #[test]
    fn test_registry_list() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let config = DaemonConfig::default();
        let registry = RepoRegistry::new(config);

        registry.get_or_open(&root).unwrap();
        let entries = registry.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, root.canonicalize().unwrap());
    }

    #[test]
    fn test_registry_all_handles() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let root1 = make_repo(&tmp1);
        let root2 = make_repo(&tmp2);

        let config = DaemonConfig::default();
        let registry = RepoRegistry::new(config);

        registry.get_or_open(&root1).unwrap();
        registry.get_or_open(&root2).unwrap();

        let handles = registry.all_handles();
        assert_eq!(handles.len(), 2);
    }

    #[test]
    fn test_canonicalize_root_nonexistent() {
        let result = canonicalize_root(Path::new("/definitely/not/a/real/path"));
        assert!(result.is_err());
    }

    #[test]
    fn test_registry_path_canonicalization() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);

        let config = DaemonConfig::default();
        let registry = RepoRegistry::new(config);

        // Open with raw path
        registry.get_or_open(&root).unwrap();

        // Access with canonicalized path should return same handle
        let canonical = root.canonicalize().unwrap();
        assert!(registry.get(&canonical).is_some());
    }
}
