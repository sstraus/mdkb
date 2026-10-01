//! Adversarial cases for `mdkb repos refresh --only outdated` (story 219-9d67).
//!
//! Fixtures are real current-schema stores aged by hand: schema_version
//! rewritten AND the artefacts a later migration adds removed, so the migration
//! has something to do. Every store lives in a tempdir; nothing here touches
//! `~/.mdkb` or a real repository.

#![cfg(unix)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use mdkb::core::Context;
use mdkb::core::indexing::{UpdateRequest, update_documents};
use mdkb::core::refresh::{RefreshStatus, refresh_outdated, refresh_store};
use mdkb::daemon::config::DaemonConfig;
use mdkb::daemon::registry::RepoRegistry;
use mdkb::mcp::dispatch::cross_repo_search_impl;
use mdkb::mcp::tools::SearchParams;
use mdkb::store::schema::SCHEMA_VERSION;
use rusqlite::Connection;
use serde_json::json;

#[path = "common/cli.rs"]
mod cli;

// ── fixtures ────────────────────────────────────────────────────────────────

/// A real store at `parent/name`, current schema, closed.
fn store(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("create root");
    let root = mdkb::domain::canonicalize_plain(&root).expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    drop(Context::open(&root).expect("open"));
    root
}

fn db(root: &Path) -> PathBuf {
    root.join(".mdkb/index.sqlite")
}

fn raw(root: &Path) -> Connection {
    Connection::open(db(root)).expect("raw open")
}

fn version(root: &Path) -> i32 {
    raw(root)
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("version")
}

fn insert_entry(conn: &Connection, id: &str) {
    conn.execute(
        "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at) \
         VALUES (?1, 'title', 'content', 'topic', 1, 1)",
        [id],
    )
    .expect("insert entry");
}

fn entry_ids(path: &Path) -> BTreeSet<String> {
    let conn = Connection::open(path).expect("open");
    let mut stmt = conn
        .prepare("SELECT id FROM memory_entries")
        .expect("prepare");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

/// Roll a store back to `v`: version rewritten, and what a later migration
/// would add taken away (id guard triggers, the v34 table).
fn age(root: &Path, v: i32) {
    let conn = raw(root);
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_entries_id_guard_bi;
         DROP TRIGGER IF EXISTS memory_entries_id_guard_bu;
         DROP TABLE IF EXISTS index_head;",
    )
    .expect("strip later artefacts");
    conn.execute("UPDATE schema_version SET version = ?1", [v])
        .expect("set version");
}

fn backups(root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(root.join(".mdkb"))
        .expect("read store dir")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().contains(".pre-migrate-"))
        })
        .collect();
    found.sort();
    found
}

fn dir_listing(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root.join(".mdkb"))
        .expect("read store dir")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn schema_objects(root: &Path) -> BTreeSet<(String, String)> {
    let conn = raw(root);
    let mut stmt = conn
        .prepare("SELECT type, name FROM sqlite_master")
        .expect("prepare");
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

/// Row count of every ordinary (non-virtual) table.
fn table_counts(path: &Path) -> Vec<(String, i64)> {
    let conn = Connection::open(path).expect("open");
    let names: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' AND sql NOT LIKE 'CREATE VIRTUAL%' \
                 ORDER BY name",
            )
            .expect("prepare");
        stmt.query_map([], |r| r.get(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    };
    names
        .into_iter()
        .filter_map(|n| {
            // Shadow tables of virtual tables cannot always be counted plainly.
            conn.query_row(&format!("SELECT COUNT(*) FROM \"{n}\""), [], |r| r.get(0))
                .ok()
                .map(|c| (n, c))
        })
        .collect()
}

fn migrated(status: Result<RefreshStatus, mdkb::core::refresh::RefreshFailure>) -> bool {
    matches!(status, Ok(RefreshStatus::Migrated(_)))
}

// ── backup content ──────────────────────────────────────────────────────────

/// Catches: a backup that only compares the memory count and so passes while
/// other tables are short, or a migration that is judged by the post-state only.
/// Oracle: `VACUUM INTO` taken by the test before the refresh.
#[test]
fn backup_holds_every_table_of_the_old_store_and_the_old_version() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    let conn = raw(&root);
    insert_entry(&conn, "keep-1");
    insert_entry(&conn, "keep-2");
    drop(conn);
    age(&root, 20);

    let oracle = tmp.path().join("oracle.sqlite");
    raw(&root)
        .execute("VACUUM INTO ?1", [oracle.to_str().expect("utf8")])
        .expect("vacuum into");

    let status = refresh_store(&root, "100");
    let Ok(RefreshStatus::Migrated(m)) = status else {
        panic!("expected a migration: {status:?}");
    };
    assert_eq!(table_counts(&m.backup), table_counts(&oracle));
    let backup_version: i32 = Connection::open(&m.backup)
        .expect("open backup")
        .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
        .expect("backup version");
    assert_eq!(backup_version, 20, "the backup is the OLD store");
    assert_eq!(version(&root), SCHEMA_VERSION);
}

