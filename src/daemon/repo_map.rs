//! The set of repositories the daemon knows, persisted across restarts.
//!
//! `RepoRegistry.handles` is an LRU of *open* repos: it holds at most
//! `max_active_repos` and starts empty in every process, so after a restart the
//! daemon knows nothing until a client knocks. This map is the other half —
//! every root that ever opened, kept whether or not a handle is live. Evicting
//! a handle does not forget the repo.
//!
//! The file is daemon-owned STATE and lives beside `daemon.toml` in the daemon
//! home. `daemon.toml` is hand-owned CONFIG: its `[[repos]]` are read at
//! startup and unioned into the map, and nothing here ever writes that file.
//!
//! Concurrency: the daemon and every in-process CLI hook can hold a map at the
//! same time. Each writes the whole set through temp+rename, so no reader ever
//! sees a half-written file; a writer holding an older set can still drop a
//! root another process has just recorded. That loss costs one re-record on the
//! next `get_or_open` of the root, which is why no cross-process lock is taken.

use ignore::WalkBuilder;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::config::{DaemonConfig, RepoEntry};

/// On-disk format of `repos.json`. Bumped when the shape changes; an unknown
/// version is read anyway (the payload is a list of paths) but reported, so an
/// older binary reading a newer file says so instead of failing silently.
const FORMAT_VERSION: u32 = 2;

const DAY_SECS: i64 = 86_400;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The serialized map extends [`RepoEntry`] with daemon-owned activity metadata.
#[derive(Debug, Serialize, Deserialize)]
struct RepoMapFile {
    version: u32,
    repos: Vec<MapEntry>,
}

/// Additive v2 metadata: v1 binaries can still deserialize every root and scope.
#[derive(Debug, Serialize, Deserialize)]
struct MapEntry {
    #[serde(flatten)]
    repo: RepoEntry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_opened_at: Option<i64>,
}

/// Remove only healthy, unpinned roots whose inactivity exceeds the limit.
fn expire(
    roots: &mut BTreeMap<PathBuf, i64>,
    pinned: &BTreeSet<PathBuf>,
    expiry_days: u64,
    at: i64,
) -> bool {
    let before = roots.len();
    if expiry_days > 0 {
        roots.retain(|root, last| {
            let expired = at.saturating_sub(*last) >
                i64::try_from(expiry_days).unwrap_or(i64::MAX).saturating_mul(DAY_SECS);
            if expired && !pinned.contains(root) && classify(root) == RootHealth::Healthy {
                tracing::info!(root = %root.display(), reason = "inactivity exceeded repo_expiry_days",
                    last_opened_at = *last, expiry_days, "Dropped repo from the map");
                false
            } else {
                true
            }
        });
    }
    before != roots.len()
}

/// What a known root looks like on disk right now.
///
/// The distinction between [`RootHealth::NoStore`] and
/// [`RootHealth::Unreadable`] is the whole point of the type: a store that
/// cannot be read is a property of this binary, of a lock, or of the file
/// permissions — never evidence that the repo was deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootHealth {
    /// The root is there and its store can be listed.
    Healthy,
    /// Nothing exists at that path any more.
    Gone,
    /// The path exists but holds no `.mdkb` store.
    NoStore,
    /// The store is there and this process cannot read it. Carries the reason.
    Unreadable(String),
}

impl RootHealth {
    /// Does this state mean the repo is gone, as opposed to unreachable?
    ///
    /// Only the two states that prove absence drop a root. Everything else —
    /// including a store this binary cannot open — is kept.
    fn is_absence(&self) -> bool {
        matches!(self, RootHealth::Gone | RootHealth::NoStore)
    }

    /// Reason text for the removal log, so a pruned root is never dropped in
    /// silence. Also what a cross-repo read reports for a root it skipped: one
    /// vocabulary for "this root was not read", wherever it is said.
    pub fn reason(&self) -> &'static str {
        match self {
            RootHealth::Healthy => "healthy",
            RootHealth::Gone => "the root is gone from disk",
            RootHealth::NoStore => "the root holds no .mdkb store",
            RootHealth::Unreadable(_) => "the store cannot be read",
        }
    }
}

/// Classify a known root without opening its database.
///
/// Opening every known store at startup would cost a SQLite open per repo and
/// would turn a locked store into a startup stall, so the probe stops at the
/// filesystem: exists, holds `.mdkb`, and `.mdkb` can be listed. A store that
/// is corrupt or carries a newer schema passes this probe and is kept — it
/// fails later, at the open that actually needs it, and that failure never
/// removes it from the map.
pub fn classify(root: &Path) -> RootHealth {
    match std::fs::symlink_metadata(root) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RootHealth::Gone,
        Err(e) => return RootHealth::Unreadable(e.to_string()),
    }

    let store = root.join(".mdkb");
    match std::fs::metadata(&store) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return RootHealth::NoStore,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RootHealth::NoStore,
        Err(e) => return RootHealth::Unreadable(e.to_string()),
    }

    match std::fs::read_dir(&store) {
        Ok(_) => RootHealth::Healthy,
        Err(e) => RootHealth::Unreadable(e.to_string()),
    }
}

/// Directory names the discovery walk never enters. `.tmp` holds scratch
/// copies of repos (coverage and mutation runs), which are not repos.
const SKIPPED_DIRS: [&str; 5] = [".git", "target", "node_modules", ".mdkb", ".tmp"];

/// Generated trees and managed worktree copies are not discovery candidates.
/// Explicit known roots beneath them are still walked independently.
fn is_generated_dir(name: &str) -> bool {
    matches!(name, "build" | "dist" | "coverage" | "__pycache__") || name.ends_with("__wt")
}

/// Is `path` at or below one of the `ignore` paths?
pub fn is_ignored(path: &Path, ignore: &[PathBuf]) -> bool {
    ignore.iter().any(|ignored| path.starts_with(ignored))
}

#[cfg(test)]
pub(super) static TEST_VISITS: std::sync::LazyLock<Mutex<BTreeMap<PathBuf, usize>>> =
    std::sync::LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Normalize walk coverage, not repository identity: an explicit worktree is
