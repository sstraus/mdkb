//! A read against an older store migrates instead of refusing (story 031-cfd5).
//!
//! `Context::open_read_only` refusing a stale store was the right call for the
//! path itself — a read that migrates is a writer — but it left the fleet stuck:
//! nothing migrates a store that only ever gets read. So the read delegates. It
//! does not migrate itself; it hands the job to `Context::open`, which is
//! admitted through the same project writer lock every other writer uses, and
//! then reads read-only.
//!
//! The reduced scope Boss approved has no new IPC method: the read migrates
//! in-process under `acquire_writer`, with or without a daemon.

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use mdkb::core::Context;
use mdkb::store::schema::SCHEMA_VERSION;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");

/// A store with one findable memory entry, at the current schema.
fn seeded_store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open");
    mdkb::cli::handlers::handle_memory_add(
        &ctx,
        "stale-probe",
        "Stale probe",
        "topic",
        None,
        "findable after migration",
        None,
        None,
        None,
        None,
    )
    .expect("add");
    drop(ctx);
    (dir, root)
}

fn set_schema_version(root: &Path, v: i32) {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.execute("UPDATE schema_version SET version = ?1", [v])
        .expect("set version");
}

fn schema_version(root: &Path) -> i32 {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("read version")
}

/// The store must be genuinely older, not merely relabelled: roll the version
/// back AND remove something a real migration puts back, so a read that only
/// rewrote the version number would still fail the assertions below.
fn make_stale(root: &Path) {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.execute_batch("DROP TRIGGER IF EXISTS memory_entries_id_guard_bi;")
        .expect("drop guard");
    conn.execute("UPDATE schema_version SET version = 11", [])
        .expect("set version");
}

