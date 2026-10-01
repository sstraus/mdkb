//! `mdkb repos refresh --only outdated` and the schema v34 `index_head` record
//! (stories 219-9d67 and 220-711b).
//!
//! The stores here are real current-schema stores whose recorded version is
//! rolled back, the convention of `e2e_read_migrates.rs`: every migration step
//! probes before it acts, so replaying v18..v34 over a current store runs the
//! same statements a real old store would. Nothing here reads or writes the
//! real `~/.mdkb`: every store lives in a temp dir and the CLI runs with
//! `HOME` pointed at another.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use mdkb::core::Context;
use mdkb::core::refresh::{RefreshStatus, refresh_store};
use mdkb::store::memory::{
    EntryStatus, EntryType, MemoryEntry, PRIOR_TTL_SECS, SourceType, add_entry,
};
use mdkb::store::schema::SCHEMA_VERSION;
use rusqlite::Connection;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");
const MINED_PRIOR_CREATED: i64 = 1_000_000;

fn entry(id: &str, entry_type: EntryType, source: SourceType) -> MemoryEntry {
    MemoryEntry {
        triggers: Vec::new(),
        id: id.to_string(),
        title: format!("Title of {id}"),
        content: format!("Body of {id}"),
        entry_type,
        tags: Vec::new(),
        status: EntryStatus::Active,
        created_at: MINED_PRIOR_CREATED,
        updated_at: MINED_PRIOR_CREATED,
        superseded_by: None,
        access_count: 0,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        corrections: 0,
        last_confirmed_at: None,
        last_refuted_at: None,
        source_type: source,
        expires_at: None,
        due_at: None,
    }
}

fn db(root: &Path) -> Connection {
    Connection::open(root.join(".mdkb/index.sqlite")).expect("open store")
}

fn version_of(root: &Path) -> i32 {
    db(root)
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("read version")
}

fn insert_cluster(conn: &Connection, id: &str, kind: &str) {
    conn.execute(
        "INSERT INTO prior_clusters (id, canonical_trigger_key, trigger_kind, trigger_matcher, \
         lesson, scope, created_at, last_seen_at) \
         VALUES (?1, ?1, ?2, '{\"command_contains\":\"cargo test\"}', ?3, '{}', 1, 1)",
        rusqlite::params![id, kind, format!("lesson of {id}")],
    )
    .expect("insert cluster");
}

/// A store with memory rows and prior clusters, then rolled back to `version`.
///
/// Four readable entries (one mined prior written without a TTL, one prior a
/// person stated), one entry with an unreadable id (what v21 evicts), a cluster
/// a trigger can reach and one whose trigger kind has no injection point (what
/// v23 archives).
fn old_store(parent: &Path, name: &str, version: i32) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("create root");
    let root = mdkb::domain::canonicalize_plain(&root).expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    {
        let ctx = Context::open(&root).expect("open");
        add_entry(
            &ctx.conn,
            &entry("keep-a", EntryType::Topic, SourceType::UserStatement),
        )
        .expect("add");
        add_entry(
            &ctx.conn,
            &entry("keep-b", EntryType::Decision, SourceType::UserStatement),
        )
        .expect("add");
        add_entry(
            &ctx.conn,
            &entry("mined-prior", EntryType::Prior, SourceType::AutoExtracted),
        )
        .expect("add");
        add_entry(
            &ctx.conn,
            &entry("stated-prior", EntryType::Prior, SourceType::UserStatement),
        )
        .expect("add");
        insert_cluster(&ctx.conn, "reachable", "pre_tool");
        insert_cluster(&ctx.conn, "unreachable", "stop");
    }
    let conn = db(&root);
    // Roll the store back for real: drop what the old schema did not have, so
    // a refresh that only rewrote the version number would be caught.
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_entries_id_guard_bi;
         DROP TRIGGER IF EXISTS memory_entries_id_guard_bu;
         DROP TABLE IF EXISTS index_head;",
    )
    .expect("drop what v34 added");
    conn.execute(
        "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at) \
         VALUES ('   ', 'Blank id', 'c', 'topic', '[]', 1, 1)",
        [],
    )
    .expect("unreadable id");
    conn.execute("UPDATE schema_version SET version = ?1", [version])
        .expect("roll back");
    root
}

