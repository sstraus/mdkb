//! Per-repo state management and concurrent registry with LRU eviction.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use dashmap::DashMap;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;

use crate::code::indexing::IndexFacade;
use crate::config::Config;
use crate::core::Context;
use crate::error::{Error, ErrorKind, Result};

use super::config::DaemonConfig;
use super::repo_map::RepoMap;

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
    /// How `config.toml` looked when `config` was read from it.
    config_identity: ConfigIdentity,
    /// Why the last reload was refused; `config` is then the last good one.
    config_error: std::sync::Mutex<Option<String>>,
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
    /// The cross-encoder automatic recall reranks with. Every handle points at
    /// the one process-wide instance; a test swaps in its own.
    pub reranker: Arc<dyn crate::llm::rerank::Reranker>,
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
        let config_path = config_path(&root);
        let identity = ConfigIdentity::of(&config_path);
        let config = load_config(&config_path, global_priors).unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to load config for {}, using defaults: {e}",
                root.display()
            );
            let mut config = Config::default();
            config.priors = crate::config::merge_priors(global_priors, None);
            config
        });
        Ok(Self::with_state(
            root,
            config,
            identity,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ))
    }

    /// A handle serving what `config.toml` says now, over this handle's store.
    ///
    /// The store, code index and reindex flags are shared, not reopened: a
    /// reload must not open a second SQLite connection or rerun startup
    /// indexing. The caller spawns the new handle's watcher; the old watcher
    /// stops when the last request holding the old handle drops it.
    ///
    /// Unlike [`open`](Self::open), a file that does not parse is an error, not
    /// a fall back to defaults: cold start has nothing better, a reload has the
    /// config it is replacing.
    fn reopen(&self, global_priors: &toml::Table) -> Result<Self> {
        let config_path = config_path(&self.root);
        // Identity first: an edit landing during the read is then seen as a
        // change on the next access instead of being recorded as already read.
        let identity = ConfigIdentity::of(&config_path);
        let config = load_config(&config_path, global_priors)?;
        Ok(Self::with_state(
            self.root.clone(),
            config,
            identity,
            Arc::clone(&self.ctx),
            Arc::clone(&self.code_index),
            Arc::clone(&self.doc_reindex_active),
            Arc::clone(&self.code_reindex_active),
        ))
    }

    fn with_state(
        root: PathBuf,
        config: Config,
        config_identity: ConfigIdentity,
        ctx: Arc<Mutex<Option<Context>>>,
        code_index: Arc<Mutex<Option<IndexFacade>>>,
        doc_reindex_active: Arc<AtomicBool>,
        code_reindex_active: Arc<AtomicBool>,
    ) -> Self {
        let code_ignore_patterns = config.code.indexing.ignore_patterns.clone();
        let (reindex_tx, reindex_rx) = mpsc::channel(64);
        Self {
            root,
            ctx,
            code_index,
            config,
            code_ignore_patterns,
            config_identity,
            config_error: std::sync::Mutex::new(None),
            last_access: AtomicI64::new(now_unix()),
            doc_reindex_active,
            code_reindex_active,
            reindex_tx,
            reindex_rx: std::sync::Mutex::new(Some(reindex_rx)),
            reindex_send_warned: AtomicBool::new(false),
            backfill_in_flight: AtomicBool::new(false),
            watcher_handle: std::sync::Mutex::new(None),
            reranker: crate::llm::rerank::shared(),
        }
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
        let config_identity = ConfigIdentity::of(&config_path(&root));
        Self {
            root,
            ctx,
            code_index,
            config,
            code_ignore_patterns,
            config_identity,
            config_error: std::sync::Mutex::new(None),
            last_access: AtomicI64::new(now_unix()),
            doc_reindex_active,
            code_reindex_active,
            reindex_tx,
            reindex_rx: std::sync::Mutex::new(Some(reindex_rx)),
            reindex_send_warned: AtomicBool::new(false),
            backfill_in_flight: AtomicBool::new(false),
            watcher_handle: std::sync::Mutex::new(None),
            reranker: crate::llm::rerank::shared(),
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

    /// Why the last reload of `config.toml` was refused, while it still is.
    pub fn config_error(&self) -> Option<String> {
        self.config_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Record a refused reload. True when the reason is new, so the caller
    /// logs a broken file once rather than on every request against it.
    fn set_config_error(&self, why: String) -> bool {
        let mut slot = self.config_error.lock().unwrap_or_else(|e| e.into_inner());
        let new = slot.as_deref() != Some(why.as_str());
        *slot = Some(why);
        new
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

fn config_path(root: &Path) -> PathBuf {
    root.join(".mdkb/config.toml")
}

/// `config.toml` read strictly, with the daemon's global `[priors]` layered
/// under the repo's own. A missing file is the defaults, not an error.
fn load_config(path: &Path, global_priors: &toml::Table) -> Result<Config> {
    let mut config = if path.exists() {
        Config::load(path)?
    } else {
        Config::default()
    };
    let repo_priors = crate::config::raw_priors_layer(path);
    config.priors = crate::config::merge_priors(global_priors, repo_priors.as_ref());
    Ok(config)
}

/// How a config file looked when it was read: `(len, mtime)`, or absent.
///
/// The same trade as [`super::ExeIdentity`]: a `stat` per request instead of
/// a content hash. A `touch` costs one needless reload; a missed edit costs a
/// config that silently does not apply.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigIdentity {
    path: PathBuf,
    stamp: Option<(u64, Option<std::time::SystemTime>)>,
}

impl ConfigIdentity {
    fn of(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            stamp: Self::stamp(path),
        }
    }

    fn stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
        std::fs::metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok()))
    }

    fn changed(&self) -> bool {
        Self::stamp(&self.path) != self.stamp
    }
}