/// Catches: a backup taken by copying `index.sqlite` alone, which loses every
/// commit still in the `-wal` (a daemon, or a crashed writer, leaves one).
#[test]
fn rows_that_live_only_in_the_wal_are_in_the_backup() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    let live = raw(&root);
    live.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
        .expect("wal");
    insert_entry(&live, "only-in-wal");
    live.execute_batch(
        "DROP TRIGGER IF EXISTS memory_entries_id_guard_bi;
         DROP TRIGGER IF EXISTS memory_entries_id_guard_bu;
         DROP TABLE IF EXISTS index_head;
         UPDATE schema_version SET version = 20;",
    )
    .expect("age inside the wal");
    let wal = db(&root).with_extension("sqlite-wal");
    assert!(
        std::fs::metadata(&wal)
            .map(|m| m.len() > 0)
            .unwrap_or(false),
        "fixture: the aging must still be in the -wal"
    );

    let status = refresh_store(&root, "101");
    let Ok(RefreshStatus::Migrated(m)) = status else {
        panic!("expected a migration: {status:?}");
    };
    assert!(
        entry_ids(&m.backup).contains("only-in-wal"),
        "backup lost a WAL row"
    );
    assert!(entry_ids(&db(&root)).contains("only-in-wal"));
    drop(live);
}

/// Catches: v21's DELETE reaching rows that ARE readable (a leading-space id is
/// text with content), and a backup that is made after the delete.
#[test]
fn v21_deletes_only_blank_ids_and_the_backup_still_has_them() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    let conn = raw(&root);
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_entries_id_guard_bi;
         DROP TRIGGER IF EXISTS memory_entries_id_guard_bu;
         DROP TABLE IF EXISTS index_head;",
    )
    .expect("strip");
    for id in ["ok-1", " lead", "tail ", "   ", "\t", "\n"] {
        insert_entry(&conn, id);
    }
    conn.execute("UPDATE schema_version SET version = 20", [])
        .expect("version");
    drop(conn);

    let Ok(RefreshStatus::Migrated(m)) = refresh_store(&root, "102") else {
        panic!("expected a migration");
    };
    assert_eq!(m.memory_before, 6);
    assert_eq!(m.memory_after, 3, "only the three blank ids go");
    let kept = entry_ids(&db(&root));
    for id in ["ok-1", " lead", "tail "] {
        assert!(kept.contains(id), "readable id {id:?} was deleted");
    }
    assert_eq!(
        entry_ids(&m.backup).len(),
        6,
        "the backup predates the delete"
    );
}

// ── failure handling ────────────────────────────────────────────────────────

