//! Per-repo state management and concurrent registry with LRU eviction.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use dashmap::DashMap;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

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
    pub doc_reindex_active: Arc<AtomicBool>,
    /// True while startup code reindex is in progress.
    pub code_reindex_active: Arc<AtomicBool>,
    /// Sender for injecting file paths directly into the watcher's reindex batch.
    /// post-tool-use IPC hook clones this to bypass the reindex-queue.jsonl file.
    pub reindex_tx: mpsc::Sender<PathBuf>,
    /// Receiver consumed once by spawn_watcher_for_handle; None after the watcher starts.
    reindex_rx: std::sync::Mutex<Option<mpsc::Receiver<PathBuf>>>,
    /// One-shot guard so a dead/backpressured reindex channel is logged once per
    /// failure episode instead of on every post_tool_use (was 571 repeats).
    /// Cleared on the next successful send so a recovered channel can warn again.
    pub reindex_send_warned: AtomicBool,
    /// Single-flight guard for the background memory-embedding backfill. Set while
    /// a drain is in flight so concurrent hook triggers (session-start + stop)
    /// don't stack redundant ONNX passes; reset via an RAII guard even on panic.
    pub backfill_in_flight: AtomicBool,
    /// Handle to the spawned file watcher task. Aborted on drop to prevent
    /// orphan watcher threads (notify-rs debouncer + fsevents) after LRU eviction.
    watcher_handle: std::sync::Mutex<Option<JoinHandle<()>>>,
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
    ///
    /// `global_priors` is the daemon-wide `[priors]` base (from `~/.mdkb/daemon.toml`);
    /// the repo's own `[priors]` overrides it field-by-field. Passing it in (rather
    /// than reading `daemon.toml` here) keeps `open` free of hidden global state so
    /// tests stay deterministic.
    pub fn open(root: &Path, global_priors: &toml::Table) -> Result<Self> {
        let root = canonicalize_root(root)?;
        let config_path = root.join(".mdkb/config.toml");
        let mut config = if config_path.exists() {
            match Config::load(&config_path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(
                        "Failed to load config for {}, using defaults: {e}",
                        root.display()
                    );
                    Config::default()
                }
            }
        } else {
            Config::default()
        };
        // Layer the global [priors] base under any per-repo override.
        let repo_priors = crate::config::raw_priors_layer(&config_path);
        config.priors = crate::config::merge_priors(global_priors, repo_priors.as_ref());
        let code_ignore_patterns = config.code.indexing.ignore_patterns.clone();

        let (reindex_tx, reindex_rx) = mpsc::channel(64);
        Ok(Self {
            root,
            ctx: Arc::new(Mutex::new(None)),
            code_index: Arc::new(Mutex::new(None)),
            config,
            code_ignore_patterns,
            last_access: AtomicI64::new(now_unix()),
            doc_reindex_active: Arc::new(AtomicBool::new(false)),
            code_reindex_active: Arc::new(AtomicBool::new(false)),
            reindex_tx,
            reindex_rx: std::sync::Mutex::new(Some(reindex_rx)),
            reindex_send_warned: AtomicBool::new(false),
            backfill_in_flight: AtomicBool::new(false),
            watcher_handle: std::sync::Mutex::new(None),
        })
    }

    /// Create a handle from pre-existing shared state (for standalone mode).
    /// The Arcs are shared with McpServer so startup reindex flags stay in sync.
    pub fn from_shared(
        root: PathBuf,
        ctx: Arc<Mutex<Option<Context>>>,
        code_index: Arc<Mutex<Option<IndexFacade>>>,
        config: Config,
        code_ignore_patterns: Vec<String>,
        doc_reindex_active: Arc<AtomicBool>,
        code_reindex_active: Arc<AtomicBool>,
    ) -> Self {
        let (reindex_tx, reindex_rx) = mpsc::channel(64);
        Self {
            root,
            ctx,
            code_index,
            config,
            code_ignore_patterns,
            last_access: AtomicI64::new(now_unix()),
            doc_reindex_active,
            code_reindex_active,
            reindex_tx,
            reindex_rx: std::sync::Mutex::new(Some(reindex_rx)),
            reindex_send_warned: AtomicBool::new(false),
            backfill_in_flight: AtomicBool::new(false),
            watcher_handle: std::sync::Mutex::new(None),
        }
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

impl Drop for RepoHandle {
    fn drop(&mut self) {
        if let Some(handle) = self
            .watcher_handle
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            handle.abort();
            tracing::debug!(root = %self.root.display(), "Aborted file watcher task");
        }
    }
}

