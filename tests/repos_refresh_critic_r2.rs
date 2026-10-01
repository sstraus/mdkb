//! Critic round 2 for story 219-9d67: attacks on the in-transaction migration
//! DDL and on the failure report.
//!
//! The version fixtures under `tests/fixtures/schema_v*.sql` are the statements
//! `init_schema` executed at the commit that set that `SCHEMA_VERSION`, cut out
//! of `git show <commit>:src/store/schema.rs` by script (v22 0166725, v30
//! dd3cf2d, v32 247bde2, v33 296b415). The stores on the maintainer's machine
//! sit at v22, v30, v32 and v33; v17 alone proves nothing about them.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use mdkb::core::refresh::{RefreshStatus, refresh_store};
use mdkb::store::schema::{SCHEMA_VERSION, init_schema};
use rusqlite::Connection;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");

const FIXTURES: &[(i32, &str)] = &[
    (17, include_str!("fixtures/schema_v17.sql")),
    (22, include_str!("fixtures/schema_v22.sql")),
    (30, include_str!("fixtures/schema_v30.sql")),
    (32, include_str!("fixtures/schema_v32.sql")),
    (33, include_str!("fixtures/schema_v33.sql")),
];

fn sql_for(version: i32) -> &'static str {
    FIXTURES
        .iter()
        .find(|(v, _)| *v == version)
        .expect("fixture for version")
        .1
}

/// A store built from the recorded DDL of `version`, with three memory entries
/// (one with a blank id, which the v21 migration deletes), one document and one
/// prior cluster.
fn old_store(parent: &Path, name: &str, version: i32) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(root.join(".mdkb")).expect("create store dir");
    let root = mdkb::domain::canonicalize_plain(&root).expect("canonicalize");
    let conn = Connection::open(root.join(".mdkb/index.sqlite")).expect("create index");
    conn.execute_batch(sql_for(version)).expect("build schema");
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [version],
    )
    .expect("record version");
    conn.execute_batch(
        "INSERT INTO collections (name, path, created_at, updated_at) VALUES ('docs', 'docs', 1, 1);
         INSERT INTO content (hash, body, created_at) VALUES ('h1', 'quokka body', 1);
         INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES ('docs', 'a.md', 'h1', 'Quokka', 1, 1);
         INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('keep-one', 'One', 'wombat body', 'topic', '[]', 1, 1),
                    ('keep-two', 'Two', 'numbat body', 'decision', '[]', 2, 2);
         INSERT INTO prior_clusters (id, canonical_trigger_key, trigger_kind, trigger_matcher,
                                     lesson, scope, created_at, last_seen_at)
             VALUES ('c1', 'k1', 'prompt', '{}', 'lesson', '{}', 1, 1);",
    )
    .expect("seed rows");
    root
}

/// The id-guard trigger of v21 did not exist before it, so a blank id can be
/// planted in a store at v21 or lower.
fn plant_blank_id(root: &Path) {
    Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open")
        .execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('   ', 'Blank', 'blank id body', 'topic', '[]', 3, 3)",
            [],
        )
        .expect("plant a blank id");
}

/// A trigger that makes the v21 purge fail. Everything the migration did before
/// that statement — the added columns, and with the round-2 change the created
/// tables, indexes and triggers — has to be undone with it.
fn poison_the_purge(root: &Path) {
    Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open")
        .execute_batch(
            "CREATE TRIGGER poison BEFORE DELETE ON memory_entries \
             BEGIN SELECT RAISE(ABORT, 'poison'); END;",
        )
        .expect("plant poison");
}

fn unpoison(root: &Path) {
    Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open")
        .execute_batch("DROP TRIGGER poison;")
        .expect("drop poison");
}

fn objects(conn: &Connection) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare("SELECT type || ':' || name FROM sqlite_master ORDER BY 1")
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("collect")
}

fn columns(conn: &Connection, table: &str) -> BTreeSet<String> {
    let mut stmt = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("collect")
}

fn version_of(root: &Path) -> i32 {
    Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open")
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("version")
}

fn count(root: &Path, table: &str) -> i64 {
    Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open")
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .expect("count")
}

/// Catches: a `SCHEMA_SQL` index, trigger or table that only works on a v17
/// store, or a migration step that does not survive the shape of a v22, v30,
/// v32 or v33 store — the versions the real stores are at. The round-2 fixture
/// covers v17 only.
#[test]
fn stores_at_every_recorded_version_migrate_to_current_without_losing_rows() {
    for (version, _) in FIXTURES {
        let parent = tempfile::tempdir().expect("parent");
        let root = old_store(parent.path(), "old", *version);

        let status = refresh_store(&root, "t1")
            .unwrap_or_else(|f| panic!("v{version} store failed to migrate: {f:?}"));

        let RefreshStatus::Migrated(m) = status else {
            panic!("v{version}: expected a migration, got {status:?}");
        };
        assert_eq!(m.from, *version, "v{version}");
        assert_eq!(version_of(&root), SCHEMA_VERSION, "v{version}");
        assert_eq!(count(&root, "memory_entries"), 2, "v{version} memory rows");
        assert_eq!(count(&root, "documents"), 1, "v{version} documents");
        assert_eq!(
            count(&root, "prior_clusters"),
            1,
            "v{version} prior clusters"
        );
        let conn = Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .expect("integrity");
        assert_eq!(integrity, "ok", "v{version}");
        let found: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH 'wombat'",
                [],
                |r| r.get(0),
            )
            .expect("fts");
        assert_eq!(found, 1, "v{version}: the memory FTS finds the old row");
    }
}