/// A migration step that fails: v21's DELETE hits a trigger that aborts it.
fn store_whose_v21_fails(parent: &Path) -> PathBuf {
    let root = store(parent, "r");
    let conn = raw(&root);
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS memory_entries_id_guard_bi;
         DROP TRIGGER IF EXISTS memory_entries_id_guard_bu;
         DROP TABLE IF EXISTS index_head;",
    )
    .expect("strip");
    insert_entry(&conn, "ok-1");
    insert_entry(&conn, "   ");
    conn.execute_batch(
        "CREATE TRIGGER block_delete BEFORE DELETE ON memory_entries
         BEGIN SELECT RAISE(ABORT, 'delete blocked'); END;
         UPDATE schema_version SET version = 20;",
    )
    .expect("arm");
    root
}

/// Catches: a failure mid-migration that commits part of it or loses rows.
#[test]
fn a_failing_migration_keeps_the_old_version_the_rows_and_a_good_backup() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store_whose_v21_fails(tmp.path());

    let Err(failure) = refresh_store(&root, "103") else {
        panic!("the armed store must not migrate");
    };
    assert_eq!(version(&root), 20, "version moved despite the failure");
    assert_eq!(
        entry_ids(&db(&root)).len(),
        2,
        "rows lost despite the failure"
    );
    let kept = failure.backup.expect("a failure after the copy names it");
    assert_eq!(entry_ids(&kept).len(), 2);
}

/// Catches: "each is unchanged at its old schema" being false. `init_schema`
/// runs SCHEMA_SQL (new tables, triggers) OUTSIDE the migration transaction, so
/// a store whose migration rolled back has still gained the v34 table and the
/// id-guard triggers: a half-migrated store at the old version number.
#[test]
fn a_failing_migration_leaves_the_schema_objects_as_they_were() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store_whose_v21_fails(tmp.path());
    let before = schema_objects(&root);

    assert!(refresh_store(&root, "104").is_err());

    assert_eq!(
        schema_objects(&root),
        before,
        "a failed refresh changed the store's schema objects"
    );
}

/// Catches: a failed run that cannot be retried (stale state left behind), or
/// that deletes the first failure's recovery copy.
#[test]
fn a_failed_store_migrates_on_the_next_run_and_keeps_the_first_backup() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store_whose_v21_fails(tmp.path());
    assert!(refresh_store(&root, "105").is_err());
    raw(&root)
        .execute_batch("DROP TRIGGER block_delete;")
        .expect("disarm");

    assert!(migrated(refresh_store(&root, "106")));
    assert_eq!(version(&root), SCHEMA_VERSION);
    assert_eq!(backups(&root).len(), 2, "both recovery copies are kept");
}

/// Catches: overwriting a recovery copy that already holds the name.
#[test]
fn an_existing_file_at_the_backup_path_is_never_overwritten() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    age(&root, 20);
    let taken = PathBuf::from(format!("{}.pre-migrate-v20-107", db(&root).display()));
    std::fs::write(&taken, b"precious").expect("squat the name");

    assert!(refresh_store(&root, "107").is_err());
    assert_eq!(std::fs::read(&taken).expect("read"), b"precious");
    assert_eq!(version(&root), 20, "no migration without its own copy");
}

/// Catches: a store with a damaged page being migrated (or quarantined) with
/// only a "verified" copy of the damage, and a bad copy left under the name of
/// a recovery copy.
#[test]
fn a_damaged_store_is_not_migrated_and_leaves_no_backup() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    insert_entry(&raw(&root), "ok-1");
    age(&root, 20);
    let (page, size): (i64, i64) = {
        let conn = raw(&root);
        let page = conn
            .query_row(
                "SELECT rootpage FROM sqlite_master WHERE name = 'memory_entries'",
                [],
                |r| r.get(0),
            )
            .expect("rootpage");
        let size = conn
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .expect("page size");
        (page, size)
    };
    // Zero the root page: not a b-tree page any more.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(db(&root))
            .expect("open file");
        f.seek(SeekFrom::Start(((page - 1) * size) as u64))
            .expect("seek");
        f.write_all(&vec![0u8; size as usize]).expect("zero");
    }
    let before = std::fs::read(db(&root)).expect("read");

    assert!(refresh_store(&root, "108").is_err());
    assert_eq!(
        std::fs::read(db(&root)).expect("read"),
        before,
        "store changed"
    );
    assert!(backups(&root).is_empty(), "a bad copy was left behind");
}

