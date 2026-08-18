//! Autohealing for a structurally-corrupt `index.sqlite`.
//!
//! Some SQLite corruption — torn pointer-map pages, freelist mismatches — is
//! invisible to ordinary reads: `SELECT ... FROM sqlite_master` still succeeds
//! and only `PRAGMA quick_check` (which walks every page) reports it. Reactively
//! catching `SQLITE_CORRUPT` from normal queries therefore does not work; we have
//! to probe. The probe reads the whole file, so on a multi-GB index it is not
//! free — a sidecar mtime throttles it to at most once per [`CHECK_INTERVAL`] so
//! a burst of one-shot CLI opens re-scans at most once.
//!
//! Most of the index is derived data — documents re-index from their `.md`
//! sources, code symbols from source files — but NOT all of it: `memory_entries`
//! and `memory_edges` live ONLY in this database (the markdown projection under
//! `.mdkb/memory/` is best-effort and never covers DB-only entries or edges).
//! Quarantining therefore risks silent memory loss, so healing does three things
//! the caller must wire up: quarantine the corrupt files, SALVAGE the memory
//! tables out of the quarantined file into the fresh one ([`salvage_memory`]),
//! and record a [`QuarantineReport`] so the loss is surfaced loudly (stderr now,
//! `mdkb stats` + SessionStart warmup until the corrupt file is cleaned up).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::{Connection, params};

use crate::error::Result;

/// Re-run the integrity probe at most once per this interval per database.
pub const CHECK_INTERVAL: Duration = Duration::from_hours(6);

/// Outcome of [`ensure_sound`].
#[derive(Debug, PartialEq, Eq)]
pub enum Heal {
    /// Database is sound, or was verified within [`CHECK_INTERVAL`] (probe skipped).
    Sound,
    /// Database was structurally corrupt; its files were renamed to
    /// `corrupt_path` (and `-wal`/`-shm` siblings). The caller must open a fresh
    /// database at the original path and trigger a reindex.
    Quarantined { corrupt_path: PathBuf },
    /// Database is structurally corrupt but another process holds a live
    /// connection to it, so it was left in place.
    ///
    /// Renaming under an open connection recycles the path onto a second inode
    /// while the survivor keeps deriving `-wal`/`-shm` from the same names — the
    /// surviving connection can then land its frames in the *replacement*
    /// database's WAL, which is how one quarantine seeds the next corruption.
    /// The caller must surface this: every mdkb process (daemon included) has to
    /// close before the next open can quarantine and rebuild.
    CorruptInUse,
}

/// Append `suffix` to a path's file name (`index.sqlite` + `.corrupt-1` →
/// `index.sqlite.corrupt-1`). Operates on the raw `OsString` so the full
/// `index.sqlite` name is preserved rather than treated as stem + extension.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Sidecar whose mtime records the last time `db_path` passed `quick_check`.
fn marker_path(db_path: &Path) -> PathBuf {
    with_suffix(db_path, ".integrity-ok")
}

/// True if `marker` was touched within `interval` and is no older than the
/// database generation it certifies.
///
/// Age alone is insufficient: a long-lived daemon can keep writing the DB/WAL
/// after a successful probe. A marker older than either file must never suppress
/// the next open-time integrity check.
fn checked_recently(db_path: &Path, marker: &Path, interval: Duration, now: SystemTime) -> bool {
    let Ok(mtime) = std::fs::metadata(marker).and_then(|m| m.modified()) else {
        return false;
    };
    let recent = now
        .duration_since(mtime)
        .map(|age| age < interval)
        .unwrap_or(false);
    if !recent {
        return false;
    }

    for path in [db_path.to_path_buf(), with_suffix(db_path, "-wal")] {
        if let Ok(modified) = std::fs::metadata(path).and_then(|m| m.modified()) {
            if modified > mtime {
                return false;
            }
        }
    }
    true
}

/// Record a successful probe by creating/truncating the marker (updates mtime).
/// Best-effort: a failed touch just means the next open re-probes.
fn touch_marker(marker: &Path) {
    let _ = std::fs::File::create(marker);
}

/// Invalidate the last successful integrity probe before an index-wide write.
///
/// If the process crashes mid-mutation, the next open cannot trust an old
/// marker and will run `quick_check` before using the index.
pub fn invalidate_marker(db_path: &Path) {
    let _ = std::fs::remove_file(marker_path(db_path));
}