/// A store whose prior-cluster archive step fails, so the migration fails after
/// earlier steps have already written.
fn make_migration_fail(root: &Path) {
    db(root)
        .execute_batch(
            "CREATE TRIGGER refuse_cluster_updates BEFORE UPDATE ON prior_clusters \
             BEGIN SELECT RAISE(ABORT, 'refused by the test'); END;",
        )
        .expect("install failing trigger");
}

fn memory_ids(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT id FROM memory_entries ORDER BY id")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("collect")
}

fn expires_at(conn: &Connection, id: &str) -> Option<i64> {
    conn.query_row(
        "SELECT expires_at FROM memory_entries WHERE id = ?1",
        [id],
        |r| r.get(0),
    )
    .expect("expires_at")
}

fn cluster_state(conn: &Connection, id: &str) -> String {
    conn.query_row(
        "SELECT state FROM prior_clusters WHERE id = ?1",
        [id],
        |r| r.get(0),
    )
    .expect("cluster state")
}

fn migrated(status: RefreshStatus) -> mdkb::core::refresh::Migrated {
    match status {
        RefreshStatus::Migrated(m) => m,
        other => panic!("expected a migration, got {other:?}"),
    }
}

/// A v17 store carries every data-changing step: v21 evicts the unreadable id,
/// v22 dates the mined prior, v23 archives the unreachable cluster.
///
/// Catches: a migration that drops readable memory rows along with the
/// unreadable one, rewrites the prior a person stated, or archives a cluster
/// whose trigger a hook can still match.
#[test]
fn a_v17_store_keeps_its_memory_rows_and_only_the_planned_cluster_changes() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "old", 17);

    let m = migrated(refresh_store(&root, "t1").expect("refresh"));

    assert_eq!((m.from, m.memory_before, m.memory_after), (17, 5, 4));
    let conn = db(&root);
    assert_eq!(version_of(&root), SCHEMA_VERSION);
    assert_eq!(
        memory_ids(&conn),
        ["keep-a", "keep-b", "mined-prior", "stated-prior"],
        "every readable entry survives; only the unreadable id is evicted"
    );
    assert_eq!(
        expires_at(&conn, "mined-prior"),
        Some(MINED_PRIOR_CREATED + PRIOR_TTL_SECS),
        "the mined prior is dated from its creation"
    );
    assert_eq!(expires_at(&conn, "stated-prior"), None, "a stated prior is not");
    assert_eq!(cluster_state(&conn, "reachable"), "candidate");
    assert_eq!(cluster_state(&conn, "unreachable"), "archived");
    let lesson: String = conn
        .query_row(
            "SELECT lesson FROM prior_clusters WHERE id = 'unreachable'",
            [],
            |r| r.get(0),
        )
        .expect("lesson");
    assert_eq!(lesson, "lesson of unreachable", "archived, not rewritten");
}

/// v32 → v34 changes no row: the whole of both tables is identical afterwards.
///
/// Catches: a recent-version migration that touches data it has no business
/// with, and the `index_head` table not being created.
#[test]
fn a_v32_store_gains_index_head_and_no_row_changes() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "recent", 32);
    let dump = |conn: &Connection| -> Vec<String> {
        let mut rows = Vec::new();
        for sql in [
            "SELECT id || '|' || title || '|' || content || '|' || state FROM prior_clusters",
            "SELECT id || '|' || title || '|' || content || '|' || ifnull(expires_at, 'null') FROM memory_entries",
        ] {
            let mut stmt = conn.prepare(sql).expect("prepare");
            let mut part: Vec<String> = stmt
                .query_map([], |r| r.get(0))
                .expect("query")
                .collect::<rusqlite::Result<_>>()
                .expect("collect");
            part.sort();
            rows.extend(part);
        }
        rows
    };
    let before = dump(&db(&root));

    let m = migrated(refresh_store(&root, "t1").expect("refresh"));

    assert_eq!(m.from, 32);
    assert_eq!(dump(&db(&root)), before);
    assert_eq!(version_of(&root), SCHEMA_VERSION);
    let has_table: bool = db(&root)
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'index_head')",
            [],
            |r| r.get(0),
        )
        .expect("probe");
    assert!(has_table, "v34 adds index_head");
}