/// still a directory to explore, even when its store belongs to the main repo.
pub(super) fn covered_walk_roots(roots: &[PathBuf]) -> Vec<PathBuf> {
    let sorted: BTreeSet<_> = roots
        .iter()
        .map(|root| crate::domain::canonicalize_plain(root).unwrap_or_else(|_| root.clone()))
        .collect();
    let mut covered: Vec<PathBuf> = Vec::new();
    for root in sorted {
        let already_covered = covered.iter().any(|ancestor| {
            root.strip_prefix(ancestor).is_ok_and(|relative| {
                // An explicitly requested root below a pruned directory is not
                // reached by the ancestor's walk; retain that separate walk.
                !relative.components().any(|part| {
                    let name = part.as_os_str().to_string_lossy();
                    SKIPPED_DIRS.contains(&name.as_ref()) || is_generated_dir(&name)
                }) && !root
                    .ancestors()
                    .skip(1)
                    .take_while(|path| path.starts_with(ancestor))
                    .any(|path| {
                        // An explicit known root may live below an ignore rule.
                        // Without evaluating the rule here, conservatively keep
                        // its independent walk. The hot-path check is metadata
                        // only, not a second traversal or a Git subprocess.
                        path.join(".gitignore").is_file() || path.join(".ignore").is_file()
                    })
            })
        });
        if !already_covered {
            covered.push(root);
        }
    }
    covered
}

/// Find stores below roots the daemon already knows, without opening or
/// registering them. A store is identified by its SQLite file; walking stops
/// at `.mdkb` so index internals are never traversed, and at every directory
/// in [`SKIPPED_DIRS`], generated/worktree trees, local ignore rules, or an
/// explicit `ignore` path. Explicit walk roots do not inherit parent ignore
/// files: a registered store remains reachable even below an excluded tree.
pub fn discover_nested_stores(roots: &[PathBuf], ignore: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    for root in covered_walk_roots(roots) {
        let ignored = ignore.to_vec();
        let mut builder = WalkBuilder::new(root);
        builder
            .hidden(false)
            .parents(false)
            .git_global(false)
            .git_exclude(false)
            .require_git(false)
            .follow_links(false)
            .filter_entry(move |entry| {
                let name = entry.file_name().to_string_lossy();
                !SKIPPED_DIRS.contains(&name.as_ref())
                    && (entry.depth() == 0 || !is_generated_dir(&name))
                    && !is_ignored(entry.path(), &ignored)
            });
        for entry in builder.build().filter_map(Result::ok) {
            if !entry.file_type().is_some_and(|kind| kind.is_dir()) {
                continue;
            }
            #[cfg(test)]
            {
                *TEST_VISITS
                    .lock()
                    .unwrap()
                    .entry(entry.path().to_path_buf())
                    .or_default() += 1;
            }
            let candidate = entry.path().join(".mdkb/index.sqlite");
            if candidate.is_file() {
                let key = canonical_key(entry.path());
                // The walk compares the path as it was reached; the key is the
                // canonical spelling the ignore entries are written in.
                if !is_ignored(&key, ignore) {
                    found.insert(key);
                }
            }
        }
    }
    found
}

/// Read the persisted roots without normalization or a write-back, leaving out
/// the ones that are gone from disk: an entry kept in the file only for its
/// hand-written scope is not a repo. Reporting commands use this path so
/// inspecting coverage cannot change it.
pub fn read_known_roots(path: &Path) -> Vec<PathBuf> {
    try_read_known_roots(path).unwrap_or_else(|why| {
        tracing::warn!(path = %path.display(), "Reading the repo map failed: {why}");
        Vec::new()
    })
}

/// [`read_known_roots`] that says when it could not read: a file that is absent
/// has no roots, one that is unreadable or not valid JSON is an error. Unlike
/// the map's own load it never moves a file aside — only the writer may.
pub fn try_read_known_roots(path: &Path) -> Result<Vec<PathBuf>, String> {
    let read = parse_file(path).map_err(|e| e.to_string())?;
    let config_path = path.with_file_name("daemon.toml");
    let config = DaemonConfig::load_or_default(&config_path).map_err(|e| e.to_string())?;
    let mut pinned: BTreeSet<_> = config
        .repos
        .iter()
        .map(|r| canonical_key(Path::new(&r.root)))
        .collect();
    pinned.extend(try_read_scope_overrides(path)?.into_keys());
    let mut roots: BTreeMap<_, _> = triage(canonical_set(&read.roots))
        .kept
        .into_iter()
        .map(|root| {
            let at = read.last_opened.get(&root).copied().unwrap_or_else(now);
            (root, at)
        })
        .collect();
    expire(&mut roots, &pinned, config.repo_expiry_days, now());
    Ok(roots.into_keys().collect())
}

/// Persisted activity for text listings; missing legacy dates remain unknown.
pub fn read_last_opened(path: &Path) -> Result<BTreeMap<PathBuf, i64>, String> {
    parse_file(path)
        .map(|read| read.last_opened)
        .map_err(|e| e.to_string())
}

/// The spelling every root is stored under (see [`canonical_key`]): one repo,
/// one entry, however the file wrote it. Shared by the map's load and by the
/// read-only reader so the two list the same roots.
fn canonical_set(roots: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    roots.iter().map(|r| canonical_key(r)).collect()
}

/// The outcome of one pass over the known roots.
///
/// Returned rather than logged in place so the fate of every root is data a
/// test can assert, not a line in a log nobody reads.
#[derive(Debug, Default)]
struct Triage {
    /// The roots the map keeps.
    kept: BTreeSet<PathBuf>,
    /// The roots removed, each with the reason it is logged under.
    dropped: Vec<(PathBuf, RootHealth)>,
    /// Kept, but unreadable in this process right now: reported, never removed.
    unreachable: Vec<(PathBuf, String)>,
}

/// The key a root is stored under: the same one `RepoRegistry` derives for its
/// handle table, so one repo cannot occupy two entries under two spellings —
/// a `daemon.toml` naming a symlink or a git worktree, and a `get_or_open`
/// naming the resolved root, are the same repo.
///
/// A path that resolves to nothing is kept verbatim: it is about to be
/// classified [`RootHealth::Gone`] and dropped, and the spelling the operator
/// wrote is the one that belongs in that log line.
pub(super) fn canonical_key(root: &Path) -> PathBuf {
    // An empty persisted root is absent, not the current worktree: joining
    // `.git` to it would read the caller's worktree pointer from the cwd.
    if root.as_os_str().is_empty() {
        return root.to_path_buf();
    }
    let resolved = crate::git::resolve_main_worktree(root);
    // `canonicalize` returns `\\?\C:\...` on Windows, which names the same file
    // and compares equal to nothing. Every key written that way would be a
    // second entry for a repo already on the map.
    crate::domain::canonicalize_plain(&resolved).unwrap_or(resolved)
}

/// Split known roots into the ones the map keeps and the ones it drops.
fn triage(roots: BTreeSet<PathBuf>) -> Triage {
    let mut out = Triage::default();
    for root in roots {
        let health = classify(&root);
        if health.is_absence() {
            out.dropped.push((root, health));
            continue;
        }
        if let RootHealth::Unreadable(why) = health {
            out.unreachable.push((root.clone(), why));
        }
        out.kept.insert(root);
    }
    out
}