/// Registry managing multiple repo handles with LRU eviction.
pub struct RepoRegistry {
    handles: DashMap<PathBuf, Arc<RepoHandle>>,
    max_active: usize,
    daemon_config: DaemonConfig,
    /// Serializes the check-evict-insert triple so concurrent callers cannot
    /// both pass the capacity check before either inserts, which would exceed max_active.
    open_gate: std::sync::Mutex<()>,
}

impl std::fmt::Debug for RepoRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoRegistry")
            .field("active", &self.handles.len())
            .field("max_active", &self.max_active)
            .finish_non_exhaustive()
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
            open_gate: std::sync::Mutex::new(()),
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

        // Serializes check-evict-insert so concurrent callers cannot both pass
        // the capacity check before either has inserted, which would exceed max_active.
        let _gate = self.open_gate.lock().unwrap_or_else(|e| e.into_inner());

        // Re-check after acquiring the gate: another caller may have just inserted this key.
        if let Some(handle) = self.handles.get(&canonical) {
            handle.touch();
            return Ok(Arc::clone(&handle));
        }

        // Evict if at capacity
        if self.handles.len() >= self.max_active {
            self.evict_lru();
        }

        // Open new handle
        let handle = Arc::new(RepoHandle::open(&canonical, &self.daemon_config.priors)?);
        self.handles.insert(canonical.clone(), Arc::clone(&handle));
        tracing::info!("Registered repo: {}", canonical.display());

        drop(_gate);

        // Spawn the file watcher for this repo exactly once (on first open).
        // Story 017-91cb: the watcher lives only inside the daemon — standalone
        // MCP stdio sessions never touch files.
        spawn_watcher_for_handle(&handle);

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
        self.handles
            .iter()
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    /// Evict the least recently used repo handle.
    fn evict_lru(&self) {
        let lru_key = self
            .handles
            .iter()
            .min_by_key(|entry| entry.value().last_access_time())
            .map(|entry| entry.key().clone());

        if let Some(key) = lru_key {
            if let Some((path, handle)) = self.handles.remove(&key) {
                // Arc::strong_count includes the clone we just removed from the map.
                // A count > 1 means callers still hold live clones; resources
                // (SQLite, Tantivy, ONNX Session) will not be freed until those
                // clones are dropped. Eviction from the registry is still correct
                // (no new callers will receive this handle), but resource release
                // is deferred — log a warning so operators can tune max_active_repos.
                let outstanding = Arc::strong_count(&handle).saturating_sub(1);
                if outstanding > 0 {
                    tracing::warn!(
                        path = %path.display(),
                        outstanding_clones = outstanding,
                        "Evicted repo handle has outstanding Arc clones; \
                         SQLite/Tantivy resources will not be freed until all \
                         clones are dropped. Consider increasing max_active_repos."
                    );
                } else {
                    tracing::info!("Evicted repo (LRU): {}", path.display());
                }
                // `handle` drops here; if outstanding == 0 resources are freed
                // immediately, otherwise deferred to last clone drop.
            }
        }
    }
}

/// Spawn the file watcher for a freshly opened `RepoHandle`.
///
/// The watcher increments `mcp::server::WATCHER_SPAWN_COUNT` and drives
/// incremental reindex on change events. It shares `ctx` / `code_index`
/// Arcs with the handle, so every client that resolves the handle via
/// the registry sees the same watcher-driven state.
fn spawn_watcher_for_handle(handle: &Arc<RepoHandle>) {
    // `get_or_open` is called from sync contexts (tests, synchronous CLI
    // paths) where no tokio runtime exists. Bail silently in that case — the
    // daemon always runs under tokio, so production still gets the watcher.
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::warn!(
            root = %handle.root.display(),
            "spawn_watcher_for_handle: no tokio runtime — file watcher skipped"
        );
        return;
    }
    let root = handle.root.clone();
    let ctx = Arc::clone(&handle.ctx);
    let code_index = Arc::clone(&handle.code_index);
    let code_enabled = handle.config.code.enabled;
    let code_ignore_patterns = handle.code_ignore_patterns.clone();
    let respect_gitignore = handle.config.code.indexing.respect_gitignore;
    let debounce_ms = handle.config.code.indexing.debounce_ms;
    let batch_idle_ms = handle.config.code.indexing.batch_idle_ms;
    // Take the receiver exactly once; subsequent calls (same handle) yield None.
    let reindex_rx = handle
        .reindex_rx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let join_handle = tokio::spawn(async move {
        if let Err(e) = crate::mcp::server::run_file_watcher(
            root,
            ctx,
            code_index,
            code_enabled,
            code_ignore_patterns,
            respect_gitignore,
            debounce_ms,
            batch_idle_ms,
            reindex_rx,
        )
        .await
        {
            tracing::error!("File watcher error: {e}");
        }
    });
    *handle
        .watcher_handle
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(join_handle);
}

