//! `mdkb repos refresh` against a store that is genuinely v17 (story 219-9d67).
//!
//! The other refresh tests age a current store by rewriting its version, so
//! every column a later migration adds is already there. This one builds the
//! store from `tests/fixtures/schema_v17.sql`, the statements this repository's
//! own `init_schema` ran at schema 17 (recorded from git history at 44cc461^,
//! not written by hand), so `last_refuted_at`, `triggers`, `last_audited_at`
//! and the rest are really missing and the current `SCHEMA_SQL` has to cope.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;

use mdkb::core::refresh::{RefreshStatus, refresh_store};
use mdkb::store::schema::SCHEMA_VERSION;
use rusqlite::Connection;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");
const V17_SQL: &str = include_str!("fixtures/schema_v17.sql");

fn v17_store(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(root.join(".mdkb")).expect("create store dir");
    let root = mdkb::domain::canonicalize_plain(&root).expect("canonicalize");
    let conn = Connection::open(root.join(".mdkb/index.sqlite")).expect("create index");
    conn.execute_batch(V17_SQL).expect("build the v17 schema");
    conn.execute("INSERT INTO schema_version (version) VALUES (17)", [])
        .expect("record v17");
    conn.execute_batch(
        "INSERT INTO collections (name, path, created_at, updated_at) VALUES ('docs', 'docs', 1, 1);
         INSERT INTO content (hash, body, created_at) VALUES ('h1', 'the quokka document body', 1);
         INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES ('docs', 'a.md', 'h1', 'Quokka', 1, 1);
         INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('entry-one', 'First', 'wombat memory body', 'topic', '[\"x\"]', 1, 1),
                    ('entry-two', 'Second', 'numbat memory body', 'decision', '[]', 2, 2);",
    )
    .expect("seed rows");
    root
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut stmt = conn
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .expect("prepare");
    stmt.query_map([], |r| r.get(0))
        .expect("query")
        .collect::<rusqlite::Result<_>>()
        .expect("collect")
}

/// Catches: a `SCHEMA_SQL` index or trigger that names a column a later
/// migration adds, which fails on a real old store and is invisible on a
/// relabelled current one; and a migration that drops or damages rows on the
/// way to v34.
#[test]
fn a_genuine_v17_store_migrates_to_current_with_its_rows_and_objects_intact() {
    let parent = tempfile::tempdir().expect("parent");
    let root = v17_store(parent.path(), "old");
    let before = Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    assert!(
        !columns(&before, "memory_entries").contains(&"last_refuted_at".to_string()),
        "the fixture must really lack what later migrations add"
    );
    drop(before);

    let status = refresh_store(&root, "t1").expect("refresh");
    let RefreshStatus::Migrated(m) = status else {
        panic!("expected a migration, got {status:?}");
    };
    assert_eq!((m.from, m.memory_before, m.memory_after), (17, 2, 2));

    let conn = Connection::open(root.join(".mdkb/index.sqlite")).expect("open");
    let version: i32 = conn
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, SCHEMA_VERSION);
    let cols = columns(&conn, "memory_entries");
    for added in [
        "projected_hash",
        "last_refuted_at",
        "last_audited_at",
        "triggers",
    ] {
        assert!(
            cols.contains(&added.to_string()),
            "{added} missing: {cols:?}"
        );
    }
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .expect("integrity");
    assert_eq!(integrity, "ok");
    let fk_violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .expect("fk check");
    assert_eq!(fk_violations, 0);

    // The triggers and FTS tables the migration left behind still work: a
    // write fires the update trigger, and a search still finds the old rows.
    conn.execute(
        "UPDATE memory_entries SET title = 'Renamed' WHERE id = 'entry-one'",
        [],
    )
    .expect("memory_au fires on the migrated store");
    let found: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_fts WHERE memory_fts MATCH 'numbat'",
            [],
            |r| r.get(0),
        )
        .expect("memory fts");
    assert_eq!(found, 1, "the migrated memory FTS still finds the old row");
    let doc: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM documents_fts WHERE documents_fts MATCH 'quokka'",
            [],
            |r| r.get(0),
        )
        .expect("documents fts");
    assert_eq!(doc, 1, "and the document FTS the old row");
    let rejected = conn.execute(
        "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at) \
         VALUES ('   ', 't', 'c', 'topic', '[]', 1, 1)",
        [],
    );
    assert!(rejected.is_err(), "the v21 id guard is in place");

    let copy = Connection::open(&m.backup).expect("open backup");
    assert!(
        !columns(&copy, "memory_entries").contains(&"triggers".to_string()),
        "the backup is the store as it was, still v17"
    );
}

/// Catches: a failure after the migration committed reported as "unchanged".
/// A foreign index named `sessions` makes the stats-table creation that runs
/// after the migration fail.
#[test]
fn a_failure_after_the_migration_committed_reports_the_store_as_migrated() {
    let parent = tempfile::tempdir().expect("parent");
    let root = v17_store(parent.path(), "old");
    Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open")
        .execute_batch("CREATE INDEX sessions ON memory_entries(id);")
        .expect("plant a name the stats schema needs");

    let failure = refresh_store(&root, "t1").expect_err("the post-migration step must fail");

    assert_eq!(
        failure.schema_after,
        Some(SCHEMA_VERSION),
        "the migration committed and the report says so: {failure:?}"
    );
    assert!(failure.backup.is_some());
}

/// Catches: `MDKB_NAMESPACE` silently redirecting every default store path so
/// each one fails as "not found".
#[test]
fn refresh_refuses_under_a_namespace_and_touches_nothing() {
    let parent = tempfile::tempdir().expect("parent");
    let home = tempfile::tempdir().expect("home");
    let root = v17_store(parent.path(), "old");
    let map_dir = home.path().join(".mdkb");
    std::fs::create_dir_all(&map_dir).expect("home .mdkb");
    mdkb::daemon::repo_map::RepoMap::open(Some(map_dir.join("repos.json")), &[]).record(&root);

    let out = Command::new(BIN)
        .args(["repos", "refresh", "--only", "outdated"])
        .env("HOME", home.path())
        .env("MDKB_NAMESPACE", "scratch")
        .current_dir(parent.path())
        .output()
        .expect("run mdkb");

    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("MDKB_NAMESPACE"), "{text}");
    let version: i32 = Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open")
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("version");
    assert_eq!(version, 17);
    assert!(
        !std::fs::read_dir(root.join(".mdkb"))
            .expect("read dir")
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains("pre-migrate")),
        "no backup was taken"
    );
}
