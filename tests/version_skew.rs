//! Version skew is a second writer with a different mental model of the schema.
//!
//! Story 020-9824, measured on one machine: the daemon had been up since
//! 08-05 17:11 while `target/release/mdkb` was rebuilt on 08-07 09:22, with
//! schema v18 and v19 landing in between. One-shot CLI writers and the
//! long-lived daemon were different builds of the same program writing one file,
//! and the newer CLI migrated a store the older daemon kept writing under its
//! previous assumptions.
//!
//! Two rules, because the skew has two directions. A binary that opens a store
//! *newer* than it understands must refuse. A daemon whose executable is
//! replaced underneath it must retire, so the next call spawns a matching one.

use std::io::{Seek, SeekFrom, Write};

use mdkb::cli::handlers::handle_init;
use mdkb::core::Context;
use mdkb::store::schema::SCHEMA_VERSION;

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    (dir, root)
}

fn set_schema_version(root: &std::path::Path, version: i32) {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.execute("UPDATE schema_version SET version = ?1", [version])
        .expect("set version");
}

fn read_schema_version(root: &std::path::Path) -> i32 {
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("read version")
}

/// Opening a store from the future must fail, naming both versions.
///
/// `init_schema` used to fall through its `_ =>` arm here and carry on: no
/// migration runs (correctly — there is nothing to migrate forward), and the
/// binary then reads and writes tables whose shape it does not know. It also
/// re-runs `SCHEMA_SQL`, so any table or trigger the newer version redefined is
/// left as whichever definition the older binary happens to carry. That is the
/// second-writer-with-a-different-model failure in its purest form.
#[test]
fn a_store_newer_than_the_binary_is_refused() {
    let (_dir, root) = store();
    let future = SCHEMA_VERSION + 3;
    set_schema_version(&root, future);

    let db = root.join(".mdkb/index.sqlite");
    {
        let conn = rusqlite::Connection::open(&db).expect("open for stable snapshot");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("disable WAL for byte comparison");
        conn.execute_batch("VACUUM")
            .expect("stabilize database bytes");
    }
    let before = std::fs::read(&db).expect("read database before refusal");

    let err = Context::open(&root).expect_err("a store from the future must not open");
    let msg = err.to_string();
    assert!(
        msg.contains(&future.to_string()) && msg.contains(&SCHEMA_VERSION.to_string()),
        "the error must name both versions so the operator knows which binary is \
         stale; got: {msg}"
    );

    assert_eq!(
        read_schema_version(&root),
        future,
        "the refusal must leave the recorded version alone — writing it back down \
         is the exact silent downgrade this rule exists to prevent"
    );
    assert_eq!(
        std::fs::read(&db).expect("read database after refusal"),
        before,
        "refusing a future schema must happen before SCHEMA_SQL or FTS ranking writes"
    );
}

#[test]
fn a_damaged_store_from_the_future_is_refused_before_autoheal() {
    let (_dir, root) = store();
    let future = SCHEMA_VERSION + 3;
    let db = root.join(".mdkb/index.sqlite");
    let (index_root, page_size) = {
        let conn = rusqlite::Connection::open(&db).expect("open fixture");
        conn.execute("UPDATE schema_version SET version = ?1", [future])
            .expect("set future version");
        conn.pragma_update(None, "journal_mode", "DELETE")
            .expect("disable WAL");
        conn.execute_batch("VACUUM").expect("stabilize fixture");
        let index_root: u64 = conn
            .query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name = 'idx_memory_status'",
                [],
                |row| row.get(0),
            )
            .expect("read index rootpage");
        let page_size: u64 = conn
            .query_row("PRAGMA page_size", [], |row| row.get(0))
            .expect("read page size");
        (index_root, page_size)
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&db)
        .expect("open index page for corruption");
    file.seek(SeekFrom::Start((index_root - 1) * page_size))
        .expect("seek to index rootpage");
    file.write_all(&[0]).expect("invalidate b-tree page type");
    file.sync_all().expect("persist fixture corruption");
    drop(file);
    {
        let conn =
            rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open damaged fixture read-only");
        assert_eq!(
            mdkb::store::schema::get_schema_version(&conn).expect("version remains readable"),
            Some(future)
        );
        let quick_check = conn.query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0));
        assert!(
            !matches!(quick_check, Ok(ref result) if result == "ok"),
            "fixture must be structurally damaged"
        );
    }
    let before = std::fs::read(&db).expect("read damaged future store");

    let err = Context::open(&root).expect_err("damaged future store must still be refused");
    let message = err.to_string();
    assert!(
        message.contains(&future.to_string()) && message.contains(&SCHEMA_VERSION.to_string()),
        "readable future version must win over autoheal; got: {message}"
    );
    assert_eq!(
        std::fs::read(&db).expect("read store after refusal"),
        before,
        "an older binary must not quarantine or rewrite a damaged future store"
    );
    assert!(
        !std::fs::read_dir(root.join(".mdkb"))
            .expect("read store directory")
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains(".corrupt-")),
        "future-version refusal must happen before quarantine"
    );
}

/// The counterpart. A store at the expected version, and one legitimately behind
/// it, must both open — a rule that fires on an ordinary upgrade is worse than
/// no rule, because it will be disabled.
#[test]
fn a_current_or_older_store_opens_normally() {
    let (_dir, root) = store();
    Context::open(&root).expect("a store at the current version must open");

    set_schema_version(&root, 11);
    Context::open(&root).expect("an older store must still migrate forward");
    assert_eq!(
        read_schema_version(&root),
        SCHEMA_VERSION,
        "a forward migration must still record the new version"
    );
}

/// A daemon holds its connection for days while the binary it was launched from
/// is rebuilt underneath it. The recorded identity of that file is what tells it
/// to stand down.
#[test]
fn a_replaced_executable_is_detected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = dir.path().join("mdkb");
    std::fs::write(&exe, b"build one").unwrap();

    let launched_as = mdkb::daemon::ExeIdentity::of(&exe).expect("identity of a real file");
    assert!(
        !launched_as.changed(),
        "an untouched executable must never look replaced — a check that fires \
         on a normal restart gets turned off"
    );

    // `cargo build` replaces the file; size and mtime both move.
    std::fs::write(&exe, b"build two, which is longer").unwrap();
    assert!(
        launched_as.changed(),
        "a rebuilt executable must be detected, or the daemon keeps serving a \
         store its binary no longer understands"
    );
}

/// A deleted executable is skew too — the daemon is running code no file on disk
/// corresponds to — but it must not be mistaken for "unchanged".
#[test]
fn a_deleted_executable_counts_as_changed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = dir.path().join("mdkb");
    std::fs::write(&exe, b"build one").unwrap();
    let launched_as = mdkb::daemon::ExeIdentity::of(&exe).expect("identity");

    std::fs::remove_file(&exe).unwrap();
    assert!(
        launched_as.changed(),
        "an executable that no longer exists cannot be vouched for"
    );
}
