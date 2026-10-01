//! Critic attacks on the lock-free post-write probe (#201-481e, round 3).
//! Public API only: `run_mutation_verify_after_release`,
//! `heal::verify_and_mark_unadmitted`, `handle_update{,_unverified}`.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use mdkb::core::indexing::{handle_update, handle_update_unverified};
use mdkb::core::{Context, run_mutation_verify_after_release};
use mdkb::error::{Error, ErrorKind};
use mdkb::store::{heal, mutation_lock};
use rusqlite::Connection;
use tempfile::TempDir;

fn marker(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(".integrity-ok");
    PathBuf::from(name)
}

fn slot_in(dir: &Path) -> tokio::sync::Mutex<Option<Context>> {
    tokio::sync::Mutex::new(Some(Context::init(dir).unwrap()))
}

fn db_of(slot: &tokio::sync::Mutex<Option<Context>>) -> PathBuf {
    slot.try_lock().unwrap().as_ref().unwrap().db_path.clone()
}

fn corrupt() -> Error {
    ErrorKind::IndexCorrupt {
        path: PathBuf::from("x"),
    }
    .into()
}

/// True when `acquire` of a project lock succeeds within a few seconds from
/// another thread (so a held lock shows as a timeout, not a hang).
fn lock_is_free(db: &Path, writer: bool) -> bool {
    let (tx, rx) = mpsc::channel();
    let db = db.to_path_buf();
    std::thread::spawn(move || {
        let guard = if writer {
            mutation_lock::acquire_writer(&db, "critic")
        } else {
            mutation_lock::acquire(&db, "critic")
        };
        let _ = tx.send(guard.is_ok());
    });
    rx.recv_timeout(Duration::from_secs(3)).unwrap_or(false)
}

/// Catches: the integrity probe still running under the cross-process writer
/// admission, which queues every CLI/daemon writer behind the full-file scan
/// (the slot test alone does not see this lock).
#[test]
fn the_writer_admission_is_free_while_the_probe_runs() {
    let dir = TempDir::new().unwrap();
    let slot = slot_in(dir.path());
    let db = db_of(&slot);
    let mut free = false;
    let out = run_mutation_verify_after_release(
        &slot,
        "t",
        |_| Ok(()),
        |p| {
            free = lock_is_free(p, true);
            Ok(())
        },
    );
    assert!(matches!(out, Some(Ok(()))));
    assert!(free, "writer admission held during the probe for {db:?}");
}

/// Catches: the verify closure running before the mutation (it would certify
/// the pre-write bytes).
#[test]
fn the_probe_runs_after_the_mutation() {
    let dir = TempDir::new().unwrap();
    let slot = slot_in(dir.path());
    let mutated = std::cell::Cell::new(false);
    let mutated_at_probe = std::cell::Cell::new(false);
    let out = run_mutation_verify_after_release(
        &slot,
        "t",
        |_| {
            mutated.set(true);
            Ok(())
        },
        |_| {
            mutated_at_probe.set(mutated.get());
            Ok(())
        },
    );
    assert!(matches!(out, Some(Ok(()))));
    assert!(mutated_at_probe.get(), "the probe ran before the mutation");
}

/// Catches: dropping `invalidate_marker` before the mutation, so a crash
/// mid-write leaves an old "sound" marker that suppresses recovery.
#[test]
fn a_stale_marker_is_gone_while_the_mutation_runs() {
    let dir = TempDir::new().unwrap();
    let slot = slot_in(dir.path());
    let db = db_of(&slot);
    std::fs::write(marker(&db), b"").unwrap();
    let mut marker_seen_by_mutation = true;
    let out = run_mutation_verify_after_release(
        &slot,
        "t",
        |_| {
            marker_seen_by_mutation = marker(&db).exists();
            Ok(())
        },
        |_| Ok(()),
    );
    assert!(matches!(out, Some(Ok(()))));
    assert!(
        !marker_seen_by_mutation,
        "pre-write marker survived into the write"
    );
}

/// Catches: closing the long-lived slot on ANY probe failure. A probe that
/// reached no verdict (locked, I/O) says nothing about the bytes; the slot must
/// stay open and the error must surface.
#[test]
fn an_undetermined_probe_keeps_the_slot_open() {
    let dir = TempDir::new().unwrap();
    let slot = slot_in(dir.path());
    let out = run_mutation_verify_after_release(
        &slot,
        "t",
        |_| Ok(()),
        |_| {
            Err(ErrorKind::Io {
                path: PathBuf::from("x"),
                operation: "probe busy".into(),
            }
            .into())
        },
    );
    assert!(matches!(out, Some(Err(ref e)) if !e.is_index_corrupt()));
    assert!(slot.try_lock().unwrap().is_some());
}

/// Catches: only the probe's error deciding the close. Corruption reported by
/// the mutation itself (SQLITE_CORRUPT mid-write) with a probe that passes must
/// still close the slot, as `run_mutation` does.
#[test]
fn corruption_returned_by_the_mutation_closes_the_slot_even_if_the_probe_passes() {
    let dir = TempDir::new().unwrap();
    let slot = slot_in(dir.path());
    let out = run_mutation_verify_after_release(
        &slot,
        "t",
        |_| -> Result<(), Error> { Err(corrupt()) },
        |_| Ok(()),
    );
    assert!(matches!(out, Some(Err(ref e)) if e.is_index_corrupt()));
    assert!(slot.try_lock().unwrap().is_none());
}