/// Why a store could not be opened for a cross-repo read.
#[derive(Debug)]
pub enum ReadRefusal {
    /// Older than this binary. Not a fault of the store: `mdkb repos refresh
    /// --only outdated` brings it forward, so a caller reports these together
    /// instead of one paragraph each.
    SchemaOutdated { found: i32 },
    /// Anything else, in the store's own words.
    Other(String),
}

impl std::fmt::Display for ReadRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SchemaOutdated { found } => write!(
                f,
                "schema v{found} is older than this binary's v{}",
                crate::store::schema::SCHEMA_VERSION
            ),
            Self::Other(why) => f.write_str(why),
        }
    }
}

/// Registry managing multiple repo handles with LRU eviction.
pub struct RepoRegistry {
    handles: DashMap<PathBuf, Arc<RepoHandle>>,
    max_active: usize,
    daemon_config: DaemonConfig,
    /// `daemon_config.ignore`, validated once: a bad entry is reported when the
    /// registry is built, not on every discovery call.
    ignored: Vec<PathBuf>,
    /// The last scope policy that read cleanly; what a bad `repos.json` falls
    /// back to.
    last_scope_policy: std::sync::Mutex<Option<super::scope::ScopePolicy>>,
    /// Every repo this daemon has ever opened, persisted across restarts.
    /// Deliberately not `handles`: that one is capped at `max_active` and
    /// starts empty in every process.
    repo_map: RepoMap,
    /// Serializes the check-evict-insert triple so concurrent callers cannot
    /// both pass the capacity check before either inserts, which would exceed max_active.
    open_gate: std::sync::Mutex<()>,
    /// The last nested-store walk, reusable while it is fresh and while the
    /// set of directories it covered has not changed.
    discovery: std::sync::Mutex<Option<Discovery>>,
}

/// One nested-store walk, and what it is valid for.
///
/// `walked` is the key, not the `extra` that was asked for: two calls with
/// different scopes that both fall inside the same known root walk the same
/// directories, and the second must not repeat the first.
struct Discovery {
    walked: Vec<PathBuf>,
    roots: Vec<PathBuf>,
    at: std::time::Instant,
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
    ///
    /// Loading the config loads the repo map with it: the persisted set and the
    /// `[[repos]]` of `daemon.toml` are unioned here, so a restarted daemon
    /// knows its repos before any client knocks.
    pub fn new(config: DaemonConfig) -> Self {
        let max_active = config.max_active_repos;
        let repo_map = RepoMap::open(config.repo_map_path(), &config.repos);
        let ignored = config.ignored_paths();
        Self {
            handles: DashMap::new(),
            max_active,
            daemon_config: config,
            ignored,
            last_scope_policy: std::sync::Mutex::new(None),
            repo_map,
            open_gate: std::sync::Mutex::new(()),
            discovery: std::sync::Mutex::new(None),
        }
    }