fn guard_trigger_exists(root: &Path) -> bool {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'trigger' AND name = 'memory_entries_id_guard_bi'",
        [],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

fn run_search(root: &Path, home: &Path, extra_env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = Command::new(BIN);
    cmd.args(["search", "findable", "--scope", "memory"])
        .current_dir(root)
        .env("HOME", home);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("run mdkb search")
}

#[test]
fn a_read_migrates_a_stale_store_in_process_without_a_daemon() {
    let (_dir, root) = seeded_store();
    let home = tempfile::tempdir().expect("home");
    make_stale(&root);
    assert_eq!(schema_version(&root), 11, "fixture must start stale");
    assert!(!guard_trigger_exists(&root), "fixture must be really old");

    let out = run_search(
        &root,
        home.path(),
        &[("MDKB_NO_DAEMON", "1"), ("MDKB_NO_SPAWN", "1")],
    );
    assert!(
        out.status.success(),
        "a read against a stale store must succeed, not refuse: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("stale-probe"),
        "the retry must return real results, not an empty success: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        schema_version(&root),
        SCHEMA_VERSION,
        "the store must be left migrated, so the next read costs nothing"
    );
    assert!(
        guard_trigger_exists(&root),
        "a real migration ran, not just a version relabel"
    );
}

/// The same read, with a real daemon up. Under the reduced scope there is no
/// migrate IPC: what the daemon changes is contention, since the read now takes
/// the writer lock the daemon also takes for each of its own mutations.
#[test]
fn a_read_migrates_a_stale_store_with_the_daemon_running() {
    let (_dir, root) = seeded_store();
    let home = tempfile::tempdir().expect("home");
    std::fs::create_dir_all(home.path().join(".mdkb")).expect("home .mdkb");

    let started = Command::new(BIN)
        .args(["serve", "--daemon", "--detach"])
        .env("HOME", home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn daemon");
    assert!(started.status.success(), "daemon must start");

    let pid_file = home.path().join(".mdkb").join("daemon.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !pid_file.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(pid_file.exists(), "daemon did not come up within 5s");

    make_stale(&root);
    let out = run_search(&root, home.path(), &[]);

    // Stop the daemon before asserting, so a failure cannot leave it running.
    if let Ok(pid) = std::fs::read_to_string(&pid_file)
        .unwrap_or_default()
        .trim()
        .parse::<u32>()
    {
        // SAFETY: kill is safe; SIGTERM is well-defined.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    }

    assert!(
        out.status.success(),
        "a read against a stale store must succeed with the daemon up: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(schema_version(&root), SCHEMA_VERSION);
    assert!(guard_trigger_exists(&root));
}

/// What a daemon actually changes for this path is contention: the read now
/// takes the same writer lock every other writer takes. Holding that lock while
/// the read runs is the hazard itself, stated without depending on which store a
/// daemon happens to have opened — the read must WAIT for the writer and then
/// migrate, not fail because someone else held the lock.
#[test]
fn a_read_waits_for_a_held_writer_lock_rather_than_failing() {
    let (_dir, root) = seeded_store();
    let home = tempfile::tempdir().expect("home");
    make_stale(&root);

    let db_path = root.join(".mdkb/index.sqlite");
    let (released_tx, released_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let guard = mdkb::store::mutation_lock::acquire_writer(&db_path, "test-holder")
            .expect("hold writer lock");
        std::thread::sleep(Duration::from_millis(500));
        drop(guard);
        released_tx.send(Instant::now()).expect("signal release");
    });
    // Let the holder win the race before the read starts.
    std::thread::sleep(Duration::from_millis(100));

    let out = run_search(
        &root,
        home.path(),
        &[("MDKB_NO_DAEMON", "1"), ("MDKB_NO_SPAWN", "1")],
    );
    let finished = Instant::now();
    let released = released_rx.recv().expect("holder released");
    holder.join().expect("holder thread");

    assert!(
        out.status.success(),
        "a read must wait out a held writer lock, not fail: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        finished > released,
        "the read must have finished AFTER the lock was released — otherwise it \
         migrated without holding the lock, which is the corruption this guards"
    );
    assert_eq!(schema_version(&root), SCHEMA_VERSION);
}

/// Migrating cannot fix a store from the FUTURE, so that direction must keep
/// refusing. If delegation caught it too, an older binary would migrate a store
/// it admits it cannot understand.
#[test]
fn a_newer_store_is_still_refused() {
    let (_dir, root) = seeded_store();
    let home = tempfile::tempdir().expect("home");
    let future = SCHEMA_VERSION + 2;
    set_schema_version(&root, future);

    let out = run_search(
        &root,
        home.path(),
        &[("MDKB_NO_DAEMON", "1"), ("MDKB_NO_SPAWN", "1")],
    );
    assert!(
        !out.status.success(),
        "a future store must still be refused"
    );
    let msg = String::from_utf8_lossy(&out.stderr);
    assert!(
        msg.contains(&future.to_string()) && msg.contains(&SCHEMA_VERSION.to_string()),
        "the refusal must still name both versions: {msg}"
    );
    assert_eq!(
        schema_version(&root),
        future,
        "a refused read must leave the store exactly as it was"
    );
}

/// A migration that cannot run must surface as a fault, promptly. This is the
/// observable half of "the retry happens exactly once": there is no loop, so a
/// migration that fails ends the command instead of being tried again.
#[test]
fn a_migration_that_cannot_run_fails_the_read_instead_of_looping() {
    let (_dir, root) = seeded_store();
    let home = tempfile::tempdir().expect("home");
    make_stale(&root);

    // Read-only store directory: the writer lock file cannot be created, so
    // delegation fails before it can migrate anything.
    let mdkb_dir = root.join(".mdkb");
    let original = std::fs::metadata(&mdkb_dir).expect("stat").permissions();
    let mut locked = original.clone();
    std::os::unix::fs::PermissionsExt::set_mode(&mut locked, 0o500);
    std::fs::set_permissions(&mdkb_dir, locked).expect("lock dir");

    let started = Instant::now();
    let out = run_search(
        &root,
        home.path(),
        &[("MDKB_NO_DAEMON", "1"), ("MDKB_NO_SPAWN", "1")],
    );
    let elapsed = started.elapsed();

    std::fs::set_permissions(&mdkb_dir, original).expect("restore dir");

    assert!(
        !out.status.success(),
        "a failed migration must fail the read: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        elapsed < Duration::from_secs(20),
        "the read must fail promptly, not retry in a loop (took {elapsed:?})"
    );
}

/// The current-schema path must not pay for any of this. A write-capable
/// connection under WAL creates `-wal`/`-shm` the moment it opens, so their
/// absence is externally-observable evidence that delegation never ran.
#[test]
fn a_current_store_still_reads_without_becoming_a_writer() {
    let (_dir, root) = seeded_store();
    let home = tempfile::tempdir().expect("home");

    // Collapse the WAL, so any sidecar afterwards was created by the command.
    {
        let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("checkpoint");
    }
    let wal = root.join(".mdkb/index.sqlite-wal");
    let shm = root.join(".mdkb/index.sqlite-shm");
    assert!(!wal.exists() && !shm.exists(), "fixture must start clean");

    let out = run_search(
        &root,
        home.path(),
        &[("MDKB_NO_DAEMON", "1"), ("MDKB_NO_SPAWN", "1")],
    );
    assert!(out.status.success(), "current-schema read must work");
    assert!(
        !wal.exists() && !shm.exists(),
        "a read on a current store must stay read-only — no delegation, no writer"
    );
}