/// The `scope` each repo carries in the persisted map, keyed by canonical root.
/// Read without triage or a write-back, like [`read_known_roots`]; a file that
/// cannot be read or parsed has none.
pub fn read_scope_overrides(path: &Path) -> BTreeMap<PathBuf, String> {
    try_read_scope_overrides(path).unwrap_or_else(|why| {
        tracing::warn!(path = %path.display(), "Reading scopes from the repo map failed: {why}");
        BTreeMap::new()
    })
}

/// [`read_scope_overrides`] that says when it could not read: a file that is
/// absent has no scopes, one that is unreadable or not valid JSON (a hand edit
/// in progress) is an error, so a caller can keep what it last knew.
pub fn try_read_scope_overrides(path: &Path) -> Result<BTreeMap<PathBuf, String>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.to_string()),
    };
    let file = serde_json::from_str::<RepoMapFile>(&content).map_err(|e| e.to_string())?;
    Ok(file
        .repos
        .into_iter()
        .filter_map(|r| {
            Some((
                canonical_key(Path::new(&r.repo.root)),
                clean_scope(&r.repo.scope?)?,
            ))
        })
        .collect())
}

/// A scope name as declared: trimmed, and blank means none was declared, so
/// the prefix rule decides. `daemon.toml` rules are read the same way.
pub(super) fn clean_scope(scope: &str) -> Option<String> {
    Some(scope.trim().to_string()).filter(|s| !s.is_empty())
}

/// The persisted set of known repository roots.
pub struct RepoMap {
    /// Where the set is persisted. `None` for a config with no daemon home
    /// behind it: nothing on disk backs it, so nothing is written on its behalf.
    path: Option<PathBuf>,
    /// Held across the file write so two recorders cannot interleave a set with
    /// a rename and persist a map that never existed in memory.
    roots: Mutex<BTreeMap<PathBuf, i64>>,
    pinned: BTreeSet<PathBuf>,
    expiry_days: u64,
    /// May this process overwrite what is on disk?
    ///
    /// False when the file exists and could not be understood. Re-discovery is
    /// cheap; a map replaced by an empty one because this binary could not
    /// parse it is a set of repos nobody can get back.
    replaceable: bool,
    /// A persist was skipped or failed, so the file lags the set in memory.
    /// `record` retries while this is set, even for a root already known: the
    /// root that was recorded during a bad moment is otherwise never written.
    dirty: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for RepoMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoMap")
            .field("path", &self.path)
            .field("known", &self.roots.lock().map(|r| r.len()).unwrap_or(0))
            .field("replaceable", &self.replaceable)
            .finish_non_exhaustive()
    }
}

impl RepoMap {
    /// Load the map at `path`, union the `seeds` from `daemon.toml`, drop the
    /// roots that are gone, and persist the result if any of that changed it.
    ///
    /// A seed that no longer exists is dropped from the map but stays in
    /// `daemon.toml`, which this never rewrites: the operator's list is theirs.
    pub fn open(path: Option<PathBuf>, seeds: &[RepoEntry]) -> Self {
        Self::open_with_expiry(path, seeds, DaemonConfig::default().repo_expiry_days)
    }

    /// Load with the configured inactivity limit; zero disables age-based expiry.
    pub fn open_with_expiry(path: Option<PathBuf>, seeds: &[RepoEntry], expiry_days: u64) -> Self {
        Self::open_at(path, seeds, expiry_days, now())
    }

    fn open_at(path: Option<PathBuf>, seeds: &[RepoEntry], expiry_days: u64, at: i64) -> Self {
        let read = match &path {
            Some(p) => read_file(p),
            None => MapRead {
                roots: BTreeSet::new(),
                last_opened: BTreeMap::new(),
                replaceable: true,
            },
        };
        let loaded = read.roots;
        let before = loaded.len();

        let mut union = canonical_set(&loaded);
        // A file whose entries do not survive normalization is rewritten below,
        // so two spellings of one repo collapse to one the first time a daemon
        // reads them.
        let normalized = union != loaded;
        for seed in seeds {
            union.insert(canonical_key(Path::new(&seed.root)));
        }
        let seeded = union.len();

        let triaged = triage(union);
        for (root, health) in &triaged.dropped {
            tracing::info!(
                root = %root.display(),
                reason = health.reason(),
                "Dropped repo from the map"
            );
        }
        for (root, why) in &triaged.unreachable {
            tracing::warn!(
                root = %root.display(),
                "Known repo cannot be read ({why}); kept on the map — an unreadable store is not a deleted repo"
            );
        }

        // A dropped entry that carries a hand-written scope stays in the file
        // (see `write_atomic`), so dropping it is not a change to write.
        let declared = path
            .as_deref()
            .and_then(|p| try_read_scope_overrides(p).ok())
            .unwrap_or_default();
        let dropped_for_good = triaged
            .dropped
            .iter()
            .any(|(root, _)| !declared.contains_key(root));

        let pinned: BTreeSet<_> = seeds
            .iter()
            .map(|seed| canonical_key(Path::new(&seed.root)))
            .collect();
        let mut protected = pinned.clone();
        protected.extend(declared.into_keys());
        let migrated = triaged
            .kept
            .iter()
            .any(|r| !read.last_opened.contains_key(r));
        let mut roots: BTreeMap<_, _> = triaged
            .kept
            .into_iter()
            .map(|root| {
                let last = read.last_opened.get(&root).copied().unwrap_or(at);
                (root, last)
            })
            .collect();
        let expired = expire(&mut roots, &protected, expiry_days, at);
        let map = Self {
            path,
            roots: Mutex::new(roots),
            pinned,
            expiry_days,
            replaceable: read.replaceable,
            dirty: std::sync::atomic::AtomicBool::new(false),
        };
        // Persist only when the set on disk is not the set in hand: every CLI
        // hook builds a registry, and rewriting an unchanged map on each one
        // would rename a file per hook for nothing.
        let changed = normalized || seeded != before || dropped_for_good || migrated || expired;
        if changed {
            let mut roots = map.roots.lock().unwrap_or_else(|e| e.into_inner());
            map.persist(&mut roots);
        }
        map
    }

    /// Record a successful access, including an already-live registry handle.
    /// Persist at most once per UTC day per root, unless a failed write needs
    /// retrying. Same-day access can therefore lag by less than 24 hours.
    pub fn record(&self, root: &Path) {
        self.record_at(root, now());
    }