    /// Create a single-repo registry around an already-open standalone handle.
    ///
    /// Network MCP and HTTP hooks then resolve the same `RepoHandle` and share
    /// its SQLite connection, index facade, and startup state.
    #[cfg(feature = "http-server")]
    pub(crate) fn with_handle(config: DaemonConfig, handle: Arc<RepoHandle>) -> Self {
        let registry = Self::new(config);
        registry
            .handles
            .insert(handle.root.clone(), Arc::clone(&handle));
        registry
    }

    /// Get or open a repo handle, applying whitelist check and LRU eviction.
    pub fn get_or_open(&self, root: &Path) -> Result<Arc<RepoHandle>> {
        let canonical = canonicalize_root(root)?;

        // Fast path: already open
        if let Some(handle) = self.open_handle(&canonical) {
            return Ok(handle);
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
        let handle = match RepoHandle::open(&canonical, &self.daemon_config.priors) {
            Ok(handle) => Arc::new(handle),
            Err(e) => {
                // An open that fails is not evidence the repo is gone: a schema
                // newer than this binary, a corrupt index and a held lock all
                // land here, and all of them read again from another binary or
                // a moment later. Say so and leave the root on the map.
                if self.repo_map.contains(&canonical) {
                    tracing::warn!(
                        root = %canonical.display(),
                        "Known repo failed to open ({e}); kept on the map — an unreadable store is not a deleted repo"
                    );
                }
                return Err(e);
            }
        };
        self.handles.insert(canonical.clone(), Arc::clone(&handle));
        // Recorded only here: after the whitelist check and after the store
        // opened, so a refused or broken root never enters the map.
        self.repo_map.record(&canonical);
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
        self.open_handle(&canonical)
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

    /// Which scope each path belongs to, for `root="*"`. Read from `daemon.toml`
    /// and `repos.json` on every call, as the CLI does, so an edit made while
    /// the daemon runs applies to the next search and both surfaces agree. A
    /// `repos.json` that cannot be read right now (a hand edit in progress)
    /// keeps the last policy that could.
    pub fn scope_policy(&self) -> super::scope::ScopePolicy {
        let mut last = self
            .last_scope_policy
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match super::scope::ScopePolicy::try_load(&self.daemon_config) {
            Ok(policy) => {
                *last = Some(policy.clone());
                policy
            }
            Err((partial, why)) => {
                tracing::warn!("repos.json unreadable, keeping the last good scopes: {why}");
                last.clone().unwrap_or(partial)
            }
        }
    }

    /// Every repo this daemon knows, whether or not a handle is open for it.
    ///
    /// Survives both LRU eviction and a daemon restart, so it answers "which
    /// repos are there" where [`list`](Self::list) answers "which repos are
    /// open right now".
    pub fn known_roots(&self) -> Vec<PathBuf> {
        self.repo_map.roots()
    }

    /// Known roots plus stores nested below them, found by a read-only
    /// filesystem walk. Discovery does not add handles or persist map entries.
    pub fn discoverable_roots(&self) -> Vec<PathBuf> {
        self.discoverable_roots_under(&[])
    }

    /// The same, also walking `extra` — the workspace an MCP client declared.
    ///
    /// A directory that merely holds repositories anchors no store, so nothing
    /// registers it and it never enters the map. Walking only the map therefore
    /// found nothing under it, and a `root`-less call fell back to whatever
    /// happened to be open — while the log line said "kept as the fan-out
    /// scope; stores nested below it answer". They did not answer, because
    /// nobody looked. Passing the scope in is what makes that sentence true.
    pub fn discoverable_roots_under(&self, extra: &[PathBuf]) -> Vec<PathBuf> {
        let known = self.known_roots();
        let mut roots: std::collections::BTreeSet<PathBuf> = known.iter().cloned().collect();
        let mut walk: Vec<PathBuf> = known;
        // A scope already covered by a known root adds nothing but a second
        // walk of the same tree.
        let uncovered: Vec<PathBuf> = extra
            .iter()
            .filter(|e| !walk.iter().any(|k| e.starts_with(k)))
            .cloned()
            .collect();
        walk.extend(uncovered);
        roots.extend(self.walk_for(&walk));
        self.retain_unignored(&mut roots);
        roots.into_iter().collect()
    }

    /// Drop the roots `daemon.toml` says to ignore.
    pub fn retain_unignored(&self, roots: &mut std::collections::BTreeSet<PathBuf>) {
        if !self.ignored.is_empty() {
            roots.retain(|root| !super::repo_map::is_ignored(root, &self.ignored));
        }
    }

    /// The stores nested under `walk`, from the cache when it is still valid.
    ///
    /// Valid means two things, and both are needed. The TTL bounds a store
    /// created behind the daemon's back. The `walked` comparison bounds the
    /// other direction: a second client declaring a different workspace asks
    /// about directories the cached walk never entered, and answering it from
    /// that walk would report the first client's repos as the second's.
    ///
    /// That comparison is also the whole invalidation. Recording a repo
    /// changes `known_roots`, which changes `walk`, which misses — so a repo
    /// the daemon itself opens is discoverable on the very next call without
    /// a second mechanism watching `record`. An explicit drop there was
    /// written first and removed: both tests stayed green without it, which
    /// makes it a second path to one transition, not a safety net.
    /// The lock is NOT held across the walk. Two clients that declared
    /// different workspaces would then serialize on a tree neither shares, and
    /// the second would wait out the first's walk before starting its own —
    /// slower than the uncached code it replaces. A concurrent miss walks
    /// twice, exactly as before; the last one to finish wins the slot.
    fn walk_for(&self, walk: &[PathBuf]) -> Vec<PathBuf> {
        let ttl = std::time::Duration::from_secs(self.daemon_config.discovery_cache_secs);
        if ttl.is_zero() {
            return super::repo_map::discover_nested_stores(walk, &self.ignored)
                .into_iter()
                .collect();
        }
        let fresh = |d: &Discovery| d.walked == walk && d.at.elapsed() < ttl;
        if let Some(hit) = self
            .discovery
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|d| fresh(d))
        {
            return hit.roots.clone();
        }
        let roots: Vec<PathBuf> = super::repo_map::discover_nested_stores(walk, &self.ignored)
            .into_iter()
            .collect();
        *self.discovery.lock().unwrap_or_else(|e| e.into_inner()) = Some(Discovery {
            walked: walk.to_vec(),
            roots: roots.clone(),
            at: std::time::Instant::now(),
        });
        roots
    }

    /// Get all active repo handles (for cross-repo operations).
    pub fn all_handles(&self) -> Vec<Arc<RepoHandle>> {
        self.handles
            .iter()
            .map(|entry| Arc::clone(entry.value()))
            .collect()
    }

    /// Open one known root read-only, or say why not. One root, on purpose:
    /// this is what a cross-repo READ opens, immediately before searching that
    /// store and dropping it again, so a fan-out over a hundred stores holds
    /// one connection rather than a hundred.
    ///
    /// [`all_handles`](Self::all_handles) is not what a read fans out over: it
    /// returns the at-most `max_active_repos` handles that happen to be open,
    /// so a repo that is known but closed — every repo, after a restart —
    /// would be absent from the answer without ever being mentioned. That is
    /// the false negative this exists to remove. Which roots to read is the
    /// caller's decision, because the `root` selector already made it.
    ///
    /// [`get_or_open`](Self::get_or_open) is the wrong tool for a read: it
    /// mounts the repo, takes an LRU slot (evicting the repo the caller is
    /// actually working in) and spawns a file watcher. A read needs none of
    /// that, so the store is opened read-only instead — no migration, no
    /// autoheal, no `-wal`/`-shm` pair, nothing left behind. Nothing here
    /// touches the handle table at all, so LRU recency is unchanged.
    ///
    /// The whitelist is re-checked here rather than trusted from the moment the
    /// root was recorded: `daemon.toml` can be edited while the map persists.
    ///
    /// The reasons come from [`repo_map::classify`], so a root the map calls
    /// absent and a root the map calls unreadable are named here exactly as the
    /// map names them. An open that fails past the probe — a schema newer than
    /// this binary, a corrupt file, a held lock — carries the store's own error
    /// text, because "cannot be read" without the reason is what leaves an
    /// operator guessing.
    pub fn read_only_context(&self, root: &Path) -> std::result::Result<Context, ReadRefusal> {
        if let Err(e) = self.daemon_config.check_whitelist(root) {
            return Err(ReadRefusal::Other(format!(
                "outside the daemon whitelist: {e}"
            )));
        }
        match super::repo_map::classify(root) {
            super::repo_map::RootHealth::Healthy => {}
            super::repo_map::RootHealth::Unreadable(why) => return Err(ReadRefusal::Other(why)),
            absent => return Err(ReadRefusal::Other(absent.reason().to_string())),
        }
        Context::open_read_only(root).map_err(|e| match e.kind() {
            ErrorKind::SchemaStale { found, .. } => ReadRefusal::SchemaOutdated { found: *found },
            _ => ReadRefusal::Other(e.to_string()),
        })
    }

    /// The open handle for `canonical`, reloaded first when `config.toml`
    /// changed on disk since that handle read it.
    ///
    /// Story 180-b051: the config used to be read once per handle, so an edit
    /// was silently ignored until LRU eviction or a daemon restart.
    fn open_handle(&self, canonical: &Path) -> Option<Arc<RepoHandle>> {
        let handle = Arc::clone(&*self.handles.get(canonical)?);
        handle.touch();
        if !handle.config_identity.changed() {
            return Some(handle);
        }
        let _gate = self.open_gate.lock().unwrap_or_else(|e| e.into_inner());
        // Another caller may have reloaded it while this one waited.
        let current = Arc::clone(&*self.handles.get(canonical)?);
        if !current.config_identity.changed() {
            return Some(current);
        }
        match current.reopen(&self.daemon_config.priors) {
            Ok(fresh) => {
                let fresh = Arc::new(fresh);
                self.handles
                    .insert(canonical.to_path_buf(), Arc::clone(&fresh));
                drop(_gate);
                tracing::info!("Reloaded config: {}", canonical.display());
                spawn_watcher_for_handle(&fresh);
                Some(fresh)
            }
            Err(e) => {
                if current.set_config_error(e.to_string()) {
                    tracing::warn!(
                        root = %canonical.display(),
                        "config.toml changed but does not load ({e}); still serving the previous config"
                    );
                }
                Some(current)
            }
        }
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
    // Plain, not `\\?\C:\...`: the same key `repo_map::canonical_key` writes,
    // and the spelling the MCP "Specify root" error hands back to the caller.
    crate::domain::canonicalize_plain(&resolved).map_err(|e| {
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

    /// A store under the client's workspace is found even though nothing
    /// registered that workspace.
    ///
    /// A directory that merely HOLDS repositories anchors no store of its own,
    /// so it never enters the map and walking the map never reaches below it.
    /// The log line told the operator "kept as the fan-out scope; stores nested
    /// below it answer" while nothing was looking — and a `root`-less call fell
    /// back to whatever else happened to be open, which is the behaviour the
    /// scope exists to replace.
    #[test]
    fn a_store_under_the_client_scope_is_discoverable_only_when_the_scope_is_passed() {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        let nested = workspace.join("customer-x");
        std::fs::create_dir_all(nested.join(".mdkb")).unwrap();
        // Discovery identifies a store by its SQLite file, not by the folder.
        std::fs::write(nested.join(".mdkb/index.sqlite"), b"").unwrap();

        let registry = RepoRegistry::new(allow_temp_config());

        assert!(
            registry.discoverable_roots().is_empty(),
            "nothing is on the map, so walking the map finds nothing"
        );

        let found = registry.discoverable_roots_under(std::slice::from_ref(&workspace));
        assert_eq!(
            found,
            vec![crate::domain::canonicalize_plain(&nested).unwrap()],
            "with the workspace in hand, the store below it answers"
        );
    }

    /// A scope already covered by a known root is not walked twice.
    #[test]
    fn a_scope_inside_a_known_root_adds_no_second_walk() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let registry = RepoRegistry::new(allow_temp_config());
        registry
            .get_or_open(&root)
            .expect("open puts it on the map");

        let with_child = registry.discoverable_roots_under(&[root.join("sub")]);
        let without = registry.discoverable_roots();
        assert_eq!(
            with_child, without,
            "a scope beneath a root already on the map changes nothing"
        );
    }

    /// A store the walk would find, planted where only a second walk could see
    /// it. Returns the key discovery would report it under — canonical, since
    /// `/var` is a symlink to `/private/var` on macOS and the raw temp path
    /// compares equal to nothing.
    fn plant_store(at: PathBuf) -> PathBuf {
        std::fs::create_dir_all(at.join(".mdkb")).unwrap();
        std::fs::write(at.join(".mdkb/index.sqlite"), b"").unwrap();
        crate::domain::canonicalize_plain(&at).unwrap_or(at)
    }

    fn config_with_ttl(secs: u64) -> DaemonConfig {
        DaemonConfig {
            discovery_cache_secs: secs,
            ..allow_temp_config()
        }
    }

    /// The walk is the cost this cache exists for, so the test asserts the
    /// walk did not happen — by planting a store the walk cannot miss and
    /// showing the answer does not have it.
    #[test]
    fn a_second_discovery_within_the_ttl_does_not_walk_again() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let registry = RepoRegistry::new(config_with_ttl(60));
        registry.get_or_open(&root).expect("open");

        let first = registry.discoverable_roots();
        let nested = plant_store(root.join("nested"));

        assert_eq!(
            registry.discoverable_roots(),
            first,
            "a store planted after the walk cannot be in a cached answer: {}",
            nested.display()
        );
    }

    /// The TTL is what bounds a store created by another process — an
    /// `mdkb init` from the CLI, a clone carrying a committed `.mdkb/`. The
    /// daemon never sees that write, so nothing can invalidate on it.
    #[test]
    fn a_store_created_behind_the_cache_appears_after_the_ttl() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let registry = RepoRegistry::new(config_with_ttl(1));
        registry.get_or_open(&root).expect("open");

        registry.discoverable_roots();
        let nested = plant_store(root.join("nested"));
        std::thread::sleep(std::time::Duration::from_millis(1_100));

        assert!(
            registry.discoverable_roots().contains(&nested),
            "the TTL expired, so the walk must run again"
        );
    }

    /// A repo the daemon itself opens must be discoverable on the very next
    /// call, not a minute later: the daemon knows about that write, so aging
    /// out would be a staleness it chose.
    #[test]
    fn a_repo_the_daemon_opens_is_discoverable_at_once() {
        let tmp = TempDir::new().unwrap();
        let first = make_repo(&tmp);
        let registry = RepoRegistry::new(config_with_ttl(60));
        registry.get_or_open(&first).expect("open first");

        registry.discoverable_roots();
        let nested = plant_store(first.join("nested"));
        let second = tmp.path().join("second");
        std::fs::create_dir_all(&second).unwrap();
        registry.get_or_open(&second).expect("open second");

        assert!(
            registry.discoverable_roots().contains(&nested),
            "registering a repo changes the walk set, so the cached walk cannot answer"
        );
    }

    /// The cache is keyed by the directories that were walked, not by time
    /// alone. Two clients declaring different workspaces ask about different
    /// trees, and answering the second from the first's walk would report one
    /// client's repositories as the other's.
    #[test]
    fn a_different_declared_workspace_is_not_answered_from_the_previous_walk() {
        let tmp = TempDir::new().unwrap();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        let in_alpha = plant_store(alpha.join("one"));
        let in_beta = plant_store(beta.join("two"));
        let registry = RepoRegistry::new(config_with_ttl(60));

        let seen_alpha = registry.discoverable_roots_under(&[alpha]);
        let seen_beta = registry.discoverable_roots_under(&[beta]);

        assert_eq!(seen_alpha, vec![in_alpha.clone()]);
        assert_eq!(seen_beta, vec![in_beta]);
        assert!(!seen_beta.contains(&in_alpha), "no cross-workspace leak");
    }

    #[test]
    fn test_repo_handle_open() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let handle = RepoHandle::open(&root, &toml::Table::new()).unwrap();
        assert_eq!(
            handle.root,
            crate::domain::canonicalize_plain(&root).unwrap()
        );
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
        assert_eq!(
            handle.root,
            crate::domain::canonicalize_plain(&root).unwrap()
        );
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
        assert_eq!(
            entries[0].0,
            crate::domain::canonicalize_plain(&root).unwrap()
        );
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

    /// A daemon home for the repo map. Without one the map keeps its set in
    /// memory and writes nothing, which is what every other test in this file
    /// relies on to stay clear of the real `~/.mdkb`.
    fn allow_temp_config_with_state(state: &Path) -> DaemonConfig {
        DaemonConfig {
            state_dir: Some(state.to_path_buf()),
            ..allow_temp_config()
        }
    }

    /// AC#1 — a repo opened by one process is known to the next. Two
    /// registries over one daemon home stand in for a restart: the first
    /// process is gone, only `repos.json` connects them.
    #[test]
    fn a_repo_opened_once_is_known_after_a_restart() {
        let state = TempDir::new().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);

        let before = RepoRegistry::new(allow_temp_config_with_state(state.path()));
        before.get_or_open(&root).unwrap();
        assert_eq!(
            before.known_roots(),
            vec![crate::domain::canonicalize_plain(&root).unwrap()]
        );
        drop(before);

        let restarted = RepoRegistry::new(allow_temp_config_with_state(state.path()));
        assert_eq!(
            restarted.known_roots(),
            vec![crate::domain::canonicalize_plain(&root).unwrap()],
            "the map is what one daemon leaves behind for the next"
        );
        assert_eq!(
            restarted.active_count(),
            0,
            "knowing a repo is not holding it open: the handle LRU still starts empty"
        );
    }