/// Catches: the migration rolling back its version row but not the tables,
/// columns, triggers and indexes created before the failing step — the
/// "half old, half new" store the round-2 change claims to prevent. Also
/// catches a connection left inside an open transaction after the failure.
#[test]
fn a_migration_that_fails_midway_leaves_the_store_byte_for_byte_as_it_was() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "old", 17);
    plant_blank_id(&root);
    poison_the_purge(&root);
    let db = root.join(".mdkb/index.sqlite");

    let conn = Connection::open(&db).expect("open");
    let before = objects(&conn);
    let before_cols = columns(&conn, "memory_entries");

    let err = init_schema(&conn).expect_err("the poisoned purge must fail the migration");

    assert!(err.to_string().contains("poison"), "{err}");
    assert!(
        conn.is_autocommit(),
        "the failed migration left its transaction open"
    );
    assert_eq!(objects(&conn), before, "tables/triggers/indexes were kept");
    assert_eq!(columns(&conn, "memory_entries"), before_cols);
    drop(conn);
    assert_eq!(version_of(&root), 17);
    assert_eq!(count(&root, "memory_entries"), 3);
}

/// Catches: a failed refresh that leaves the store unusable for the retry
/// (a half-applied migration whose second run trips over its own leftovers),
/// or a failure report that claims the migration committed when it rolled back.
#[test]
fn a_refresh_that_failed_in_the_migration_reports_the_old_schema_and_a_retry_succeeds() {
    let parent = tempfile::tempdir().expect("parent");
    let root = old_store(parent.path(), "old", 17);
    plant_blank_id(&root);
    poison_the_purge(&root);

    let failure = refresh_store(&root, "t1").expect_err("the poisoned purge must fail");

    assert_eq!(failure.schema_after, Some(17), "{failure:?}");
    let backup = failure
        .backup
        .expect("the copy was taken before the failure");
    assert!(backup.exists());
    assert_eq!(version_of(&root), 17);

    unpoison(&root);
    let status = refresh_store(&root, "t2").expect("the retry migrates");
    let RefreshStatus::Migrated(m) = status else {
        panic!("expected a migration, got {status:?}");
    };
    assert_eq!((m.from, m.memory_before, m.memory_after), (17, 3, 2));
    assert_eq!(version_of(&root), SCHEMA_VERSION);
    assert!(backup.exists(), "the first backup was not overwritten");
}

fn run_refresh(home: &Path, cwd: &Path, namespace: Option<&str>) -> (bool, String) {
    let mut cmd = Command::new(BIN);
    cmd.args(["repos", "refresh", "--only", "outdated"])
        .env("HOME", home)
        .env_remove("MDKB_NAMESPACE")
        .current_dir(cwd);
    if let Some(ns) = namespace {
        cmd.env("MDKB_NAMESPACE", ns);
    }
    let out = cmd.output().expect("run mdkb");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

fn register(home: &Path, root: &Path) {
    let map_dir = home.join(".mdkb");
    std::fs::create_dir_all(&map_dir).expect("home .mdkb");
    mdkb::daemon::repo_map::RepoMap::open(Some(map_dir.join("repos.json")), &[]).record(root);
}

/// Catches: the FAILED line for a rolled-back migration saying the store is
/// migrated (or saying nothing about its state), and the summary error still
/// claiming "unchanged" for stores that may be migrated.
#[test]
fn the_failed_line_of_a_rolled_back_store_says_it_is_still_at_its_old_schema() {
    let parent = tempfile::tempdir().expect("parent");
    let home = tempfile::tempdir().expect("home");
    let root = old_store(parent.path(), "old", 17);
    plant_blank_id(&root);
    poison_the_purge(&root);
    register(home.path(), &root);

    let (ok, text) = run_refresh(home.path(), parent.path(), Some("default"));

    assert!(!ok, "{text}");
    assert!(text.contains("FAILED"), "{text}");
    assert!(text.contains("schema v17, as before"), "{text}");
    assert!(!text.contains("migration committed"), "{text}");
    assert!(
        text.contains("pre-migrate-v17"),
        "backup path shown: {text}"
    );
}

/// Catches: the namespace refusal firing for a process that explicitly selected
/// the default store (`MDKB_NAMESPACE=default`), which would make refresh
/// unusable from a test runner's environment; and an empty value being treated
/// as a namespace.
#[test]
fn refresh_runs_with_namespace_default_and_with_an_empty_namespace() {
    for namespace in [Some("default"), Some(""), None] {
        let parent = tempfile::tempdir().expect("parent");
        let home = tempfile::tempdir().expect("home");
        let root = old_store(parent.path(), "old", 17);
        register(home.path(), &root);

        let (ok, text) = run_refresh(home.path(), parent.path(), namespace);

        assert!(ok, "namespace {namespace:?}: {text}");
        assert_eq!(version_of(&root), SCHEMA_VERSION, "namespace {namespace:?}");
    }
}

/// Catches: the namespace refusal running after the store list is built or
/// after a store was touched — with several stores registered, none may move
/// when the refusal fires, and an invalid namespace name must also refuse
/// rather than fall through to a refresh.
#[test]
fn refusal_under_a_namespace_leaves_every_registered_store_untouched() {
    let parent = tempfile::tempdir().expect("parent");
    let home = tempfile::tempdir().expect("home");
    let a = old_store(parent.path(), "a", 17);
    let b = old_store(parent.path(), "b", 30);
    register(home.path(), &a);
    register(home.path(), &b);

    for namespace in ["scratch", "../escape"] {
        let (ok, text) = run_refresh(home.path(), parent.path(), Some(namespace));
        assert!(!ok, "{namespace}: {text}");
        assert!(!text.contains("FAILED"), "{namespace}: {text}");
    }

    assert_eq!(version_of(&a), 17);
    assert_eq!(version_of(&b), 30);
}