    fn record_at(&self, root: &Path, at: i64) {
        let key = canonical_key(root);
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        let last = roots.get(&key).copied();
        let changed = last.is_none_or(|last| at.div_euclid(DAY_SECS) > last.div_euclid(DAY_SECS));
        if changed {
            roots.insert(key.clone(), at);
            if last.is_none() {
                tracing::info!(root = %key.display(), "Recorded repo on the map");
            }
        } else if !self.dirty.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        self.persist(&mut roots);
    }

    /// Is this root known?
    pub fn contains(&self, root: &Path) -> bool {
        self.roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&canonical_key(root))
    }

    /// Every known root, sorted.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots_at(now())
    }

    fn roots_at(&self, at: i64) -> Vec<PathBuf> {
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        // A hook can refresh activity after this daemon loaded its snapshot.
        if self.merge_persisted_activity(&mut roots).is_err() {
            return roots.keys().cloned().collect();
        }
        let mut pinned = self.pinned.clone();
        if let Some(path) = &self.path {
            match try_read_scope_overrides(path) {
                Ok(scopes) => pinned.extend(scopes.into_keys()),
                // A hand edit in progress is not permission to expire its entries.
                Err(_) => return roots.keys().cloned().collect(),
            }
        }
        if expire(&mut roots, &pinned, self.expiry_days, at) {
            self.persist(&mut roots);
        }
        roots.keys().cloned().collect()
    }

    /// Snapshot activity after reconciling successful opens from other processes.
    pub(super) fn last_opened(&self) -> BTreeMap<PathBuf, i64> {
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(why) = self.merge_persisted_activity(&mut roots) {
            tracing::warn!("Reading repo activity failed: {why}");
        }
        roots.clone()
    }

    /// Merge activity for retained roots without restoring an expired root.
    fn merge_persisted_activity(&self, roots: &mut BTreeMap<PathBuf, i64>) -> Result<(), String> {
        if let Some(path) = &self.path {
            let opened = read_last_opened(path)?;
            for (root, last) in roots {
                if let Some(persisted) = opened.get(root) {
                    *last = (*last).max(*persisted);
                }
            }
        }
        Ok(())
    }

    /// Write the whole set, atomically. A failure is reported and swallowed:
    /// losing the map costs a re-discovery, failing the open that triggered it
    /// costs the caller their tool call.
    fn persist(&self, roots: &mut BTreeMap<PathBuf, i64>) {
        let Some(path) = &self.path else {
            return;
        };
        if !self.replaceable {
            tracing::warn!(
                path = %path.display(),
                "Not overwriting a repo map this binary could not read; the set is correct in memory only"
            );
            return;
        }
        // The scopes are written by hand, and this process never records one:
        // read them back from the file so a rewrite for a new root cannot erase
        // them. A file that cannot be read is a hand edit in progress: writing
        // over it would destroy that edit and every scope in it, so the write
        // waits for the next persist, which finds the file whole.
        let scopes = match try_read_scope_overrides(path) {
            Ok(scopes) => scopes,
            Err(why) => {
                tracing::warn!(
                    path = %path.display(),
                    "Not persisting the repo map: it cannot be read right now ({why}); the set is correct in memory only"
                );
                self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        // Never roll back another live map's activity in a whole-map write.
        if let Err(why) = self.merge_persisted_activity(roots) {
            tracing::warn!(path = %path.display(), "Not persisting repo activity: {why}");
            self.dirty.store(true, std::sync::atomic::Ordering::Relaxed);
            return;
        }
        let failed = write_atomic(path, roots, &scopes).map_err(|e| {
            tracing::warn!(
                path = %path.display(),
                "Could not persist the repo map: {e} — the set is still correct in memory"
            );
        });
        self.dirty
            .store(failed.is_err(), std::sync::atomic::Ordering::Relaxed);
    }
}

/// What a read of the persisted map produced.
///
/// The distinction that matters is not "empty or not" but "may this be
/// overwritten". A map that is absent may: there is nothing to lose. A map
/// that exists and could not be understood may not — the next `persist` would
/// replace a recoverable file with whatever this process happens to know,
/// which for a daemon that just started is nothing at all.
struct MapRead {
    roots: BTreeSet<PathBuf>,
    last_opened: BTreeMap<PathBuf, i64>,
    /// False when the file exists but could not be read, parsed, or is newer
    /// than this binary understands.
    replaceable: bool,
}

/// Why a map file could not be read.
enum Unreadable {
    Io(String),
    Parse(String),
}

impl std::fmt::Display for Unreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot be read: {e}"),
            Self::Parse(e) => write!(f, "is not valid JSON: {e}"),
        }
    }
}

/// Parse the persisted set and touch nothing: no rename, no write. A file that
/// is absent is an empty, replaceable map.
fn parse_file(path: &Path) -> Result<MapRead, Unreadable> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(MapRead {
                roots: BTreeSet::new(),
                last_opened: BTreeMap::new(),
                replaceable: true,
            });
        }
        Err(e) => return Err(Unreadable::Io(e.to_string())),
    };
    let file = serde_json::from_str::<RepoMapFile>(&content)
        .map_err(|e| Unreadable::Parse(e.to_string()))?;
    let mut last_opened: BTreeMap<PathBuf, i64> = BTreeMap::new();
    for entry in &file.repos {
        if let Some(at) = entry.last_opened_at {
            let key = canonical_key(Path::new(&entry.repo.root));
            last_opened
                .entry(key)
                .and_modify(|last| *last = (*last).max(at))
                .or_insert(at);
        }
    }
    let roots = file
        .repos
        .into_iter()
        .map(|r| PathBuf::from(r.repo.root))
        .collect();
    if file.version > FORMAT_VERSION {
        // Writing our own FORMAT_VERSION over this would silently downgrade a
        // map a newer binary owns, dropping whatever that format carries which
        // this one cannot represent.
        tracing::warn!(
            path = %path.display(),
            found = file.version,
            known = FORMAT_VERSION,
            "Repo map was written by a newer mdkb; reading it, and leaving it alone"
        );
        return Ok(MapRead {
            roots,
            last_opened,
            replaceable: false,
        });
    }
    if file.version != FORMAT_VERSION {
        tracing::warn!(
            path = %path.display(),
            found = file.version,
            known = FORMAT_VERSION,
            "Repo map written by an older format version; reading it anyway"
        );
    }
    Ok(MapRead {
        roots,
        last_opened,
        replaceable: true,
    })
}