/// The copy exists, is a store at the OLD schema with every row the original
/// had, and sits next to it.
///
/// Catches: a backup taken after the migration, a copy of the wrong file, a
/// copy that is empty.
#[test]
fn the_backup_is_a_verified_copy_at_the_old_schema_next_to_the_store() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "old", 17);

    let m = migrated(refresh_store(&root, "t1").expect("refresh"));

    assert_eq!(m.backup, root.join(".mdkb/index.sqlite.pre-migrate-v17-t1"));
    let copy = Connection::open(&m.backup).expect("open backup");
    let version: i32 = copy
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, 17, "the copy is the store as it was");
    assert_eq!(memory_ids(&copy).len(), 5, "including the row the migration evicted");
}

/// Fail closed: when the copy cannot be written the store is not opened for
/// writing at all.
///
/// Catches: a migration that goes ahead without a recovery copy. The target is
/// occupied before the run, which is the one way to make a copy fail without
/// depending on file permissions.
#[test]
fn a_store_whose_backup_cannot_be_written_stays_untouched() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "old", 17);
    let occupied = root.join(".mdkb/index.sqlite.pre-migrate-v17-fixed");
    std::fs::write(&occupied, b"an earlier recovery copy").expect("occupy target");
    let before = std::fs::read(root.join(".mdkb/index.sqlite")).expect("read store");

    let failure = refresh_store(&root, "fixed").expect_err("must refuse");

    assert!(failure.backup.is_none(), "{failure:?}");
    assert_eq!(version_of(&root), 17);
    assert_eq!(
        std::fs::read(root.join(".mdkb/index.sqlite")).expect("read store"),
        before,
        "not one byte of the store changed"
    );
    assert_eq!(
        std::fs::read(&occupied).expect("read occupied"),
        b"an earlier recovery copy",
        "and the recovery copy that was already there is not overwritten or removed"
    );
}

/// A migration that fails after earlier steps wrote leaves nothing half-done.
///
/// Catches: a partial migration. v22 dates the mined prior before v23 trips the
/// failing trigger; if the steps were not one transaction the date would stay
/// and the version would not move.
#[test]
fn a_failed_migration_rolls_back_every_step_and_keeps_the_backup() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "old", 17);
    make_migration_fail(&root);

    let failure = refresh_store(&root, "t1").expect_err("the migration must fail");

    assert!(failure.reason.contains("refused by the test"), "{failure:?}");
    let backup = failure.backup.expect("the copy was taken before the attempt");
    assert!(backup.exists());
    let conn = db(&root);
    assert_eq!(version_of(&root), 17);
    assert_eq!(expires_at(&conn, "mined-prior"), None, "v22 was rolled back");
    assert_eq!(memory_ids(&conn).len(), 5, "v21 was rolled back");
    assert_eq!(cluster_state(&conn, "unreachable"), "candidate");
}

#[test]
fn a_current_store_is_left_alone_and_gets_no_backup() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "current", SCHEMA_VERSION);

    let status = refresh_store(&root, "t1").expect("refresh");

    assert!(matches!(status, RefreshStatus::Current), "{status:?}");
    let backups = std::fs::read_dir(root.join(".mdkb"))
        .expect("read dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("pre-migrate"))
        .count();
    assert_eq!(backups, 0);
}

/// A newer store is refused by every other path and migrating cannot help it;
/// refresh must not open it for writing either.
#[test]
fn a_newer_store_is_left_alone() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "future", SCHEMA_VERSION + 5);

    let status = refresh_store(&root, "t1").expect("refresh");

    assert!(matches!(status, RefreshStatus::Newer { .. }), "{status:?}");
    assert_eq!(version_of(&root), SCHEMA_VERSION + 5);
}