/// Run `PRAGMA quick_check`; `true` iff the database is structurally sound.
///
/// `quick_check` returns the single row `"ok"` on a clean database and one row
/// per problem otherwise. An `Err` means the file could not even be probed
/// (`SQLITE_NOTADB`, `SQLITE_CORRUPT`) — also treated as unsound.
pub fn is_structurally_sound(conn: &Connection) -> bool {
    match conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
        Ok(first) => first == "ok",
        Err(_) => false,
    }
}

/// Verify a live connection after an index-wide mutation and record success.
///
/// The caller must hold the project mutation lock. On failure the marker stays
/// absent, forcing the next process to quarantine/rebuild before normal use.
pub fn verify_and_mark(conn: &Connection, db_path: &Path) -> Result<()> {
    if !is_structurally_sound(conn) {
        invalidate_marker(db_path);
        return Err(crate::error::ErrorKind::IndexCorrupt {
            path: db_path.to_path_buf(),
        }
        .into());
    }
    touch_marker(&marker_path(db_path));
    Ok(())
}

/// [`verify_and_mark`], skipped when the last probe is younger than
/// [`CHECK_INTERVAL`].
///
/// For a database that can reach gigabytes (the code index), a full-file
/// `quick_check` after every mutation would cost more than the mutation. The
/// throttle bounds it to one scan per interval while still bounding how long
/// corruption can go unnoticed — which matters because a long-lived connection
/// serves reads from its page cache and writes into the WAL, so it can operate
/// for days over a torn file without SQLite ever reporting it.
pub fn verify_and_mark_throttled(db_path: &Path) -> Result<()> {
    verify_and_mark_throttled_at(db_path, CHECK_INTERVAL, SystemTime::now())
}

/// [`verify_and_mark_throttled`] with an injectable interval and clock.
pub fn verify_and_mark_throttled_at(
    db_path: &Path,
    interval: Duration,
    now: SystemTime,
) -> Result<()> {
    if checked_recently(db_path, &marker_path(db_path), interval, now) {
        return Ok(());
    }
    if !db_path.exists() {
        return Ok(());
    }

    // Probe on a THROWAWAY connection, not the caller's. A long-lived
    // connection answers `quick_check` out of its own page cache, so damage
    // written to the file underneath it — the whole failure mode this guards —
    // reads back as sound. A fresh connection sees the file (plus its WAL).
    let sound = {
        let probe = Connection::open(db_path)?;
        is_structurally_sound(&probe)
    };

    if !sound {
        invalidate_marker(db_path);
        return Err(crate::error::ErrorKind::IndexCorrupt {
            path: db_path.to_path_buf(),
        }
        .into());
    }
    touch_marker(&marker_path(db_path));
    Ok(())
}

/// Rename `db_path` and its `-wal`/`-shm` sidecars to `*.corrupt-<unix_secs>` so
/// a fresh database can take the original path. Returns the quarantined main-DB
/// path. The `-wal`/`-shm` moves are best-effort (they may not exist).
pub fn quarantine(db_path: &Path) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let corrupt_path = available_quarantine_path(db_path, ts);
    std::fs::rename(db_path, &corrupt_path)?;
    let mut moved_sidecars = Vec::new();
    for ext in ["-wal", "-shm"] {
        let side = with_suffix(db_path, ext);
        if side.exists() {
            let target = with_suffix(&corrupt_path, ext);
            if let Err(error) = std::fs::rename(&side, &target) {
                // A fresh database must never open beside an orphaned WAL from the corrupt
                // generation. Restore every completed rename before returning the failure.
                for (moved, original) in moved_sidecars.into_iter().rev() {
                    let _ = std::fs::rename(moved, original);
                }
                let _ = std::fs::rename(&corrupt_path, db_path);
                return Err(error.into());
            }
            moved_sidecars.push((target, side));
        }
    }
    Ok(corrupt_path)
}

fn available_quarantine_path(db_path: &Path, timestamp: u64) -> PathBuf {
    let base_suffix = format!(".corrupt-{timestamp}");
    let mut candidate = with_suffix(db_path, &base_suffix);
    let mut collision = 0_u32;
    while candidate.exists() {
        collision += 1;
        candidate = with_suffix(db_path, &format!("{base_suffix}-{collision}"));
    }
    candidate
}