/// Read the persisted set for the map's own load. Any failure yields an empty
/// set and a warning: a map that cannot be parsed must not take the daemon down
/// with it. This is the only reader that moves a corrupt file aside.
fn read_file(path: &Path) -> MapRead {
    let empty = |replaceable| MapRead {
        roots: BTreeSet::new(),
        last_opened: BTreeMap::new(),
        replaceable,
    };
    match parse_file(path) {
        Ok(read) => read,
        Err(Unreadable::Io(e)) => {
            tracing::warn!(
                path = %path.display(),
                "Could not read the repo map: {e} — keeping the file, not replacing it"
            );
            empty(false)
        }
        Err(Unreadable::Parse(e)) => {
            // Quarantine rather than overwrite, and rather than refuse to
            // write. Overwriting loses the only copy of a set somebody may
            // need; refusing leaves the map broken for every later process,
            // which is what the daemon has a map for in the first place.
            // Moving it aside does neither: the bytes survive under a name
            // nothing reads, and the next record rebuilds a good file.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let aside = path.with_extension(format!("json.corrupt-{stamp}"));
            match std::fs::rename(path, &aside) {
                Ok(()) => {
                    tracing::warn!(
                        path = %path.display(),
                        quarantined = %aside.display(),
                        "Repo map is not valid JSON ({e}); moved aside, rebuilding from discovery"
                    );
                    empty(true)
                }
                Err(move_err) => {
                    tracing::warn!(
                        path = %path.display(),
                        "Repo map is not valid JSON ({e}) and could not be moved aside ({move_err}); leaving it untouched"
                    );
                    empty(false)
                }
            }
        }
    }
}