/// Catches: stores that are current, newer, absent or not databases being
/// written to, and one bad store stopping the others.
#[test]
fn one_bad_store_does_not_stop_the_run_and_untouched_stores_stay_untouched() {
    let tmp = tempfile::tempdir().expect("tmp");
    let missing = tmp.path().join("missing");
    std::fs::create_dir_all(&missing).expect("dir");
    let garbage = store(tmp.path(), "garbage");
    std::fs::write(db(&garbage), b"this is not a database at all").expect("garbage");
    let empty = store(tmp.path(), "empty");
    std::fs::write(db(&empty), b"").expect("empty");
    let old = store(tmp.path(), "old");
    insert_entry(&raw(&old), "keep");
    age(&old, 20);
    let current = store(tmp.path(), "current");
    let newer = store(tmp.path(), "newer");
    raw(&newer)
        .execute("UPDATE schema_version SET version = 99", [])
        .expect("future");

    let snapshots: Vec<(PathBuf, Vec<String>, Vec<u8>)> = [&garbage, &empty, &current, &newer]
        .iter()
        .map(|r| {
            (
                r.to_path_buf(),
                dir_listing(r),
                std::fs::read(db(r)).expect("read"),
            )
        })
        .collect();

    let reports = refresh_outdated(&[
        missing.clone(),
        garbage.clone(),
        empty.clone(),
        old.clone(),
        current.clone(),
        newer.clone(),
    ]);

    assert_eq!(reports.len(), 6);
    assert!(reports[0].outcome.is_err(), "no store dir");
    assert!(reports[1].outcome.is_err(), "garbage file");
    assert!(reports[2].outcome.is_err(), "empty file");
    assert!(matches!(reports[3].outcome, Ok(RefreshStatus::Migrated(_))));
    assert!(matches!(reports[4].outcome, Ok(RefreshStatus::Current)));
    assert!(matches!(
        reports[5].outcome,
        Ok(RefreshStatus::Newer { found: 99 })
    ));
    for (root, listing, bytes) in snapshots {
        assert_eq!(dir_listing(&root), listing, "{} grew files", root.display());
        assert_eq!(
            std::fs::read(db(&root)).expect("read"),
            bytes,
            "{} written",
            root.display()
        );
    }
}

// ── concurrency ─────────────────────────────────────────────────────────────

/// Catches: two refreshes racing on one store each taking a copy and migrating,
/// or the loser failing instead of finding the store current.
#[test]
fn two_refreshes_racing_on_one_store_migrate_it_once() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    insert_entry(&raw(&root), "keep");
    age(&root, 20);
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (root, barrier) = (root.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                refresh_store(&root, "109")
            })
        })
        .collect();
    let outcomes: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("join"))
        .collect();

    assert!(outcomes.iter().all(Result::is_ok), "{outcomes:?}");
    let migrations = outcomes
        .iter()
        .filter(|o| matches!(o, Ok(RefreshStatus::Migrated(_))))
        .count();
    assert_eq!(migrations, 1, "{outcomes:?}");
    assert_eq!(backups(&root).len(), 1);
    assert_eq!(version(&root), SCHEMA_VERSION);
}

/// Catches: the migration waiting on, or failing against, a reader that holds an
/// open snapshot (a daemon serving a search) instead of completing.
#[test]
fn refresh_completes_while_another_connection_holds_a_read_snapshot() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    insert_entry(&raw(&root), "keep");
    age(&root, 20);
    let reader = raw(&root);
    reader.execute_batch("BEGIN;").expect("begin");
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
        .expect("snapshot");

    let status = refresh_store(&root, "110");

    assert!(migrated(status), "refresh failed under a reader snapshot");
    drop(reader);
    assert_eq!(version(&root), SCHEMA_VERSION);
}