/// Count of rows recovered from a quarantined database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Salvage {
    pub entries: usize,
    pub edges: usize,
    /// Collection registrations recovered. Reported separately because their
    /// loss has a different symptom from memory loss: not "an entry is missing"
    /// but "every later `mdkb update` silently indexes the wrong thing".
    pub collections: usize,
    /// Memory revisions recovered — edit history, and since schema v19 the
    /// losing side of every file/DB conflict.
    pub revisions: usize,
    /// Mined behavioural priors (candidates + clusters) recovered.
    pub priors: usize,
}

/// Tables that a quarantine must carry into the fresh database, in the order
/// they are copied.
///
/// The rule is whether the rows can be re-derived from files still on disk.
/// `documents`, `content` and `edges` can: `mdkb update` rebuilds them from the
/// markdown, so dropping them costs a reindex. These cannot. `collections`
/// records the *decision* that a directory is a collection, which exists nowhere
/// else — story 012-19e7 is what its loss looks like from outside: a store went
/// from 2046 indexed documents to 3, `mdkb update` printed success and exited 0,
/// and the cause was blamed on an unrelated config edit for weeks.
///
/// Order matters: `memory_revisions` has a foreign key onto `memory_entries`, so
/// the parent is copied first and `INSERT OR IGNORE` drops any child whose
/// parent was in the torn pages.
///
/// `evolution` is deliberately absent — its foreign keys point at `documents`,
/// which the rebuild wipes, so every row would be rejected. Recovering it would
/// have to happen after a reindex, against document ids that are re-assigned.
const SALVAGED_TABLES: [&str; 5] = [
    "memory_entries",
    "memory_edges",
    "memory_revisions",
    "collections",
    "prior_clusters",
];

/// Copy the non-derivable tables out of a quarantined database into the fresh
/// one via `ATTACH ... immutable=1`.
///
/// `immutable=1` tells SQLite the file will not change, so it skips locking and
/// hot-journal rollback — the only safe way to read a possibly-corrupt file. The
/// copy is best-effort: it never fails the caller's open. A table that cannot be
/// read (its pages are the torn ones) is logged loudly with the row count that
/// was present but lost, so a data-loss event is never silent.
pub fn salvage_memory(fresh: &Connection, corrupt_path: &Path) -> Salvage {
    let uri = format!("file:{}?immutable=1", corrupt_path.to_string_lossy());
    if let Err(e) = fresh.execute("ATTACH DATABASE ?1 AS corrupt", params![uri]) {
        tracing::error!(
            "salvage: cannot attach quarantined {} ({e}) — memory entries and collection \
             registrations may be lost",
            corrupt_path.display()
        );
        return Salvage::default();
    }
    let mut salvage = Salvage::default();
    for table in SALVAGED_TABLES {
        let rows = salvage_table(fresh, table);
        match table {
            "memory_entries" => salvage.entries = rows,
            "memory_edges" => salvage.edges = rows,
            "memory_revisions" => salvage.revisions = rows,
            "collections" => salvage.collections = rows,
            _ => salvage.priors += rows,
        }
    }
    // Candidates reference a cluster, so clusters go first; counted together
    // because the pair is one feature to the operator.
    salvage.priors += salvage_table(fresh, "prior_candidates");
    if let Err(e) = fresh.execute("DETACH DATABASE corrupt", []) {
        tracing::warn!("salvage: detach failed: {e}");
    }
    if salvage.collections > 0 {
        tracing::warn!(
            collections = salvage.collections,
            "salvaged collection registrations from the quarantined index — run `mdkb update` \
             to re-index their documents"
        );
    }
    salvage
}

