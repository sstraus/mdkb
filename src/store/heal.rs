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
//! The index is derived data: documents re-index from their `.md` sources and
//! memory vectors re-embed from the JSON entry store (neither of which lives in
//! this database). Healing is therefore just: quarantine the corrupt files and
//! let the caller open a fresh empty database that the normal reindex path
//! repopulates.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use rusqlite::Connection;

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

/// Probe `db_path` for structural corruption (throttled by [`CHECK_INTERVAL`])
/// and quarantine it if corrupt.
///
/// Call BEFORE opening the working connection: on [`Heal::Quarantined`] the
/// original path no longer exists, so the caller's `Connection::open` creates a
/// clean database in its place.
pub fn ensure_sound(db_path: &Path) -> Result<Heal> {
    ensure_sound_at(db_path, CHECK_INTERVAL, SystemTime::now())
}

/// [`ensure_sound`] with an injectable interval and clock, for tests.
fn ensure_sound_at(db_path: &Path, interval: Duration, now: SystemTime) -> Result<Heal> {
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
}