// ── index_head ──────────────────────────────────────────────────────────────

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn whole_tree() -> UpdateRequest {
    UpdateRequest::default()
}

fn head_row(root: &Path) -> Option<String> {
    let ctx = Context::open(root).expect("open");
    mdkb::store::index_head::read(&ctx.conn).expect("read")
}

/// Catches: a stale commit surviving a whole-tree run in a repository that has
/// no HEAD, and a targeted run claiming the whole tree is at HEAD.
#[test]
fn index_head_follows_whole_tree_runs_only() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = mdkb::domain::canonicalize_plain(tmp.path()).expect("canon");
    git(&root, &["init", "-q"]);
    std::fs::write(root.join("a.md"), "# a\n").expect("write");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "one"]);
    let first = git(&root, &["rev-parse", "HEAD"]);
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open");

    update_documents(&ctx, &root, &whole_tree()).expect("update");
    assert_eq!(
        mdkb::store::index_head::read(&ctx.conn).expect("read"),
        Some(first.clone())
    );

    std::fs::write(root.join("b.md"), "# b\n").expect("write");
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "two"]);
    let second = git(&root, &["rev-parse", "HEAD"]);
    assert_ne!(first, second);

    let targeted = UpdateRequest {
        files: vec!["b.md".into()],
        force: false,
    };
    update_documents(&ctx, &root, &targeted).expect("targeted update");
    assert_eq!(
        mdkb::store::index_head::read(&ctx.conn).expect("read"),
        Some(first),
        "a targeted run must not claim the tree is at the new HEAD"
    );

    update_documents(&ctx, &root, &whole_tree()).expect("update");
    assert_eq!(
        mdkb::store::index_head::read(&ctx.conn).expect("read"),
        Some(second)
    );
}

#[test]
fn a_repository_without_commits_clears_a_stale_recorded_head() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = mdkb::domain::canonicalize_plain(tmp.path()).expect("canon");
    git(&root, &["init", "-q"]);
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open");
    mdkb::store::index_head::record(&ctx.conn, Some("deadbeef"), 1).expect("seed");

    update_documents(&ctx, &root, &whole_tree()).expect("update");

    assert_eq!(
        mdkb::store::index_head::read(&ctx.conn).expect("read"),
        None
    );
}

/// Catches: the v33→v34 migration breaking `index_head` readers on a store that
/// has never been indexed since: they must see "no record", not an error.
#[test]
fn a_freshly_migrated_store_reports_no_indexed_head() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = store(tmp.path(), "r");
    age(&root, 33);
    assert!(migrated(refresh_store(&root, "111")));
    assert_eq!(head_row(&root), None);
}

// ── root=* footer ───────────────────────────────────────────────────────────

fn one_slot(state: &Path) -> DaemonConfig {
    DaemonConfig {
        max_active_repos: 1,
        whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().to_string()],
        state_dir: Some(state.to_path_buf()),
        ..DaemonConfig::default()
    }
}

fn everywhere() -> SearchParams {
    serde_json::from_value(json!({
        "query": "zonk_harvest", "root": "*", "scope": "memory", "limit": 10
    }))
    .expect("params")
}