/// Write `roots` to `path` through a temp file and a rename.
///
/// The rename is what makes the map crash-safe: the destination is replaced by
/// a complete file or not at all, so no crash can leave a truncated map behind.
/// The temp file is fsynced first, so the rename cannot publish a name whose
/// contents are still in the page cache.
fn write_atomic(
    path: &Path,
    roots: &BTreeMap<PathBuf, i64>,
    scopes: &BTreeMap<PathBuf, String>,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // An entry with a hand-written scope stays even when its root is not in
    // the set: a store that is missing (an unmounted volume, a directory that
    // holds no `.mdkb`) is not a reason to erase what the operator declared.
    let kept: BTreeSet<&PathBuf> = roots.keys().chain(scopes.keys()).collect();
    let file = RepoMapFile {
        version: FORMAT_VERSION,
        repos: kept
            .into_iter()
            .map(|r| MapEntry {
                repo: RepoEntry {
                    root: r.to_string_lossy().to_string(),
                    scope: scopes.get(r).cloned(),
                },
                last_opened_at: roots.get(r).copied(),
            })
            .collect(),
    };
    let json = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;

    // The temp name carries the pid. A single shared `repos.json.tmp` is not a
    // private scratch file: two processes writing at once both create and
    // truncate it, interleave their bytes, and one of them renames the result
    // over the map. The rename is atomic; picking the name was not.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let write = (|| -> std::io::Result<()> {
        let mut handle = std::fs::File::create(&tmp)?;
        handle.write_all(json.as_bytes())?;
        handle.write_all(b"\n")?;
        handle.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if write.is_err() {
        // A failed write must not leave its scratch file behind for every
        // later process to wonder about.
        let _ = std::fs::remove_file(&tmp);
        return write;
    }
    // The rename itself is only durable once the directory entry is. Best
    // effort: a filesystem that refuses the fsync has still published a
    // complete file.
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A root that looks like a repo with a store. Returned under the key the
    /// map stores it under: a `TempDir` under `/var` on macOS is really under
    /// `/private/var`, and on Windows `canonicalize` alone would add the `\\?\`
    /// prefix the map strips.
    fn make_repo(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        canonical_key(&root)
    }

    fn entries(roots: &[&Path]) -> Vec<RepoEntry> {
        roots
            .iter()
            .map(|r| RepoEntry {
                root: r.to_string_lossy().to_string(),
                scope: None,
            })
            .collect()
    }

    /// Catches expiry measured from first record rather than the last open.
    #[test]
    fn expiry_uses_last_open_keeps_29_days_and_drops_31_days() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let old = make_repo(tmp.path(), "old");
        let recent = make_repo(tmp.path(), "recent");
        let start = 1_767_225_600;
        let map = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        map.record_at(&old, start);
        map.record_at(&recent, start);
        map.record_at(&recent, start + 2 * 86_400);
        drop(map);
        let map = RepoMap::open_at(Some(path.clone()), &[], 30, start + 31 * 86_400);
        assert_eq!(map.roots_at(start + 31 * 86_400), vec![recent]);
        assert!(old.join(".mdkb").is_dir(), "expiry never deletes the store");
        map.record_at(&old, start + 31 * 86_400);
        let restarted = RepoMap::open_at(Some(path), &[], 30, start + 31 * 86_400);
        assert!(
            restarted.contains(&old),
            "opening an expired root restores it"
        );
    }

    /// Catches a >= boundary, ignored configuration, or expiring a live daemon
    /// only at startup. The file is populated by real record calls.
    #[test]
    fn expiry_obeys_boundary_custom_limit_and_off_without_restart() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(tmp.path(), "repo");
        let start = 1_767_225_600;
        for (days, age, kept) in [
            (30, 30 * 86_400, true),
            (30, 30 * 86_400 + 1, false),
            (7, 8 * 86_400, false),
            (0, 365 * 86_400, true),
        ] {
            let map = RepoMap::open_at(None, &[], days, start);
            map.record_at(&root, start);
            assert_eq!(
                map.roots_at(start + age).contains(&root),
                kept,
                "{days} days, age {age}"
            );
        }
    }

    /// Catches writes on every hook and activity frozen on first registration.
    #[cfg(unix)]
    #[test]
    fn activity_is_persisted_once_per_utc_day_across_processes() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let root = make_repo(tmp.path(), "repo");
        let path = tmp.path().join("repos.json");
        let start = 1_767_225_600;
        let map = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        map.record_at(&root, start);
        let inode = std::fs::metadata(&path).unwrap().ino();
        map.record_at(&root, start + 100);
        let restarted = RepoMap::open_at(Some(path.clone()), &[], 30, start + 200);
        restarted.record_at(&root, start + 200);
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode);
        restarted.record_at(&root, start + 86_400);
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(read_last_opened(&path).unwrap()[&root], start + 86_400);
    }

    /// Catches a long-lived daemon expiring a repo that a separate hook has
    /// successfully opened since the daemon took its activity snapshot.
    #[test]
    fn expiry_keeps_activity_recorded_by_another_live_map() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(tmp.path(), "repo");
        let path = tmp.path().join("repos.json");
        let start = 1_767_225_600;
        let seed = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        seed.record_at(&root, start);
        let daemon = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        let hook = RepoMap::open_at(Some(path.clone()), &[], 30, start + 29 * 86_400);
        hook.record_at(&root, start + 29 * 86_400);
        assert_eq!(read_last_opened(&path).unwrap()[&root], start + 29 * 86_400);
        assert_eq!(
            daemon.roots_at(start + 31 * 86_400),
            vec![root],
            "a separate hook's successful open must prevent expiry"
        );
    }

    /// Catches a stale whole-map write rolling back another hook's timestamp,
    /// or replacing newer local activity with an older persisted timestamp.
    #[test]
    fn activity_merge_preserves_newest_timestamp_on_unrelated_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let root = make_repo(tmp.path(), "repo");
        let other = make_repo(tmp.path(), "other");
        let start = 1_767_225_600;
        let stale = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        stale.record_at(&root, start);
        let hook = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        hook.record_at(&root, start + 29 * 86_400);
        stale.record_at(&other, start + 86_400);
        assert_eq!(read_last_opened(&path).unwrap()[&root], start + 29 * 86_400);
        stale.record_at(&root, start + 32 * 86_400);
        assert_eq!(read_last_opened(&path).unwrap()[&root], start + 32 * 86_400);
        hook.record_at(&other, start + 33 * 86_400);
        assert_eq!(read_last_opened(&path).unwrap()[&root], start + 32 * 86_400);
    }

    /// Catches deletion of user declarations by age and resetting their clocks
    /// on every startup. Neither daemon.toml nor store bytes may change.
    #[test]
    fn expiry_preserves_configured_and_hand_scoped_roots() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let config_path = tmp.path().join("daemon.toml");
        let seeded = make_repo(tmp.path(), "seeded");
        let scoped = make_repo(tmp.path(), "scoped");
        let old = make_repo(tmp.path(), "old");
        let start = 1_767_225_600;
        let map = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        for root in [&seeded, &scoped, &old] {
            map.record_at(root, start);
        }
        let mut file: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for entry in file["repos"].as_array_mut().unwrap() {
            if entry["root"] == scoped.to_string_lossy().as_ref() {
                entry["scope"] = "home".into();
            }
        }
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let config = format!(
            "repo_expiry_days = 30\n[[repos]]\nroot = {:?}\n",
            seeded.to_string_lossy()
        );
        std::fs::write(&config_path, &config).unwrap();
        let at = start + 31 * 86_400;
        let map = RepoMap::open_at(Some(path.clone()), &entries(&[&seeded]), 30, at);
        assert_eq!(map.roots_at(at), vec![scoped.clone(), seeded.clone()]);
        assert_eq!(read_scope_overrides(&path)[&scoped], "home");
        assert_eq!(std::fs::read_to_string(config_path).unwrap(), config);
        assert!(old.join(".mdkb").is_dir());
    }

    /// Catches an old unreadable root being treated like an unused healthy root.
    #[cfg(unix)]
    #[test]
    fn expiry_never_removes_an_unreadable_store() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let root = make_repo(tmp.path(), "locked");
        let start = 1_767_225_600;
        let map = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        map.record_at(&root, start);
        std::fs::set_permissions(root.join(".mdkb"), std::fs::Permissions::from_mode(0o000))
            .unwrap();
        let map = RepoMap::open_at(Some(path), &[], 30, start + 31 * 86_400);
        let roots = map.roots_at(start + 31 * 86_400);
        std::fs::set_permissions(root.join(".mdkb"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        assert_eq!(roots, vec![root]);
    }

    /// Catches legacy entries being lost or their migration grace being renewed
    /// at each restart, and breaking the shape understood by v1 binaries.
    #[test]
    fn expiry_migrates_legacy_entries_once_and_keeps_v1_read_compatibility() {
        #[derive(Deserialize)]
        struct V1 {
            version: u32,
            repos: Vec<RepoEntry>,
        }
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let root = make_repo(tmp.path(), "legacy");
        std::fs::write(
            &path,
            serde_json::json!({"version": 1, "repos": [{"root": root}]}).to_string(),
        )
        .unwrap();
        let start = 1_767_225_600;
        let map = RepoMap::open_at(Some(path.clone()), &[], 30, start);
        assert!(map.contains(&root));
        let bytes = std::fs::read(&path).unwrap();
        let old_reader: V1 = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(old_reader.version, 2);
        assert_eq!(old_reader.repos[0].root, root.to_string_lossy());
        let map = RepoMap::open_at(Some(path.clone()), &[], 30, start + 29 * 86_400);
        assert!(map.contains(&root));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let map = RepoMap::open_at(Some(path), &[], 30, start + 31 * 86_400);
        assert!(!map.contains(&root));
    }

    /// AC#1 — a recorded root is still there when the next process loads the
    /// map. Two `RepoMap`s over one path stand in for a daemon restart: the
    /// first process is gone, only the file connects them.
    #[test]
    fn a_recorded_root_survives_the_process_that_recorded_it() {
        let tmp = TempDir::new().unwrap();
        let state = tmp.path().join("state");
        let root = make_repo(tmp.path(), "alpha");

        let first = RepoMap::open(Some(state.join("repos.json")), &[]);
        first.record(&root);
        drop(first);

        let restarted = RepoMap::open(Some(state.join("repos.json")), &[]);
        assert_eq!(
            restarted.roots(),
            vec![root],
            "the map is what one process leaves behind for the next"
        );
    }

    /// AC#2 — the two shapes of absence, and only those, drop a root.
    #[test]
    fn a_root_that_is_gone_or_has_no_store_is_dropped_with_a_reason() {
        let tmp = TempDir::new().unwrap();
        let deleted = make_repo(tmp.path(), "deleted");
        let storeless = make_repo(tmp.path(), "storeless");
        let healthy = make_repo(tmp.path(), "healthy");
        std::fs::remove_dir_all(&deleted).unwrap();
        std::fs::remove_dir_all(storeless.join(".mdkb")).unwrap();

        let triaged = triage(
            [deleted.clone(), storeless.clone(), healthy.clone()]
                .into_iter()
                .collect(),
        );

        assert_eq!(triaged.kept, [healthy].into_iter().collect::<BTreeSet<_>>());
        let reasons: Vec<_> = triaged
            .dropped
            .iter()
            .map(|(p, h)| (p.clone(), h.reason()))
            .collect();
        assert!(
            reasons.contains(&(deleted, "the root is gone from disk")),
            "every removal carries its reason: {reasons:?}"
        );
        assert!(
            reasons.contains(&(storeless, "the root holds no .mdkb store")),
            "every removal carries its reason: {reasons:?}"
        );
    }

    /// AC#3 — the distinction the story exists for. A store this process cannot
    /// read is not a deleted repo: it is kept, and it is reported.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_store_is_kept_and_reported_never_removed() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let locked = make_repo(tmp.path(), "locked");
        let corrupt = make_repo(tmp.path(), "corrupt");
        // A store whose database is garbage: opening it fails, listing it does
        // not. It stands for "schema newer than this binary" and "corrupt".
        std::fs::write(corrupt.join(".mdkb/index.sqlite"), b"not a database").unwrap();
        // A store this process has no permission to read at all.
        let store = locked.join(".mdkb");
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o000)).unwrap();

        let health = classify(&locked);
        assert!(
            matches!(health, RootHealth::Unreadable(_)),
            "an unreadable store must be named as such, not as absence: {health:?}"
        );
        assert!(!health.is_absence());
        assert_eq!(classify(&corrupt), RootHealth::Healthy);

        let triaged = triage([locked.clone(), corrupt.clone()].into_iter().collect());
        assert!(
            triaged.dropped.is_empty(),
            "nothing unreadable is ever dropped"
        );
        assert_eq!(triaged.kept.len(), 2);
        assert_eq!(
            triaged
                .unreachable
                .iter()
                .map(|(p, _)| p)
                .collect::<Vec<_>>(),
            vec![&locked],
            "the unreadable root is reported by name, not dropped in silence"
        );

        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// AC#4 — temp + rename, proven by the inode. An in-place write keeps the
    /// inode and truncates the live file, which is exactly the window a crash
    /// turns into a half-written map; a rename publishes a new inode holding a
    /// complete file.
    #[cfg(unix)]
    #[test]
    fn the_map_is_renamed_into_place_never_truncated_in_place() {
        use std::os::unix::fs::MetadataExt;

        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let first = make_repo(tmp.path(), "first");
        let second = make_repo(tmp.path(), "second");

        let map = RepoMap::open(Some(path.clone()), &[]);
        map.record(&first);
        let before = std::fs::metadata(&path).unwrap().ino();

        map.record(&second);
        let after = std::fs::metadata(&path).unwrap().ino();

        assert_ne!(
            before, after,
            "a new inode proves the file was renamed into place"
        );
        assert!(
            !path.with_extension("json.tmp").exists(),
            "the temp file is consumed by the rename, not left behind"
        );
        let reread = RepoMap::open(Some(path), &[]);
        assert_eq!(reread.roots(), vec![first, second]);
    }

    /// AC#5 — `daemon.toml`'s `[[repos]]` join the map, and a root the map
    /// already knew is not duplicated by the union.
    #[test]
    fn daemon_toml_repos_are_unioned_with_the_persisted_map() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let persisted = make_repo(tmp.path(), "persisted");
        let configured = make_repo(tmp.path(), "configured");

        let first = RepoMap::open(Some(path.clone()), &[]);
        first.record(&persisted);
        first.record(&configured);
        drop(first);

        let union = RepoMap::open(
            Some(path),
            &entries(&[configured.as_path(), persisted.as_path()]),
        );
        assert_eq!(
            union.roots(),
            vec![configured, persisted],
            "the union of the two sources, each root once"
        );
    }

    /// AC#5 — a configured root that no longer exists is dropped from the map.
    /// `daemon.toml` still lists it; the map is not `daemon.toml`.
    #[test]
    fn a_configured_root_that_is_gone_does_not_enter_the_map() {
        let tmp = TempDir::new().unwrap();
        let gone = tmp.path().join("never-existed");

        let map = RepoMap::open(
            Some(tmp.path().join("repos.json")),
            &entries(&[gone.as_path()]),
        );
        assert_eq!(map.roots(), [] as [std::path::PathBuf; 0]);
    }

    /// Two spellings of one repo are one entry. An operator writing a symlinked
    /// path in `daemon.toml` must not add a second row for a repo the daemon
    /// already opened under its resolved name.
    #[cfg(unix)]
    #[test]
    fn two_spellings_of_one_root_collapse_to_one_entry() {
        let tmp = TempDir::new().unwrap();
        let real = make_repo(tmp.path(), "real");
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let map = RepoMap::open(
            Some(tmp.path().join("repos.json")),
            &entries(&[link.as_path()]),
        );
        map.record(&link);

        assert_eq!(map.roots(), vec![real], "one repo, one entry");
        assert!(map.contains(&link), "under either spelling");
    }

    /// A map that is not backed by a daemon home keeps its set in memory and
    /// writes nothing. This is what keeps a `DaemonConfig` built in a test from
    /// reaching into the real `~/.mdkb`.
    #[test]
    fn a_map_with_no_path_records_in_memory_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let root = make_repo(tmp.path(), "alpha");

        let map = RepoMap::open(None, &[]);
        map.record(&root);

        assert_eq!(map.roots(), vec![root]);
        assert_eq!(
            std::fs::read_dir(tmp.path()).unwrap().count(),
            1,
            "only the repo itself: no state file was created anywhere"
        );
    }

    /// A corrupt map must not take the daemon down, and must say so rather than
    /// starting empty in silence.
    #[test]
    fn a_corrupt_map_file_starts_empty_instead_of_failing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let map = RepoMap::open(Some(path.clone()), &[]);
        assert_eq!(map.roots(), [] as [std::path::PathBuf; 0]);

        // And the next record repairs the file.
        let root = make_repo(tmp.path(), "alpha");
        map.record(&root);
        assert_eq!(RepoMap::open(Some(path), &[]).roots(), vec![root]);
    }

    /// The corrupt bytes survive the repair.
    ///
    /// Repairing by overwriting would destroy the only copy of a set somebody
    /// may still need to read — the roots are in there, however malformed the
    /// JSON around them is.
    #[test]
    fn a_corrupt_map_is_moved_aside_rather_than_overwritten() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        std::fs::write(&path, "{ this is not json").unwrap();

        let map = RepoMap::open(Some(path.clone()), &[]);
        map.record(&make_repo(tmp.path(), "alpha"));

        let quarantined: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("repos.json.corrupt-"))
            .collect();
        assert_eq!(
            quarantined.len(),
            1,
            "exactly one copy, named for when it was set aside"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(&quarantined[0])).unwrap(),
            "{ this is not json",
            "and it holds what could not be parsed, byte for byte"
        );
    }

    /// A map a newer mdkb owns is read and never written back.
    ///
    /// Writing this binary's FORMAT_VERSION over it would silently drop
    /// whatever the newer format carries that this one cannot represent, and
    /// the newer binary would find its own map downgraded underneath it.
    #[test]
    fn a_map_from_a_newer_format_is_read_but_not_rewritten() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let existing = make_repo(tmp.path(), "alpha");
        // Built with `serde_json`, not `format!`: a Windows root is full of
        // backslashes, and a hand-written `"C:\Users\..."` is not the JSON
        // escape of that path — the map would read nothing and the test would
        // prove nothing.
        std::fs::write(
            &path,
            serde_json::json!({
                "version": FORMAT_VERSION + 1,
                "repos": [{ "root": existing.to_string_lossy() }],
            })
            .to_string(),
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();

        let map = RepoMap::open(Some(path.clone()), &[]);
        assert_eq!(map.roots(), vec![existing], "the newer map is still read");

        map.record(&make_repo(tmp.path(), "beta"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "and nothing this binary does rewrites it"
        );
    }

    /// Recording is idempotent: the second record of a root neither duplicates
    /// it nor rewrites the file.
    #[test]
    fn recording_a_known_root_twice_changes_nothing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("repos.json");
        let root = make_repo(tmp.path(), "alpha");

        let map = RepoMap::open(Some(path.clone()), &[]);
        map.record(&root);
        let written = std::fs::read_to_string(&path).unwrap();
        map.record(&root);

        assert_eq!(map.roots(), vec![root]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), written);
    }

    /// Thread safety: concurrent recorders all land, and the file the last one
    /// leaves is a complete map — never a set that existed in no thread.
    #[test]
    fn concurrent_records_all_land_and_leave_one_complete_file() {
        use std::sync::{Arc, Barrier};

        const ROOTS: usize = 16;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("state/repos.json");
        let roots: Vec<PathBuf> = (0..ROOTS)
            .map(|i| make_repo(tmp.path(), &format!("repo{i}")))
            .collect();

        let map = Arc::new(RepoMap::open(Some(path.clone()), &[]));
        let barrier = Arc::new(Barrier::new(ROOTS));
        std::thread::scope(|s| {
            for root in &roots {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                s.spawn(move || {
                    barrier.wait();
                    map.record(root);
                });
            }
        });

        assert_eq!(map.roots().len(), ROOTS);
        assert_eq!(
            RepoMap::open(Some(path), &[]).roots().len(),
            ROOTS,
            "the persisted map holds every root, so no writer clobbered another's set"
        );
    }

    /// A store the walk finds: discovery identifies one by its SQLite file.
    fn plant_store(at: &Path) -> PathBuf {
        std::fs::create_dir_all(at.join(".mdkb")).unwrap();
        std::fs::write(at.join(".mdkb/index.sqlite"), b"").unwrap();
        canonical_key(at)
    }

    /// Catches whole generated/worktree subtrees being enumerated per miss.
    #[test]
    fn discovery_io_prunes_generated_and_worktree_subtrees() {
        let tmp = TempDir::new().unwrap();
        let root = plant_store(&tmp.path().join("workspace"));
        let nested = plant_store(&root.join("work/people/hr"));
        std::fs::write(root.join(".gitignore"), "/generated/\n").unwrap();
        for output in ["generated", "coverage", "plugins__wt", "build", "dist"] {
            for i in 0..100 {
                std::fs::create_dir_all(root.join(output).join(format!("copy{i}/src/deep")))
                    .unwrap();
            }
            plant_store(&root.join(output).join("copied-store"));
        }
        let found = discover_nested_stores(std::slice::from_ref(&root), &[]);
        let visits: usize = TEST_VISITS
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.starts_with(&root))
            .map(|(_, visits)| visits)
            .sum();
        assert_eq!(
            visits, 4,
            "only workspace/work/people/hr, not 1500 generated directories"
        );
        assert_eq!(found, BTreeSet::from([root, nested]));
    }

    /// Catches a known root beneath an excluded ancestor losing its own nested
    /// stores after coverage normalization; explicit roots override walk pruning.
    #[test]
    fn discovery_io_explicit_roots_below_pruned_trees_keep_nested_stores() {
        let tmp = TempDir::new().unwrap();
        let parent = plant_store(&tmp.path().join("parent"));
        std::fs::write(parent.join(".gitignore"), "/generated/\n").unwrap();
        std::fs::write(parent.join(".ignore"), "/private/\n").unwrap();
        for path in [
            "generated/repo",
            "private/repo",
            "plugins__wt/repo",
            "coverage/repo",
        ] {
            let root = plant_store(&parent.join(path));
            let nested = plant_store(&root.join("nested"));
            let found = discover_nested_stores(&[parent.clone(), root.clone()], &[]);
            assert!(found.contains(&root), "explicit {root:?}");
            assert!(found.contains(&nested), "nested {nested:?}");
            assert!(!discover_nested_stores(std::slice::from_ref(&parent), &[]).contains(&nested));
        }
    }

    /// Catches globally hiding dot-directories or similarly named normal repos,
    /// and ignoring local .ignore rules when no .git directory exists.
    #[test]
    fn discovery_io_local_rules_do_not_hide_visible_sibling_stores() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("parent");
        let expected: BTreeSet<_> = [
            ".archive",
            "coverage-tools",
            "build-tools",
            "plugins__wt-extra",
            "deep/inner",
        ]
        .iter()
        .map(|name| plant_store(&parent.join(name)))
        .collect();
        std::fs::write(parent.join(".ignore"), "/local-output/\n").unwrap();
        plant_store(&parent.join("local-output/copy"));
        assert_eq!(discover_nested_stores(&[parent], &[]), expected);
    }

    /// Catches: temporary copies of a repo (`.tmp/cov-audit/plugins`) listed as
    /// repos, which made `root=plugins` name four of them and be refused.
    #[test]
    fn a_store_under_a_tmp_segment_is_not_discovered() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("parent");
        let real = plant_store(&parent.join("plugins"));
        plant_store(&parent.join(".tmp/cov-audit/plugins"));
        plant_store(&parent.join(".tmp/mutants-src/plugins"));

        let found = discover_nested_stores(&[parent], &[]);

        assert_eq!(found, BTreeSet::from([real]));
    }

    /// Catches: an operator who cannot silence a store — an ignored path is
    /// neither listed nor walked into, and its siblings stay.
    #[test]
    fn an_ignored_path_is_not_discovered_and_its_siblings_are() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path().join("parent");
        let kept = plant_store(&parent.join("kept"));
        let silenced = plant_store(&parent.join("silenced"));
        plant_store(&parent.join("silenced/inner"));

        let found = discover_nested_stores(&[parent], std::slice::from_ref(&silenced));

        assert_eq!(found, BTreeSet::from([kept]));
    }
}