/// Catches: the mutation's own (more specific) error being replaced by the
/// probe's unrelated error, hiding why the write failed.
#[test]
fn a_failed_mutation_keeps_its_own_error_when_the_probe_passes() {
    let dir = TempDir::new().unwrap();
    let slot = slot_in(dir.path());
    let out = run_mutation_verify_after_release(
        &slot,
        "t",
        |_| -> Result<(), Error> {
            Err(ErrorKind::Io {
                path: PathBuf::from("mutation"),
                operation: "mutation failed".into(),
            }
            .into())
        },
        |_| Ok(()),
    );
    let text = format!("{:?}", out.unwrap().unwrap_err());
    assert!(text.contains("mutation failed"), "{text}");
    assert!(slot.try_lock().unwrap().is_some());
}

/// Catches: the post-release close hitting a HEALTHY context that another
/// holder installed while the probe ran lock-free (guarded write closed the
/// slot, `ensure_handle_context` reopened it over a quarantined, rebuilt file;
/// the late verdict describes the old file). The late "corrupt" must not tear
/// down the fresh connection.
#[test]
fn a_late_corrupt_verdict_does_not_close_a_context_reopened_meanwhile() {
    let dir = TempDir::new().unwrap();
    let slot = slot_in(dir.path());
    let out = run_mutation_verify_after_release(
        &slot,
        "t",
        |_| Ok(()),
        |_| {
            let mut g = slot.try_lock().expect("slot is free during the probe");
            *g = None;
            *g = Some(Context::init(dir.path()).unwrap());
            Err(corrupt())
        },
    );
    assert!(matches!(out, Some(Err(ref e)) if e.is_index_corrupt()));
    assert!(
        slot.try_lock().unwrap().is_some(),
        "a verdict about the old file closed the freshly opened context"
    );
}

fn page_corrupted_db(dir: &Path) -> PathBuf {
    let db = dir.join("index.sqlite");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE t (k INTEGER PRIMARY KEY, v BLOB); CREATE INDEX ti ON t(v);")
        .unwrap();
    for i in 0..3000 {
        conn.execute("INSERT INTO t (v) VALUES (randomblob(300 + ?1 % 7))", [i])
            .unwrap();
    }
    drop(conn);
    let mut f = OpenOptions::new().write(true).open(&db).unwrap();
    for page in 4..9u64 {
        f.seek(SeekFrom::Start(page * 4096)).unwrap();
        f.write_all(&[0xA5; 4096]).unwrap();
    }
    f.sync_all().unwrap();
    db
}

/// Catches: the lock-free probe mapping a genuinely torn file to Sound (or to
/// "undetermined"), so no IndexCorrupt ever reaches the daemon and the marker
/// certifies garbage.
#[test]
fn the_unadmitted_probe_reports_a_torn_file_as_corrupt_and_leaves_no_marker() {
    let dir = TempDir::new().unwrap();
    let db = page_corrupted_db(dir.path());
    let err = heal::verify_and_mark_unadmitted(&db).unwrap_err();
    assert!(err.is_index_corrupt(), "{err:?}");
    assert!(!marker(&db).exists());
}

/// Catches: the probe creating an empty database at a path that does not exist
/// (SQLite `open` creates), which would turn "no store yet" into a store.
#[test]
fn the_unadmitted_probe_on_a_missing_file_creates_nothing() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("index.sqlite");
    heal::verify_and_mark_unadmitted(&db).unwrap();
    assert!(!db.exists() && !marker(&db).exists());
}

/// Catches: the stamp comparison seeing the probe's own footprint (or a
/// WAL-mode database with a live holder, the daemon's real shape) as "a write
/// happened", so the store is never certified and every later cycle re-scans.
#[test]
fn a_quiet_wal_database_with_a_live_holder_is_certified() {
    let dir = TempDir::new().unwrap();
    let db = dir.path().join("index.sqlite");
    let holder = Connection::open(&db).unwrap();
    holder
        .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE t (x); INSERT INTO t VALUES (1);")
        .unwrap();
    heal::verify_and_mark_unadmitted(&db).unwrap();
    assert!(marker(&db).exists(), "quiet WAL store never certified");
    drop(holder);
}

/// Catches: the refactor of `handle_update_force` dropping the closing probe for
/// the CLI/startup callers (the marker is invalidated at the start, so only the
/// probe can bring it back) -- and the unverified variant leaking a marker.
#[test]
fn handle_update_certifies_and_the_unverified_variant_does_not() {
    let dir = TempDir::new().unwrap();
    let ctx = Context::init(dir.path()).unwrap();
    let db = ctx.db_path.clone();
    handle_update_unverified(&ctx, dir.path()).unwrap();
    assert!(!marker(&db).exists(), "unverified update left a marker");
    handle_update(&ctx, dir.path()).unwrap();
    assert!(marker(&db).exists(), "handle_update no longer probes");
}

/// Catches: `handle_update_unverified` returning with the project mutation lock
/// still held (the watcher would then deadlock its own probe or the next
/// writer).
#[test]
fn the_unverified_update_releases_the_mutation_lock() {
    let dir = TempDir::new().unwrap();
    let ctx = Context::init(dir.path()).unwrap();
    handle_update_unverified(&ctx, dir.path()).unwrap();
    assert!(lock_is_free(&ctx.db_path, false));
}