/// The command end to end: it reads the daemon's repo map from `HOME`, migrates
/// what is outdated, reports a failure per store without stopping at it, and
/// exits non-zero when any store failed.
///
/// Catches: a refresh that aborts at the first failure and leaves the rest of
/// the fleet outdated, and a failure that exits 0.
#[test]
fn the_command_migrates_every_outdated_store_and_exits_nonzero_on_a_failure() {
    let parent = tempfile::tempdir().expect("parent");
    let home = tempfile::tempdir().expect("home");
    let outdated = old_store(parent.path(), "outdated", 30);
    let broken = old_store(parent.path(), "broken", 17);
    make_migration_fail(&broken);
    let current = old_store(parent.path(), "current", SCHEMA_VERSION);

    let map_dir = home.path().join(".mdkb");
    std::fs::create_dir_all(&map_dir).expect("home .mdkb");
    let map = mdkb::daemon::repo_map::RepoMap::open(Some(map_dir.join("repos.json")), &[]);
    for root in [&outdated, &broken, &current] {
        map.record(root);
    }

    let out = Command::new(BIN)
        .args(["repos", "refresh", "--only", "outdated"])
        .env("HOME", home.path())
        .current_dir(parent.path())
        .output()
        .expect("run mdkb repos refresh");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(!out.status.success(), "a failed store must fail the command: {stdout}");
    assert!(
        stdout.contains("1 migrated, 1 already current, 0 newer than this binary, 1 failed"),
        "{stdout}"
    );
    assert!(stdout.contains("FAILED"), "{stdout}");
    assert!(stdout.contains("pre-migrate-v17"), "the kept backup is named: {stdout}");
    assert_eq!(version_of(&outdated), SCHEMA_VERSION, "the failure did not stop the run");
    assert_eq!(version_of(&broken), 17);

    // A second run has nothing left to do for the migrated store.
    let again = Command::new(BIN)
        .args(["repos", "refresh", "--only", "outdated"])
        .env("HOME", home.path())
        .current_dir(parent.path())
        .output()
        .expect("run mdkb repos refresh");
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("0 migrated, 2 already current"),
        "{}",
        String::from_utf8_lossy(&again.stdout)
    );
}

// ── indexed_head (story 220-711b) ───────────────────────────────────────────

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(root: &Path, file: &str, body: &str) -> String {
    let path = root.join(file);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, body).expect("write");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", file]);
    git(root, &["rev-parse", "HEAD"])
}

/// Every whole-tree update records the commit it indexed; a targeted update
/// does not; an update outside git clears the record.
///
/// Catches: a second index keeping the first commit, a partial `--files` run
/// claiming the whole tree is at HEAD, and a store that left git keeping a
/// commit that no longer describes it.
#[test]
fn a_whole_tree_update_records_head_and_nothing_else_does() {
    let dir = tempfile::tempdir().expect("dir");
    let root = mdkb::domain::canonicalize_plain(dir.path()).expect("canonicalize");
    git(&root, &["init", "-q"]);
    let first = commit(&root, "docs/a.md", "# a");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open");
    // Without a collection a targeted update returns before doing anything,
    // and the assertion below would hold for the wrong reason.
    mdkb::cli::handlers::handle_collection_add(&ctx, "docs", "docs", "**/*.md")
        .expect("add collection");
    assert_eq!(
        mdkb::store::index_head::read(&ctx.conn).expect("read"),
        None,
        "nothing is recorded before an index runs"
    );

    mdkb::cli::handlers::handle_update(&ctx, &root).expect("update");
    assert_eq!(mdkb::store::index_head::read(&ctx.conn).expect("read"), Some(first));

    let second = commit(&root, "docs/b.md", "# b");
    mdkb::core::indexing::handle_update_files(&ctx, &root, &["docs/b.md".to_string()])
        .expect("targeted update");
    assert_ne!(
        mdkb::store::index_head::read(&ctx.conn).expect("read"),
        Some(second.clone()),
        "a targeted run does not make the tree match HEAD"
    );

    mdkb::cli::handlers::handle_update(&ctx, &root).expect("update");
    assert_eq!(
        mdkb::store::index_head::read(&ctx.conn).expect("read"),
        Some(second)
    );

    std::fs::remove_dir_all(root.join(".git")).expect("leave git");
    mdkb::cli::handlers::handle_update(&ctx, &root).expect("update");
    assert_eq!(mdkb::store::index_head::read(&ctx.conn).expect("read"), None);
}
