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
pub const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Outcome of [`ensure_sound`].
#[derive(Debug, PartialEq, Eq)]
pub enum Heal {
    /// Database is sound, or was verified within [`CHECK_INTERVAL`] (probe skipped).
    Sound,
    /// Database was structurally corrupt; its files were renamed to
    /// `corrupt_path` (and `-wal`/`-shm` siblings). The caller must open a fresh
    /// database at the original path and trigger a reindex.
    Quarantined { corrupt_path: PathBuf },
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

/// True if `marker` was touched within `interval` of `now` — i.e. the probe can
/// be skipped. A missing or unreadable marker forces a probe.
fn checked_recently(marker: &Path, interval: Duration, now: SystemTime) -> bool {
    let Ok(mtime) = std::fs::metadata(marker).and_then(|m| m.modified()) else {
        return false;
    };
    now.duration_since(mtime)
        .map(|age| age < interval)
        .unwrap_or(false)
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
        return Err(crate::error::Error::other(format!(
            "{} failed PRAGMA quick_check after mutation; close this process and reopen mdkb to quarantine and rebuild the index",
            db_path.display()
        )));
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
    let corrupt_path = with_suffix(db_path, &format!(".corrupt-{ts}"));
    std::fs::rename(db_path, &corrupt_path)?;
    for ext in ["-wal", "-shm"] {
        let side = with_suffix(db_path, ext);
        if side.exists() {
            let _ = std::fs::rename(&side, with_suffix(&corrupt_path, ext));
        }
    }
    Ok(corrupt_path)
}

/// Count of memory rows recovered from a quarantined database.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Salvage {
    pub entries: usize,
    pub edges: usize,
}

/// Copy `memory_entries` and `memory_edges` out of a quarantined database into
/// the fresh one via `ATTACH ... immutable=1`.
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
            "memory salvage: cannot attach quarantined {} ({e}) — memory entries may be lost",
            corrupt_path.display()
        );
        return Salvage::default();
    }
    let entries = salvage_table(fresh, "memory_entries");
    let edges = salvage_table(fresh, "memory_edges");
    if let Err(e) = fresh.execute("DETACH DATABASE corrupt", []) {
        tracing::warn!("memory salvage: detach failed: {e}");
    }
    Salvage { entries, edges }
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
        Ok(n) => n,
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
}

/// `.report.json` sidecar path for a quarantined database file.
fn report_path(corrupt_path: &Path) -> PathBuf {
    with_suffix(corrupt_path, ".report.json")
}

/// Parse the trailing `.corrupt-<unix_secs>` suffix into its timestamp.
fn quarantine_ts(name: &str) -> Option<i64> {
    name.rsplit_once(".corrupt-")
        .and_then(|(_, ts)| ts.parse::<i64>().ok())
}

/// Write the quarantine report sidecar. Best-effort — a failed write only costs
/// the persistent notification, never the salvage itself.
pub fn write_report(corrupt_path: &Path, salvage: Salvage) {
    let name = corrupt_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let report = QuarantineReport {
        quarantined_at: quarantine_ts(&name).unwrap_or(0),
        corrupt_file: name,
        memory_entries_salvaged: salvage.entries,
        memory_edges_salvaged: salvage.edges,
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
    if checked_recently(&marker, interval, now) {
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
    fn ensure_sound_quarantines_corrupt_db_and_frees_path() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite");
        std::fs::write(&db, b"not a sqlite database at all").unwrap();

        let outcome = ensure_sound(&db).unwrap();

        match outcome {
            Heal::Quarantined { corrupt_path } => assert!(corrupt_path.exists()),
            Heal::Sound => panic!("corrupt db must be quarantined"),
        }
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