/// Catches: a store from the FUTURE being listed as "outdated, run refresh"
/// (refresh leaves it alone, so the advice cannot work); the line repeated per
/// store; all-outdated fan-outs losing the footer; the line surviving the fix.
#[tokio::test]
async fn the_footer_names_only_stores_refresh_can_fix_and_clears_after_it_ran() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let parent = store(repos.path(), "parent");
    let old_a = store(&parent, "old_a");
    let old_b = store(&parent, "old_b");
    let future = store(&parent, "from_the_future");
    age(&old_a, 20);
    age(&old_b, 31);
    raw(&future)
        .execute("UPDATE schema_version SET version = 99", [])
        .expect("future");

    let registry = Arc::new(RepoRegistry::new(one_slot(state.path())));
    registry.get_or_open(&parent).expect("open parent");

    let (output, _) = cross_repo_search_impl(&registry, &everywhere(), &[])
        .await
        .expect("search");
    let lines: Vec<&str> = output
        .lines()
        .filter(|l| l.contains("mdkb repos refresh"))
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "one outdated line, not one per store: {output}"
    );
    assert!(
        lines[0].contains("old_a") && lines[0].contains("old_b"),
        "{output}"
    );
    assert!(
        !lines[0].contains("from_the_future"),
        "a newer store cannot be fixed by refresh: {output}"
    );
    assert!(
        output.contains("from_the_future"),
        "the newer store is still reported: {output}"
    );

    let reports = refresh_outdated(&[old_a.clone(), old_b.clone(), future.clone()]);
    assert!(matches!(reports[0].outcome, Ok(RefreshStatus::Migrated(_))));
    assert!(matches!(reports[1].outcome, Ok(RefreshStatus::Migrated(_))));
    assert!(matches!(
        reports[2].outcome,
        Ok(RefreshStatus::Newer { .. })
    ));

    let (after, _) = cross_repo_search_impl(&registry, &everywhere(), &[])
        .await
        .expect("search");
    assert!(
        !after.contains("mdkb repos refresh"),
        "line outlived the fix: {after}"
    );
    assert!(after.contains("Searched 3 of 4 repos"), "{after}");
}

/// Catches: a fan-out where EVERY store is outdated returning no footer, an
/// error, or a count of zero known.
#[tokio::test]
async fn a_fan_out_over_only_outdated_stores_still_says_why_nothing_came_back() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let parent = store(repos.path(), "parent");
    let child = store(&parent, "child");
    let registry = Arc::new(RepoRegistry::new(one_slot(state.path())));
    registry.get_or_open(&parent).expect("open parent");
    age(&parent, 20);
    age(&child, 20);

    let (output, count) = cross_repo_search_impl(&registry, &everywhere(), &[])
        .await
        .expect("search must not error when every store is outdated");

    assert_eq!(count, 0);
    assert!(output.contains("Searched 0 of 2 repos"), "{output}");
    assert!(
        output.contains("parent") && output.contains("child"),
        "{output}"
    );
    assert!(
        output.contains("mdkb repos refresh --only outdated"),
        "{output}"
    );
}

// ── the command ─────────────────────────────────────────────────────────────

/// Catches: a stale entry in repos.json (deleted checkout, root with no store)
/// turning every run into a failure forever, so exit status stops meaning
/// "a store could not be migrated".
#[test]
fn a_known_root_with_no_store_is_not_a_failed_migration() {
    let tmp = tempfile::tempdir().expect("tmp");
    let old = store(tmp.path(), "old");
    insert_entry(&raw(&old), "keep");
    age(&old, 20);
    let gone = tmp.path().join("deleted_checkout");
    let home = cli::isolated_home().join(".mdkb");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(
        home.join("repos.json"),
        json!({"version": 1, "repos": [{"root": old}, {"root": gone}]}).to_string(),
    )
    .expect("repos.json");

    let out = cli::command()
        .args(["repos", "refresh", "--only", "outdated"])
        .current_dir(tmp.path())
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    assert_eq!(
        version(&old),
        SCHEMA_VERSION,
        "the real store migrates: {text}"
    );
    assert!(
        out.status.success(),
        "a root without a store is not a failed migration:\n{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let again = cli::command()
        .args(["repos", "refresh", "--only", "outdated"])
        .current_dir(tmp.path())
        .output()
        .expect("run again");
    let again = String::from_utf8_lossy(&again.stdout).into_owned();
    assert!(
        again.contains("0 migrated, 1 already current"),
        "second run: {again}"
    );
    assert_eq!(backups(&old).len(), 1, "a second run takes no second copy");
}
