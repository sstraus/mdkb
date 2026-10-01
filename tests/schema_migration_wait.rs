//! `init_schema` on a stale store while another connection holds the write lock
//! and changes the schema version before releasing it (story 219-9d67, round 4).
//!
//! Connection B takes `BEGIN IMMEDIATE` first. Connection A then opens the same
//! stale store, passes the version check that runs before the lock, and blocks
//! in its own `BEGIN IMMEDIATE`. B writes a version and commits; A resumes and
//! must act on what B left, not on what A saw earlier.

#![cfg(unix)]

use std::path::Path;
use std::time::Duration;

use mdkb::store::schema::{SCHEMA_VERSION, init_schema};
use rusqlite::Connection;

const V17_SQL: &str = include_str!("fixtures/schema_v17.sql");

fn v17_db(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("index.sqlite");
    let conn = Connection::open(&path).expect("create");
    conn.execute_batch(V17_SQL).expect("v17 schema");
    conn.execute("INSERT INTO schema_version (version) VALUES (17)", [])
        .expect("record v17");
    path
}

fn object_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))
        .expect("count")
}

/// Runs `init_schema` on a stale store in a thread that blocks on the write
/// lock; `b_action` runs on the lock holder after A is waiting.
fn init_while_b_holds_the_lock(path: &Path, b_action: &str) -> (mdkb::Result<()>, bool) {
    let b = Connection::open(path).expect("b");
    b.execute("BEGIN IMMEDIATE", []).expect("b locks");
    let a_path = path.to_path_buf();
    let a = std::thread::spawn(move || {
        let conn = Connection::open(a_path).expect("a");
        conn.busy_timeout(Duration::from_secs(10)).expect("timeout");
        let result = init_schema(&conn);
        (result, conn.is_autocommit())
    });
    std::thread::sleep(Duration::from_millis(800));
    b.execute_batch(b_action).expect("b writes");
    b.execute("COMMIT", []).expect("b commits");
    a.join().expect("thread")
}

/// Catches: the in-lock re-read treating any version that is not stale as
/// "already migrated" (`_ => Ok(())`), so a binary older than the store opens
/// it for writing after a newer binary migrated it while this one waited.
#[test]
fn a_store_a_newer_binary_migrated_while_waiting_is_refused_and_untouched() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("index.sqlite");
    drop(v17_db(dir.path()));
    let future = SCHEMA_VERSION + 1;
    let before = object_count(&Connection::open(&path).expect("open"));

    let (result, autocommit) = init_while_b_holds_the_lock(
        &path,
        &format!("UPDATE schema_version SET version = {future};"),
    );

    let message = result.expect_err("must refuse").to_string();
    assert!(message.contains("Refusing to open"), "message: {message}");
    assert!(
        autocommit,
        "the refusing connection must not stay in a transaction"
    );
    let conn = Connection::open(&path).expect("open");
    let version: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, future, "the newer store's version is untouched");
    assert_eq!(
        object_count(&conn),
        before,
        "no table, index or trigger was created"
    );
}

/// Catches: a waiter that re-runs the migration steps (or fails on them) after
/// another connection already brought the store to the current version.
#[test]
fn a_store_migrated_to_current_while_waiting_is_left_alone() {
    let dir = tempfile::tempdir().expect("dir");
    let path = v17_db(dir.path());
    let before = object_count(&Connection::open(&path).expect("open"));

    let (result, autocommit) = init_while_b_holds_the_lock(
        &path,
        &format!("UPDATE schema_version SET version = {SCHEMA_VERSION};"),
    );

    result.expect("a current store is a no-op for the waiter");
    assert!(autocommit);
    let conn = Connection::open(&path).expect("open");
    assert_eq!(
        object_count(&conn),
        before,
        "the waiter ran no migration step"
    );
}

/// Catches: a missing version row inside the lock being read as "nothing to
/// do" (`Ok`), which commits an empty transaction and reports success on a
/// store that has no version at all.
#[test]
fn a_store_whose_version_row_vanished_while_waiting_is_an_error() {
    let dir = tempfile::tempdir().expect("dir");
    let path = v17_db(dir.path());
    let before = object_count(&Connection::open(&path).expect("open"));

    let (result, autocommit) = init_while_b_holds_the_lock(&path, "DELETE FROM schema_version;");

    let message = result.expect_err("must fail").to_string();
    assert!(message.contains("no schema version"), "message: {message}");
    assert!(autocommit);
    let conn = Connection::open(&path).expect("open");
    assert_eq!(object_count(&conn), before, "nothing was half-migrated");
}