/// Copy one whole table from the attached `corrupt` db into `main`, returning the
/// number of rows recovered. `table` is a hardcoded constant (never user input),
/// so the format-string SQL carries no injection risk. Same schema on both sides
/// means `SELECT *` column order matches.
fn salvage_table(fresh: &Connection, table: &str) -> usize {
    let present: usize = fresh
        .query_row(&format!("SELECT COUNT(*) FROM corrupt.{table}"), [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    if present == 0 {
        return 0;
    }
    match fresh.execute(
        &format!("INSERT OR IGNORE INTO main.{table} SELECT * FROM corrupt.{table}"),
        [],
    ) {
        Ok(inserted) => {
            if inserted < present {
                let not_recovered = present - inserted;
                tracing::error!(
                    "memory salvage: {not_recovered} of {present} rows in {table} were NOT recovered (INSERT OR IGNORE skipped them)"
                );
            }
            inserted
        }
        Err(e) => {
            tracing::error!(
                "memory salvage: {present} rows in {table} could NOT be recovered ({e}) — they are LOST"
            );
            0
        }
    }
}

/// A record of one quarantine event, persisted as a sidecar next to the corrupt
/// file so `mdkb stats` and SessionStart can surface the loss until the operator
/// cleans up. Serialized to `<corrupt_file>.report.json`.
///
/// The forensic fields exist because a quarantined file on its own has never
/// been enough to name a cause: every past post-mortem stalled at "the index is
/// malformed" with no record of *how*. They are captured once, at quarantine
/// time, on a file that is already known to be corrupt — so they cost nothing
/// on any healthy path.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct QuarantineReport {
    /// File name (not full path) of the quarantined database.
    pub corrupt_file: String,
    /// Unix seconds when the quarantine happened (from the corrupt file suffix).
    pub quarantined_at: i64,
    /// Memory entries recovered into the fresh database.
    pub memory_entries_salvaged: usize,
    /// Memory edges recovered into the fresh database.
    pub memory_edges_salvaged: usize,
    /// `PRAGMA quick_check` rows: the damage as SQLite describes it.
    #[serde(default)]
    pub quick_check: Vec<String>,
    /// Tables owning the b-trees `quick_check` named, resolved through
    /// `sqlite_master.rootpage`. This is what distinguishes one recurrence from
    /// another — field damage has so far always landed on the memory tables.
    #[serde(default)]
    pub damaged_tables: Vec<String>,
    /// Size of the quarantined database and of its WAL, in bytes. A large WAL
    /// means the damage was taken with un-checkpointed frames outstanding.
    #[serde(default)]
    pub db_bytes: u64,
    #[serde(default)]
    pub wal_bytes: u64,
    /// Process that detected the corruption — NOT necessarily the one that
    /// caused it, which is exactly why the distinction is spelled out here.
    #[serde(default)]
    pub detected_by_pid: u32,
    #[serde(default)]
    pub detected_by_version: String,
}

/// Forensics read off a quarantined file. Best-effort throughout: a file too
/// damaged to answer a question contributes nothing rather than failing the
/// quarantine.
#[derive(Debug, Clone, Default)]
struct Diagnosis {
    quick_check: Vec<String>,
    damaged_tables: Vec<String>,
    db_bytes: u64,
    wal_bytes: u64,
}

/// Rows `quick_check` reports name b-trees by root page (`Tree 23 page 23 cell
/// 4: ...`). Extract those root pages so they can be resolved to table names.
fn root_pages_in(rows: &[String]) -> Vec<i64> {
    let mut pages = Vec::new();
    for row in rows {
        let Some(rest) = row.strip_prefix("Tree ") else {
            continue;
        };
        let Some(page) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(page) = page.parse::<i64>()
            && !pages.contains(&page)
        {
            pages.push(page);
        }
    }
    pages
}

fn file_bytes(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Describe the damage in a quarantined database.
///
/// Read through `immutable=1`, the only safe way to open a file that may be
/// torn: no locking, no hot-journal rollback, no writes.
fn diagnose(corrupt_path: &Path) -> Diagnosis {
    let mut diagnosis = Diagnosis {
        db_bytes: file_bytes(corrupt_path),
        wal_bytes: file_bytes(&with_suffix(corrupt_path, "-wal")),
        ..Default::default()
    };

    let uri = format!("file:{}?immutable=1", corrupt_path.to_string_lossy());
    let conn = match Connection::open(&uri) {
        Ok(conn) => conn,
        Err(e) => {
            diagnosis.quick_check = vec![format!("cannot open quarantined file: {e}")];
            return diagnosis;
        }
    };

    // Bounded: enough rows to characterise the damage, not a full page walk of
    // a file that can reach gigabytes.
    //
    // Stepped by hand rather than collected through an iterator because damage
    // bad enough to abort the walk surfaces as an Err *after* zero or more
    // rows, and that Err is itself the diagnosis — dropping it (as `flatten`
    // would) is how a badly torn file ends up recorded as "no damage found".
    diagnosis.quick_check = match conn.prepare("PRAGMA quick_check(20)") {
        Ok(mut stmt) => match stmt.query([]) {
            Ok(mut rows) => {
                let mut out = Vec::new();
                loop {
                    match rows.next() {
                        Ok(Some(row)) => match row.get::<_, String>(0) {
                            Ok(text) if text != "ok" => out.push(text),
                            Ok(_) => {}
                            Err(e) => out.push(format!("unreadable quick_check row: {e}")),
                        },
                        Ok(None) => break,
                        Err(e) => {
                            out.push(format!("quick_check aborted: {e}"));
                            break;
                        }
                    }
                }
                out
            }
            Err(e) => vec![format!("quick_check failed: {e}")],
        },
        Err(e) => vec![format!("quick_check unavailable: {e}")],
    };

    for page in root_pages_in(&diagnosis.quick_check) {
        let name = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE rootpage = ?1",
                params![page],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_else(|_| format!("rootpage {page}"));
        if !diagnosis.damaged_tables.contains(&name) {
            diagnosis.damaged_tables.push(name);
        }
    }

    diagnosis
}

/// `.report.json` sidecar path for a quarantined database file.
fn report_path(corrupt_path: &Path) -> PathBuf {
    with_suffix(corrupt_path, ".report.json")
}

/// Parse the trailing `.corrupt-<unix_secs>` suffix into its timestamp.
fn quarantine_ts(name: &str) -> Option<i64> {
    name.rsplit_once(".corrupt-")
        .and_then(|(_, suffix)| suffix.split('-').next())
        .and_then(|ts| ts.parse::<i64>().ok())
}

/// Write the quarantine report sidecar. Best-effort — a failed write only costs
/// the persistent notification, never the salvage itself.
pub fn write_report(corrupt_path: &Path, salvage: Salvage) {
    let name = corrupt_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let diagnosis = diagnose(corrupt_path);
    if !diagnosis.damaged_tables.is_empty() {
        tracing::error!(
            tables = diagnosis.damaged_tables.join(", "),
            "index corruption damaged these tables"
        );
    }
    let report = QuarantineReport {
        quarantined_at: quarantine_ts(&name).unwrap_or(0),
        corrupt_file: name,
        memory_entries_salvaged: salvage.entries,
        memory_edges_salvaged: salvage.edges,
        quick_check: diagnosis.quick_check,
        damaged_tables: diagnosis.damaged_tables,
        db_bytes: diagnosis.db_bytes,
        wal_bytes: diagnosis.wal_bytes,
        detected_by_pid: std::process::id(),
        detected_by_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    match serde_json::to_string_pretty(&report) {
        Ok(json) => {
            if let Err(e) = std::fs::write(report_path(corrupt_path), json) {
                tracing::warn!("quarantine report write failed: {e}");
            }
        }
        Err(e) => tracing::warn!("quarantine report serialize failed: {e}"),
    }
}

/// All outstanding quarantine reports in `mdkb_dir`: one per `*.corrupt-*`
/// database still on disk. The corrupt DB file (not its `.report.json` sidecar)
/// is the gating artifact — the warning clears once the operator deletes it.
/// A missing/unreadable sidecar still yields a report (with zero salvage counts)
/// so the quarantine itself is never hidden.
pub fn quarantine_reports(mdkb_dir: &Path) -> Vec<QuarantineReport> {
    let Ok(entries) = std::fs::read_dir(mdkb_dir) else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // The corrupt DB itself, not the sidecar or WAL/SHM siblings.
        if !name.contains(".corrupt-")
            || name.ends_with(".report.json")
            || name.ends_with("-wal")
            || name.ends_with("-shm")
        {
            continue;
        }
        let report = std::fs::read_to_string(report_path(&entry.path()))
            .ok()
            .and_then(|s| serde_json::from_str::<QuarantineReport>(&s).ok())
            .unwrap_or_else(|| QuarantineReport {
                quarantined_at: quarantine_ts(&name).unwrap_or(0),
                corrupt_file: name.clone(),
                ..Default::default()
            });
        reports.push(report);
    }
    reports.sort_by_key(|r| r.quarantined_at);
    reports
}

/// Probe `db_path` for structural corruption (throttled by [`CHECK_INTERVAL`])
/// and quarantine it if corrupt.
///
/// Call BEFORE opening the working connection: on [`Heal::Quarantined`] the
/// original path no longer exists, so the caller's `Connection::open` creates a
/// clean database in its place.
pub fn ensure_sound(db_path: &Path) -> Result<Heal> {
    let _guard = crate::store::mutation_lock::acquire(db_path, "integrity-check")?;
    ensure_sound_locked(db_path)
}

/// Probe while the caller already holds the project mutation lock.
///
/// Used by `Context::open`, which must keep the same lock through schema and
/// virtual-table initialization so concurrent openers cannot race FTS setup.
pub(crate) fn ensure_sound_locked(db_path: &Path) -> Result<Heal> {
    ensure_sound_at_locked(db_path, CHECK_INTERVAL, SystemTime::now())
}

/// [`ensure_sound`] with an injectable interval and clock, for tests.
#[cfg(test)]
fn ensure_sound_at(db_path: &Path, interval: Duration, now: SystemTime) -> Result<Heal> {
    let _guard = crate::store::mutation_lock::acquire(db_path, "integrity-check")?;
    ensure_sound_at_locked(db_path, interval, now)
}

/// Integrity probe implementation. The caller must hold the mutation lock.
fn ensure_sound_at_locked(db_path: &Path, interval: Duration, now: SystemTime) -> Result<Heal> {
    if !db_path.exists() {
        return Ok(Heal::Sound); // fresh database — nothing to probe
    }
    let marker = marker_path(db_path);
    if checked_recently(db_path, &marker, interval, now) {
        return Ok(Heal::Sound);
    }

    // Probe on a throwaway connection so no open handle survives the rename.
    let sound = {
        let conn = Connection::open(db_path)?;
        is_structurally_sound(&conn)
    };

    if sound {
        touch_marker(&marker);
        return Ok(Heal::Sound);
    }

    // Only rename when nobody is holding the database open — see [`Heal::CorruptInUse`].
    let Some(_live) = crate::store::mutation_lock::try_acquire_live_exclusive(db_path)? else {
        let _ = std::fs::remove_file(&marker);
        return Ok(Heal::CorruptInUse);
    };

    let corrupt_path = quarantine(db_path)?;
    let _ = std::fs::remove_file(&marker);
    Ok(Heal::Quarantined { corrupt_path })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a small on-disk database with a table and rows so it occupies
    /// several pages — enough that truncation produces a torn b-tree.
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, blob TEXT);
             PRAGMA journal_mode = DELETE;", // no -wal so the file is self-contained
        )
        .unwrap();
        let payload = "x".repeat(2000);
        for i in 0..200 {
            conn.execute("INSERT INTO t (id, blob) VALUES (?1, ?2)", (i, &payload))
                .unwrap();
        }
    }

    #[test]
    fn sound_db_passes_probe() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        let conn = Connection::open(&db).unwrap();
        assert!(is_structurally_sound(&conn));
    }

    #[test]
    fn garbage_file_is_not_sound() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"this is definitely not a sqlite database").unwrap();

        let conn = Connection::open(&db).unwrap();
        assert!(!is_structurally_sound(&conn));
    }

    #[test]
    fn truncated_db_is_not_sound() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        // Lop off the trailing pages: valid header, torn b-tree — exactly the
        // class of structural damage that ordinary reads miss but quick_check catches.
        let len = std::fs::metadata(&db).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        f.set_len(len / 2).unwrap();
        drop(f);

        let conn = Connection::open(&db).unwrap();
        assert!(!is_structurally_sound(&conn));
    }

    #[test]
    fn root_pages_are_read_off_real_quick_check_rows() {
        // Verbatim rows from quarantined field stores.
        let rows = vec![
            "*** in database main ***".to_string(),
            "Tree 23 page 23 cell 35: 2nd reference to page 1820".to_string(),
            "Tree 23 page 23 cell 34: 2nd reference to page 1819".to_string(),
            "Tree 66 page 66 cell 0: 2nd reference to page 5191".to_string(),
            "wrong # of entries in index idx_memory_access".to_string(),
        ];
        assert_eq!(
            root_pages_in(&rows),
            vec![23, 66],
            "each damaged b-tree is named once, in report order"
        );
    }

    #[test]
    fn the_report_records_the_damage_not_just_the_loss() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        let len = std::fs::metadata(&db).unwrap().len();
        let f = std::fs::OpenOptions::new().write(true).open(&db).unwrap();
        f.set_len(len / 2).unwrap();
        drop(f);

        let corrupt = quarantine(&db).unwrap();
        write_report(&corrupt, Salvage::default());

        let report: QuarantineReport =
            serde_json::from_str(&std::fs::read_to_string(report_path(&corrupt)).unwrap()).unwrap();

        assert!(
            !report.quick_check.is_empty(),
            "the quarantine must record how SQLite described the damage"
        );
        assert_eq!(
            report.db_bytes,
            len / 2,
            "the size of the file that was set aside"
        );
        assert_eq!(report.detected_by_pid, std::process::id());
        assert_eq!(report.detected_by_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn quarantine_moves_db_and_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(with_suffix(&db, "-wal"), b"wal").unwrap();
        std::fs::write(with_suffix(&db, "-shm"), b"shm").unwrap();

        let corrupt = quarantine(&db).unwrap();

        assert!(!db.exists(), "original db removed");
        assert!(corrupt.exists(), "db quarantined");
        assert!(
            with_suffix(&corrupt, "-wal").exists(),
            "wal quarantined alongside db"
        );
        assert!(
            with_suffix(&corrupt, "-shm").exists(),
            "shm quarantined alongside db"
        );
        assert!(!with_suffix(&db, "-wal").exists(), "original wal removed");
    }

    #[test]
    fn quarantine_path_never_overwrites_an_existing_forensic_copy() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        let first = with_suffix(&db, ".corrupt-123");
        let second = with_suffix(&db, ".corrupt-123-1");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        let candidate = available_quarantine_path(&db, 123);

        assert_eq!(candidate, with_suffix(&db, ".corrupt-123-2"));
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        assert_eq!(quarantine_ts("index.sqlite.corrupt-123-2"), Some(123));
    }

    #[test]
    fn ensure_sound_quarantines_corrupt_db_and_frees_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"not a sqlite database at all").unwrap();

        let outcome = ensure_sound(&db).unwrap();

        match outcome {
            Heal::Quarantined { corrupt_path } => assert!(corrupt_path.exists()),
            other => panic!("corrupt db must be quarantined, got {other:?}"),
        }
        assert!(!db.exists(), "path is freed for a fresh database");
    }

    /// Unix-only: the live-connection probe rides on POSIX byte-range
    /// locks. On Windows the same probe errors with os error 33 (lock
    /// violation) while a connection is live — a real platform difference
    /// in the probe, not a test artifact; it needs its own fix.
    #[cfg(unix)]
    #[test]
    fn corrupt_db_is_left_in_place_while_a_connection_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"not a sqlite database at all").unwrap();

        // Somebody has the database open: renaming it now would recycle the path
        // onto a second inode while that connection keeps deriving its
        // `-wal`/`-shm` from the old name.
        let _live = crate::store::mutation_lock::acquire_live_shared(&db).unwrap();

        assert_eq!(
            ensure_sound(&db).unwrap(),
            Heal::CorruptInUse,
            "a live connection must veto the quarantine"
        );
        assert!(
            db.exists(),
            "the corrupt file stays where the holder sees it"
        );
        assert!(
            !marker_path(&db).exists(),
            "no integrity marker, so the next open re-probes instead of trusting it"
        );
    }

    /// Unix-only: blocked by the Windows live-lock probe defect — see the
    /// note on `corrupt_db_is_left_in_place_while_a_connection_is_live` in
    /// `src/store/heal.rs`.
    #[cfg(unix)]
    #[test]
    fn quarantine_resumes_once_the_last_connection_closes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"not a sqlite database at all").unwrap();

        let live = crate::store::mutation_lock::acquire_live_shared(&db).unwrap();
        assert_eq!(ensure_sound(&db).unwrap(), Heal::CorruptInUse);
        drop(live);

        assert!(
            matches!(ensure_sound(&db).unwrap(), Heal::Quarantined { .. }),
            "with no holder left the corrupt file is quarantined as before"
        );
        assert!(!db.exists(), "path is freed for a fresh database");
    }

    #[test]
    fn ensure_sound_leaves_healthy_db_and_writes_marker() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);

        assert_eq!(ensure_sound(&db).unwrap(), Heal::Sound);
        assert!(db.exists(), "healthy db untouched");
        assert!(marker_path(&db).exists(), "successful probe records marker");
    }

    #[test]
    fn ensure_sound_skips_probe_when_recently_checked() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        // Corrupt bytes, but a fresh marker: the throttle must skip the probe so
        // the corrupt file survives (proving the probe genuinely didn't run).
        std::fs::write(&db, b"corrupt").unwrap();
        touch_marker(&marker_path(&db));

        let outcome = ensure_sound_at(&db, CHECK_INTERVAL, SystemTime::now()).unwrap();
        assert_eq!(outcome, Heal::Sound, "recent marker skips the probe");
        assert!(db.exists(), "throttled probe left the file untouched");
    }

    #[test]
    fn ensure_sound_does_not_trust_a_marker_older_than_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        make_db(&db);
        touch_marker(&marker_path(&db));

        // The production incident had a marker from 10:10 and a DB modified at
        // 12:09. Preserve that ordering at a much smaller scale.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&db, b"corrupt after the successful probe").unwrap();

        let outcome = ensure_sound_at(&db, CHECK_INTERVAL, SystemTime::now()).unwrap();
        assert!(
            matches!(outcome, Heal::Quarantined { .. }),
            "a post-marker DB write must force a fresh integrity probe"
        );
    }

    #[test]
    fn ensure_sound_reprobes_after_interval_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"corrupt").unwrap();
        touch_marker(&marker_path(&db));

        // A zero-length interval forces every probe to run despite the marker.
        let outcome = ensure_sound_at(&db, Duration::ZERO, SystemTime::now()).unwrap();
        assert!(
            matches!(outcome, Heal::Quarantined { .. }),
            "elapsed interval re-probes and quarantines the corrupt file"
        );
    }

    #[test]
    fn ensure_sound_on_missing_db_is_sound() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        assert_eq!(ensure_sound(&db).unwrap(), Heal::Sound);
    }

    /// A database with memory rows, standing in for the quarantined file.
    fn make_memory_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        for i in 0..2 {
            conn.execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
                 VALUES (?1, ?2, 'body', 'topic', 1, 1)",
                params![format!("m{i}"), format!("Title {i}")],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO memory_edges (source_id, target_ref, target_kind, relation, created_at)
             VALUES ('m0', 'm1', 'memory', 'supports', 1)",
            [],
        )
        .unwrap();
    }

    #[test]
    fn salvage_recovers_memory_tables_from_quarantined_db() {
        // memory_entries/memory_edges live ONLY in the index — a quarantine must
        // salvage them into the fresh db or they are gone (the tuicommander bug).
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-123");
        make_memory_db(&corrupt);

        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();

        let salvage = salvage_memory(&fresh, &corrupt);
        assert_eq!(salvage.entries, 2, "both memory entries recovered");
        assert_eq!(salvage.edges, 1, "the memory edge recovered");

        let entries: i64 = fresh
            .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(entries, 2);
        let edges: i64 = fresh
            .query_row("SELECT COUNT(*) FROM memory_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(edges, 1);
    }

    #[test]
    fn salvage_on_unreadable_file_returns_zero_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let bogus = dir.path().join("index.sqlite.corrupt-9");
        std::fs::write(&bogus, b"not a database").unwrap();
        let fresh = Connection::open(dir.path().join("index.sqlite")).unwrap();
        crate::store::schema::init_schema(&fresh).unwrap();

        // Best-effort: garbage yields no rows, never an error/panic.
        let salvage = salvage_memory(&fresh, &bogus);
        assert_eq!(salvage, Salvage::default());
    }

    #[test]
    fn quarantine_report_persists_and_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let corrupt = dir.path().join("index.sqlite.corrupt-1700000000");
        std::fs::write(&corrupt, b"corrupt bytes").unwrap();
        write_report(
            &corrupt,
            Salvage {
                entries: 5,
                edges: 2,
                ..Default::default()
            },
        );

        let reports = quarantine_reports(dir.path());
        assert_eq!(reports.len(), 1, "one outstanding quarantine");
        assert_eq!(reports[0].memory_entries_salvaged, 5);
        assert_eq!(reports[0].memory_edges_salvaged, 2);
        assert_eq!(reports[0].quarantined_at, 1_700_000_000);
        assert!(reports[0].corrupt_file.ends_with(".corrupt-1700000000"));
    }

    #[test]
    fn quarantine_reports_empty_on_healthy_store() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.sqlite"), b"db").unwrap();
        assert!(quarantine_reports(dir.path()).is_empty());
    }

    #[test]
    fn quarantine_reports_without_sidecar_still_reports() {
        // A corrupt file with no .report.json (e.g. an older quarantine) must not
        // be hidden — the quarantine itself is surfaced with zero salvage counts.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.sqlite.corrupt-42"), b"x").unwrap();
        let reports = quarantine_reports(dir.path());
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].quarantined_at, 42);
        assert_eq!(reports[0].memory_entries_salvaged, 0);
    }
}
