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

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

use serde::{Deserialize, Serialize};

use super::config::RepoEntry;

/// On-disk format of `repos.json`. Bumped when the shape changes; an unknown
/// version is read anyway (the payload is a list of paths) but reported, so an
/// older binary reading a newer file says so instead of failing silently.
const FORMAT_VERSION: u32 = 1;

/// The serialized map. `repos` reuses [`RepoEntry`] so the state file and the
/// `[[repos]]` blocks of `daemon.toml` describe a root the same way.
#[derive(Debug, Serialize, Deserialize)]
struct RepoMapFile {
    version: u32,
    repos: Vec<RepoEntry>,
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

/// Find stores below roots the daemon already knows, without opening or
/// registering them. A store is identified by its SQLite file; walking stops
/// at `.mdkb` so index internals are never traversed.
pub fn discover_nested_stores(roots: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut found = BTreeSet::new();
    for root in roots {
        let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(name.as_ref(), ".git" | "target" | "node_modules") && name != ".mdkb"
        });
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_dir() {
                continue;
            }
            let candidate = entry.path().join(".mdkb/index.sqlite");
            if candidate.is_file() {
                found.insert(canonical_key(entry.path()));
            }
        }
    }
    found
}

/// Read the persisted roots without triage, normalization, or a write-back.
/// Reporting commands use this path so inspecting coverage cannot change it.
pub fn read_known_roots(path: &Path) -> Vec<PathBuf> {
    read_file(path).roots.into_iter().collect()
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
fn canonical_key(root: &Path) -> PathBuf {
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

/// The persisted set of known repository roots.
pub struct RepoMap {
    /// Where the set is persisted. `None` for a config with no daemon home
    /// behind it: nothing on disk backs it, so nothing is written on its behalf.
    path: Option<PathBuf>,
    /// Held across the file write so two recorders cannot interleave a set with
    /// a rename and persist a map that never existed in memory.
    roots: Mutex<BTreeSet<PathBuf>>,
    /// May this process overwrite what is on disk?
    ///
    /// False when the file exists and could not be understood. Re-discovery is
    /// cheap; a map replaced by an empty one because this binary could not
    /// parse it is a set of repos nobody can get back.
    replaceable: bool,
}

impl std::fmt::Debug for RepoMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepoMap")
            .field("path", &self.path)
            .field("known", &self.roots.lock().map(|r| r.len()).unwrap_or(0))
            .field("replaceable", &self.replaceable)
            .finish()
    }
}

impl RepoMap {
    /// Load the map at `path`, union the `seeds` from `daemon.toml`, drop the
    /// roots that are gone, and persist the result if any of that changed it.
    ///
    /// A seed that no longer exists is dropped from the map but stays in
    /// `daemon.toml`, which this never rewrites: the operator's list is theirs.
    pub fn open(path: Option<PathBuf>, seeds: &[RepoEntry]) -> Self {
        let read = match &path {
            Some(p) => read_file(p),
            None => MapRead {
                roots: BTreeSet::new(),
                replaceable: true,
            },
        };
        let loaded = read.roots;
        let before = loaded.len();

        let mut union: BTreeSet<PathBuf> = loaded.iter().map(|r| canonical_key(r)).collect();
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

        let map = Self {
            path,
            roots: Mutex::new(triaged.kept),
            replaceable: read.replaceable,
        };
        // Persist only when the set on disk is not the set in hand: every CLI
        // hook builds a registry, and rewriting an unchanged map on each one
        // would rename a file per hook for nothing.
        let changed = normalized || seeded != before || !triaged.dropped.is_empty();
        if changed {
            let roots = map.roots.lock().unwrap_or_else(|e| e.into_inner());
            map.persist(&roots);
        }
        map
    }

    /// Record a root the registry has just opened. Idempotent: a root already
    /// on the map costs no write.
    pub fn record(&self, root: &Path) {
        let key = canonical_key(root);
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        if !roots.insert(key.clone()) {
            return;
        }
        tracing::info!(root = %key.display(), "Recorded repo on the map");
        self.persist(&roots);
    }

    /// Is this root known?
    pub fn contains(&self, root: &Path) -> bool {
        self.roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&canonical_key(root))
    }

    /// Every known root, sorted.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Write the whole set, atomically. A failure is reported and swallowed:
    /// losing the map costs a re-discovery, failing the open that triggered it
    /// costs the caller their tool call.
    fn persist(&self, roots: &BTreeSet<PathBuf>) {
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
        if let Err(e) = write_atomic(path, roots) {
            tracing::warn!(
                path = %path.display(),
                "Could not persist the repo map: {e} — the set is still correct in memory"
            );
        }
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
    /// False when the file exists but could not be read, parsed, or is newer
    /// than this binary understands.
    replaceable: bool,
}

/// Read the persisted set. Any failure yields an empty set and a warning: a
/// map that cannot be parsed must not take the daemon down with it.
fn read_file(path: &Path) -> MapRead {
    let empty = |replaceable| MapRead {
        roots: BTreeSet::new(),
        replaceable,
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return empty(true),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                "Could not read the repo map: {e} — keeping the file, not replacing it"
            );
            return empty(false);
        }
    };
    match serde_json::from_str::<RepoMapFile>(&content) {
        Ok(file) => {
            if file.version > FORMAT_VERSION {
                // Writing our own FORMAT_VERSION over this would silently
                // downgrade a map a newer binary owns, dropping whatever that
                // format carries which this one cannot represent.
                tracing::warn!(
                    path = %path.display(),
                    found = file.version,
                    known = FORMAT_VERSION,
                    "Repo map was written by a newer mdkb; reading it, and leaving it alone"
                );
                return MapRead {
                    roots: file
                        .repos
                        .into_iter()
                        .map(|r| PathBuf::from(r.root))
                        .collect(),
                    replaceable: false,
                };
            }
            if file.version != FORMAT_VERSION {
                tracing::warn!(
                    path = %path.display(),
                    found = file.version,
                    known = FORMAT_VERSION,
                    "Repo map written by an older format version; reading it anyway"
                );
            }
            MapRead {
                roots: file
                    .repos
                    .into_iter()
                    .map(|r| PathBuf::from(r.root))
                    .collect(),
                replaceable: true,
            }
        }
        Err(e) => {
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
fn write_atomic(path: &Path, roots: &BTreeSet<PathBuf>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = RepoMapFile {
        version: FORMAT_VERSION,
        repos: roots
            .iter()
            .map(|r| RepoEntry {
                root: r.to_string_lossy().to_string(),
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
            })
            .collect()
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
        assert!(map.roots().is_empty());
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
        assert!(map.roots().is_empty());

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
        std::fs::write(
            &path,
            format!(
                r#"{{"version": {}, "repos": [{{"root": "{}"}}]}}"#,
                FORMAT_VERSION + 1,
                existing.display()
            ),
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
}
