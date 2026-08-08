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