    /// AC#1 — the record happens after the whitelist check, so a refused root
    /// never enters the map. A map that recorded attempts would hand the next
    /// daemon a list of roots it was never allowed to touch.
    #[test]
    fn a_root_refused_by_the_whitelist_is_never_recorded() {
        let state = TempDir::new().unwrap();
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);

        let registry = RepoRegistry::new(DaemonConfig {
            whitelist_dirs: vec!["/nonexistent/allowed".to_string()],
            state_dir: Some(state.path().to_path_buf()),
            ..DaemonConfig::default()
        });

        assert!(registry.get_or_open(&root).is_err());
        assert!(registry.known_roots().is_empty());
        assert!(
            !state.path().join("repos.json").exists(),
            "a refused open writes nothing at all"
        );
    }

    /// AC#6 — knowing a root is not permission to open it. A root persisted by
    /// an earlier daemon, under a whitelist that has since been narrowed, is
    /// still refused: the map feeds `get_or_open`, it does not bypass it.
    #[test]
    fn a_persisted_root_outside_the_whitelist_is_still_refused() {
        let state = TempDir::new().unwrap();
        let tmp = TempDir::new().unwrap();
        let outside = make_repo(&tmp);
        std::fs::write(
            state.path().join("repos.json"),
            serde_json::json!({
                "version": 1,
                "repos": [{ "root": outside.to_string_lossy() }],
            })
            .to_string(),
        )
        .unwrap();

        let registry = RepoRegistry::new(DaemonConfig {
            whitelist_dirs: vec!["/nonexistent/allowed".to_string()],
            state_dir: Some(state.path().to_path_buf()),
            ..DaemonConfig::default()
        });

        assert_eq!(
            registry.known_roots(),
            vec![crate::domain::canonicalize_plain(&outside).unwrap()],
            "the root is known"
        );
        let err = registry.get_or_open(&outside).unwrap_err().to_string();
        assert!(err.contains("whitelist"), "and still refused: {err}");
        assert_eq!(registry.active_count(), 0);
    }

    /// AC#8 — the map is not the active-handle LRU. A root evicted from
    /// `handles` stays known.
    #[test]
    fn a_root_evicted_from_the_handle_lru_stays_on_the_map() {
        let state = TempDir::new().unwrap();
        let tmps: Vec<TempDir> = (0..3).map(|_| TempDir::new().unwrap()).collect();
        let roots: Vec<PathBuf> = tmps.iter().map(make_repo).collect();

        let registry = RepoRegistry::new(DaemonConfig {
            max_active_repos: 2,
            ..allow_temp_config_with_state(state.path())
        });
        for root in &roots {
            registry.get_or_open(root).unwrap();
        }

        assert_eq!(registry.active_count(), 2, "the handle cap is unchanged");
        assert_eq!(
            registry.known_roots().len(),
            3,
            "all three are still known: eviction frees resources, it does not forget a repo"
        );
    }

    /// AC#5 — `daemon.toml` is hand-owned config. Its `[[repos]]` are unioned
    /// into the map at startup and the file itself is left byte for byte alone.
    #[test]
    fn daemon_toml_repos_join_the_map_and_the_file_is_never_rewritten() {
        let state = TempDir::new().unwrap();
        let tmp_configured = TempDir::new().unwrap();
        let tmp_opened = TempDir::new().unwrap();
        let configured = make_repo(&tmp_configured);
        let opened = make_repo(&tmp_opened);

        let config_path = state.path().join("daemon.toml");
        let written = format!(
            // TOML literal strings: a Windows path is full of backslashes, and
            // `\U` in a basic string is an invalid escape, not a path.
            "max_active_repos = 4\nwhitelist_dirs = ['{}']\n\n[[repos]]\nroot = '{}'\n",
            std::env::temp_dir().to_string_lossy(),
            configured.to_string_lossy(),
        );
        std::fs::write(&config_path, &written).unwrap();

        let config = DaemonConfig::load_or_default(&config_path).unwrap();
        let registry = RepoRegistry::new(config);
        registry.get_or_open(&opened).unwrap();

        let mut expected = vec![
            crate::domain::canonicalize_plain(&configured).unwrap(),
            crate::domain::canonicalize_plain(&opened).unwrap(),
        ];
        expected.sort();
        assert_eq!(registry.known_roots(), expected);
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            written,
            "the daemon must never rewrite the operator's config"
        );
    }

    #[test]
    fn test_registry_path_canonicalization() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);

        let registry = RepoRegistry::new(allow_temp_config());

        // Open with raw path
        registry.get_or_open(&root).unwrap();

        // Access with canonicalized path should return same handle
        let canonical = crate::domain::canonicalize_plain(&root).unwrap();
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
        assert_eq!(
            handle.root,
            crate::domain::canonicalize_plain(&main_root).unwrap()
        );

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

    /// Story 180-b051. Measured 2026-09-29: with the handle open, turning
    /// `user_prompt_submit_shadow` on was ignored for 12 s across three prompts,
    /// because the config was read once at open and never again.
    #[test]
    fn a_config_edit_is_served_on_the_next_access() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let registry = RepoRegistry::new(allow_temp_config());

        let before = registry.get_or_open(&root).unwrap();
        assert!(!before.config.hooks.user_prompt_submit_shadow);

        std::fs::write(
            root.join(".mdkb/config.toml"),
            "[hooks]\nuser_prompt_submit_shadow = true\n",
        )
        .unwrap();

        let after = registry.get_or_open(&root).unwrap();
        assert!(
            after.config.hooks.user_prompt_submit_shadow,
            "the edit must be served without a restart"
        );
        assert!(
            Arc::ptr_eq(&before.ctx, &after.ctx),
            "a reload must share the open store, not open a second connection"
        );
        assert!(Arc::ptr_eq(&before.code_index, &after.code_index));
        assert_eq!(registry.active_count(), 1);
    }

    /// A typo in the file must not reset a tuned repo to factory defaults
    /// mid-session: cold open falls back to defaults, a reload keeps the last
    /// good config and says why.
    #[test]
    fn an_invalid_config_keeps_the_handle_it_replaces() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        let path = root.join(".mdkb/config.toml");
        std::fs::write(&path, "[hooks]\nuser_prompt_submit_shadow = true\n").unwrap();
        let registry = RepoRegistry::new(allow_temp_config());
        let good = registry.get_or_open(&root).unwrap();
        assert!(good.config_error().is_none());

        std::fs::write(&path, "[hooks\nuser_prompt_submit_shadow = tru\n").unwrap();
        let kept = registry.get_or_open(&root).unwrap();
        assert!(Arc::ptr_eq(&good, &kept));
        assert!(kept.config.hooks.user_prompt_submit_shadow);
        assert!(
            kept.config_error().is_some(),
            "the parse error is kept for doctor"
        );

        // The failed parse did not advance the identity: fixing the file is
        // picked up on the next access, and the error clears with it.
        std::fs::write(&path, "[hooks]\nuser_prompt_submit_shadow = false\n").unwrap();
        let fixed = registry.get_or_open(&root).unwrap();
        assert!(!Arc::ptr_eq(&good, &fixed));
        assert!(!fixed.config.hooks.user_prompt_submit_shadow);
        assert!(fixed.config_error().is_none());
    }

    #[test]
    fn an_unchanged_config_reuses_the_handle() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(&tmp);
        std::fs::write(
            root.join(".mdkb/config.toml"),
            "[hooks]\nrecall_limit = 3\n",
        )
        .unwrap();
        let registry = RepoRegistry::new(allow_temp_config());
        let a = registry.get_or_open(&root).unwrap();
        let b = registry.get_or_open(&root).unwrap();
        let c = registry.get(&root).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&a, &c));
    }
}