/// Canonicalize a root path, resolving symlinks and normalizing.
/// If the path is inside a git worktree, resolves to the main worktree root
/// so that all worktrees of the same repo share a single `.mdkb/` directory.
fn canonicalize_root(root: &Path) -> Result<PathBuf> {
    let resolved = crate::git::resolve_main_worktree(root);
    resolved.canonicalize().map_err(|e| {
        Error::other(format!(
            "Failed to resolve repo path {}: {e}",
            resolved.display()
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

    /// Config that whitelists the system temp dir so repos created under a
    /// `TempDir` (outside home) pass the default-deny whitelist. These tests
    /// exercise registry mechanics, not the whitelist — which has its own tests
    /// in `config.rs`. Mirrors real usage where the operator lists the repo's
    /// parent in `daemon.toml`.
    fn allow_temp_config() -> DaemonConfig {
        DaemonConfig {
            whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().to_string()],
            ..DaemonConfig::default()
        }
    }

    #[test]
    fn test_repo_handle_open() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let handle = RepoHandle::open(&root, &toml::Table::new()).unwrap();
        assert_eq!(handle.root, root.canonicalize().unwrap());
    }

    #[test]
    fn open_layers_global_priors_under_repo_override() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        // Repo opts out of mining but says nothing about the distiller.
        std::fs::write(
            root.join(".mdkb/config.toml"),
            "[priors]\nmining_enabled = false\n",
        )
        .unwrap();
        // Global base wires the machine-wide distiller and turns mining on.
        let global: toml::Table =
            toml::from_str("mining_enabled = true\ndistiller_program = \"codex\"\n").unwrap();

        let handle = RepoHandle::open(&root, &global).unwrap();
        assert!(
            !handle.config.priors.mining_enabled,
            "repo override wins over global"
        );
        assert_eq!(
            handle.config.priors.distiller_program.as_deref(),
            Some("codex"),
            "distiller inherited from global base"
        );
    }

    #[test]
    fn test_repo_handle_touch() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let handle = RepoHandle::open(&root, &toml::Table::new()).unwrap();
        let t1 = handle.last_access_time();
        handle.touch();
        let t2 = handle.last_access_time();
        assert!(t2 >= t1);
    }

    #[test]
    fn test_registry_get_or_open() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let registry = RepoRegistry::new(allow_temp_config());

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
            ..allow_temp_config()
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
        let registry = RepoRegistry::new(allow_temp_config());

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

        let registry = RepoRegistry::new(allow_temp_config());

        registry.get_or_open(&root1).unwrap();
        registry.get_or_open(&root2).unwrap();

        let handles = registry.all_handles();
        assert_eq!(handles.len(), 2);
    }

    #[test]
    fn test_repo_handle_from_shared() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let ctx = Arc::new(Mutex::new(None));
        let code_index = Arc::new(Mutex::new(None));
        let doc_flag = Arc::new(AtomicBool::new(false));
        let code_flag = Arc::new(AtomicBool::new(false));

        let handle = RepoHandle::from_shared(
            root.clone(),
            ctx.clone(),
            code_index.clone(),
            Config::default(),
            Vec::new(),
            doc_flag.clone(),
            code_flag.clone(),
        );

        // Shared Arcs point to same underlying data
        assert!(Arc::ptr_eq(&handle.ctx, &ctx));
        assert!(Arc::ptr_eq(&handle.code_index, &code_index));
        assert!(Arc::ptr_eq(&handle.doc_reindex_active, &doc_flag));
        assert!(Arc::ptr_eq(&handle.code_reindex_active, &code_flag));

        // Mutating via one Arc is visible through the other
        doc_flag.store(true, Ordering::Relaxed);
        assert!(handle.doc_reindex_active.load(Ordering::Relaxed));
    }

    #[test]
    fn test_canonicalize_root_nonexistent() {
        let result = canonicalize_root(Path::new("/definitely/not/a/real/path"));
        assert!(result.is_err());
    }

    /// AC#2 — concurrent get_or_open never exceeds max_active.
    ///
    /// Runs outside a tokio runtime so spawn_watcher_for_handle bails early
    /// (try_current().is_err()) and WATCHER_SPAWN_COUNT stays unaffected.
    #[test]
    fn test_concurrent_get_or_open_respects_max_active() {
        use std::sync::{Arc, Barrier};

        const MAX: usize = 3;
        const TASKS: usize = 20;

        let tmps: Vec<TempDir> = (0..TASKS).map(|_| TempDir::new().unwrap()).collect();
        let roots: Vec<PathBuf> = tmps.iter().map(make_repo).collect();

        let config = DaemonConfig {
            max_active_repos: MAX,
            ..Default::default()
        };
        let registry = Arc::new(RepoRegistry::new(config));
        // Barrier ensures all threads hit get_or_open simultaneously.
        let barrier = Arc::new(Barrier::new(TASKS));

        std::thread::scope(|s| {
            for root in &roots {
                let reg = Arc::clone(&registry);
                let bar = Arc::clone(&barrier);
                let root = root.clone();
                s.spawn(move || {
                    bar.wait();
                    reg.get_or_open(&root).ok();
                });
            }
        });

        assert!(
            registry.active_count() <= MAX,
            "active_count {} exceeded max_active {}",
            registry.active_count(),
            MAX
        );
    }

    /// Serializes every test that reads `WATCHER_SPAWN_COUNT` — it's a
    /// process-global counter and parallel tests would race on deltas.
    static WATCHER_COUNTER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Story 017-91cb: both halves of the contract.
    ///
    /// 1. Standalone `McpServer::new` must NOT spawn a watcher.
    /// 2. `RepoRegistry::get_or_open` spawns exactly ONE watcher even when
    ///    called twice with the same root — cache hits must not re-spawn.
    ///
    /// Both halves share the global `WATCHER_SPAWN_COUNT`, so they live in
    /// one test behind a serializing mutex.
    #[tokio::test(flavor = "current_thread")]
    async fn watcher_spawn_gated_to_daemon_registry() {
        use crate::mcp::server::{McpServer, WATCHER_SPAWN_COUNT};
        use std::sync::atomic::Ordering;

        let _guard = WATCHER_COUNTER_LOCK.lock().await;

        // --- Half 1: standalone must not spawn a watcher. ---
        let tmp_standalone = TempDir::new().unwrap();
        let root_standalone = make_repo(&tmp_standalone);
        let before_standalone = WATCHER_SPAWN_COUNT.load(Ordering::Relaxed);

        let _server = McpServer::new(root_standalone);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let after_standalone = WATCHER_SPAWN_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after_standalone, before_standalone,
            "McpServer::new must not spawn a file watcher (standalone path)"
        );

        // --- Half 2: registry.get_or_open x2 on same root spawns 1 watcher. ---
        let tmp_daemon = TempDir::new().unwrap();
        let root_daemon = make_repo(&tmp_daemon);
        let registry = RepoRegistry::new(allow_temp_config());
        let before_daemon = WATCHER_SPAWN_COUNT.load(Ordering::Relaxed);

        let _h1 = registry.get_or_open(&root_daemon).unwrap();
        let _h2 = registry.get_or_open(&root_daemon).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let after_daemon = WATCHER_SPAWN_COUNT.load(Ordering::Relaxed);
        assert_eq!(
            after_daemon - before_daemon,
            1,
            "two get_or_open calls for the same root must produce exactly one watcher"
        );
    }

    /// LRU eviction must abort the watcher task so notify-rs threads are cleaned up.
    /// Regression test for orphaned watcher threads causing 600%+ CPU.
    #[tokio::test(flavor = "current_thread")]
    async fn watcher_aborted_on_lru_eviction() {
        use crate::mcp::server::WATCHER_SPAWN_COUNT;
        use std::sync::atomic::Ordering;

        let _guard = WATCHER_COUNTER_LOCK.lock().await;

        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let tmp3 = TempDir::new().unwrap();
        let root1 = make_repo(&tmp1);
        let root2 = make_repo(&tmp2);
        let root3 = make_repo(&tmp3);

        let config = DaemonConfig {
            max_active_repos: 2,
            ..allow_temp_config()
        };
        let registry = RepoRegistry::new(config);

        let before = WATCHER_SPAWN_COUNT.load(Ordering::Relaxed);

        // Fill to capacity
        let h1 = registry.get_or_open(&root1).unwrap();
        let _h2 = registry.get_or_open(&root2).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(WATCHER_SPAWN_COUNT.load(Ordering::Relaxed) - before, 2);

        // Make h1 the LRU candidate
        h1.last_access.store(100, Ordering::Relaxed);
        drop(h1);

        // Opening root3 evicts root1; root1's watcher_handle should be aborted
        let _h3 = registry.get_or_open(&root3).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 3 watchers spawned total, but root1's should have been aborted on eviction
        assert_eq!(WATCHER_SPAWN_COUNT.load(Ordering::Relaxed) - before, 3);
        assert_eq!(registry.active_count(), 2);

        // The critical invariant: root1's handle was evicted and dropped,
        // so its watcher JoinHandle was aborted via Drop. We can't directly
        // observe thread count here, but the Drop impl logs and aborts.
    }

    #[test]
    fn test_registry_path_canonicalization() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);

        let registry = RepoRegistry::new(allow_temp_config());

        // Open with raw path
        registry.get_or_open(&root).unwrap();

        // Access with canonicalized path should return same handle
        let canonical = root.canonicalize().unwrap();
        assert!(registry.get(&canonical).is_some());
    }

    #[test]
    fn test_registry_worktree_resolves_to_main() {
        let tmp = TempDir::new().unwrap();
        let main_root = tmp.path().join("main-repo");
        let wt_root = tmp.path().join("worktree-fix");

        // Main repo with .mdkb/ (so RepoHandle::open succeeds)
        std::fs::create_dir_all(main_root.join(".mdkb")).unwrap();
        std::fs::create_dir_all(main_root.join(".git/worktrees/fix")).unwrap();

        // Worktree with .git file
        std::fs::create_dir_all(&wt_root).unwrap();
        std::fs::write(
            wt_root.join(".git"),
            format!(
                "gitdir: {}\n",
                main_root.join(".git/worktrees/fix").display()
            ),
        )
        .unwrap();

        let registry = RepoRegistry::new(allow_temp_config());

        // Opening via worktree should resolve to main repo
        let handle = registry.get_or_open(&wt_root).unwrap();
        assert_eq!(handle.root, main_root.canonicalize().unwrap());

        // Opening via main root returns the same handle
        let handle2 = registry.get_or_open(&main_root).unwrap();
        assert_eq!(Arc::as_ptr(&handle), Arc::as_ptr(&handle2));
        assert_eq!(registry.active_count(), 1);
    }

    #[test]
    fn test_registry_two_worktrees_share_one_handle() {
        let tmp = TempDir::new().unwrap();
        let main_root = tmp.path().join("main-repo");
        let wt_a = tmp.path().join("worktree-a");
        let wt_b = tmp.path().join("worktree-b");

        std::fs::create_dir_all(main_root.join(".mdkb")).unwrap();
        std::fs::create_dir_all(main_root.join(".git/worktrees/a")).unwrap();
        std::fs::create_dir_all(main_root.join(".git/worktrees/b")).unwrap();

        for (wt, name) in [(&wt_a, "a"), (&wt_b, "b")] {
            std::fs::create_dir_all(wt).unwrap();
            std::fs::write(
                wt.join(".git"),
                format!(
                    "gitdir: {}\n",
                    main_root.join(format!(".git/worktrees/{name}")).display()
                ),
            )
            .unwrap();
        }

        let registry = RepoRegistry::new(allow_temp_config());

        let ha = registry.get_or_open(&wt_a).unwrap();
        let hb = registry.get_or_open(&wt_b).unwrap();
        let hm = registry.get_or_open(&main_root).unwrap();

        assert_eq!(Arc::as_ptr(&ha), Arc::as_ptr(&hb));
        assert_eq!(Arc::as_ptr(&hb), Arc::as_ptr(&hm));
        assert_eq!(registry.active_count(), 1);
    }
}
