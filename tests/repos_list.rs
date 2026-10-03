//! `mdkb repos` (story 220-711b): one compact row per known repo, read live
//! from the stores.
//!
//! Every store is a real current-schema store in a temp dir, with the recorded
//! version rolled back where an old one is needed (the convention of
//! `repos_refresh.rs`). The CLI runs with `HOME` pointed at another temp dir,
//! so nothing here reads or writes the real `~/.mdkb`.

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use mdkb::core::Context;
use mdkb::daemon::repo_listing::{Health, RepoRow, list_repos};
use mdkb::store::schema::SCHEMA_VERSION;
use rusqlite::Connection;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_mdkb");
const FAR_FUTURE: i64 = 4_102_444_800;

fn index(root: &Path) -> Connection {
    Connection::open(root.join(".mdkb/index.sqlite")).expect("open store")
}

/// A current store at `parent/name`, with `docs` documents indexed at `indexed_at`.
fn store(parent: &Path, name: &str, docs: usize, indexed_at: i64) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).unwrap();
    let root = mdkb::domain::canonicalize_plain(&root).unwrap();
    mdkb::cli::handlers::handle_init(&root).expect("init");
    drop(Context::open(&root).expect("create store"));
    let conn = index(&root);
    conn.execute_batch(
        "INSERT INTO collections (name, path, created_at, updated_at) VALUES ('docs', 'docs', 1, 1);
         INSERT INTO content (hash, body, created_at) VALUES ('h1', 'body', 1);",
    )
    .unwrap();
    for i in 0..docs {
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES ('docs', ?1, 'h1', 'T', 1, ?2)",
            rusqlite::params![format!("{i}.md"), indexed_at],
        )
        .unwrap();
    }
    root
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .expect("run git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn commit_all(root: &Path) {
    git(root, &["init", "-q"]);
    std::fs::write(root.join("f.txt"), "x").unwrap();
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-q", "-m", "c"]);
}

fn head(root: &Path) -> String {
    mdkb::git::head_commit(root).expect("head")
}

fn row_of(root: &Path) -> RepoRow {
    list_repos(&[root.to_path_buf()]).remove(0)
}

/// Catches: one store this binary cannot read (outdated, empty, garbage)
/// failing or hiding the rest of the listing, and an outdated store reported
/// as healthy.
#[test]
fn one_unreadable_store_does_not_fail_the_listing() {
    let tmp = TempDir::new().unwrap();
    let healthy = store(tmp.path(), "healthy", 2, 1);
    let empty = store(tmp.path(), "empty", 0, 1);
    let old = store(tmp.path(), "old", 1, 1);
    index(&old)
        .execute("UPDATE schema_version SET version = 22", [])
        .unwrap();
    let garbage = tmp.path().join("garbage");
    std::fs::create_dir_all(garbage.join(".mdkb")).unwrap();
    std::fs::write(garbage.join(".mdkb/index.sqlite"), b"not sqlite at all").unwrap();
    let gone = tmp.path().join("gone");

    let rows = list_repos(&[healthy, empty, old, garbage, gone]);

    let health: Vec<Health> = rows.iter().map(|r| r.health).collect();
    assert_eq!(
        health,
        [
            Health::Healthy,
            Health::Empty,
            Health::SchemaOutdated,
            Health::Unreadable,
            Health::Gone
        ]
    );
    assert_eq!(rows[0].docs, Some(2));
    assert_eq!(rows[0].schema, Some(SCHEMA_VERSION));
    assert_eq!(rows[2].schema, Some(22));
    assert!(rows[3].detail.is_some(), "an unreadable row says why");
}

/// Catches: a worktree path (or a `.git`-less dir) being named after the
/// worktree directory instead of the main checkout the story names it from.
#[test]
fn a_worktree_is_named_after_its_main_checkout() {
    let tmp = TempDir::new().unwrap();
    let main = store(tmp.path(), "mainrepo", 1, 1);
    commit_all(&main);
    let wt = tmp.path().join("feature-wt");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    std::fs::create_dir_all(wt.join(".mdkb")).unwrap();

    assert_eq!(row_of(&wt).name, "mainrepo");
    assert_eq!(row_of(&main).kind, "git");
    let plain = store(tmp.path(), "plain", 1, 1);
    let r = row_of(&plain);
    assert_eq!((r.name.as_str(), r.kind, r.head_at), ("plain", "dir", None));
}

/// Catches: a stale index reported fresh (HEAD newer than the index, no
/// recorded commit), and a fresh one flagged.
#[test]
fn head_newer_than_the_index_is_stale_without_a_recorded_commit() {
    let tmp = TempDir::new().unwrap();
    let stale = store(tmp.path(), "stale", 1, 1);
    commit_all(&stale);
    let fresh = store(tmp.path(), "fresh", 1, FAR_FUTURE);
    commit_all(&fresh);

    assert!(row_of(&stale).stale);
    assert!(!row_of(&fresh).stale);
    assert_eq!(row_of(&stale).head_at, Some(1_767_225_600));
}

/// Catches: the commit-date proxy overriding the recorded commit. A branch
/// checkout at an older commit looks fresh by date; the recorded commit says
/// the index describes another tree. The reverse (recorded = HEAD, index time
/// older than HEAD) must not be flagged.
#[test]
fn the_recorded_commit_decides_over_the_date() {
    let tmp = TempDir::new().unwrap();
    let other = store(tmp.path(), "other", 1, FAR_FUTURE);
    commit_all(&other);
    index(&other)
        .execute(
            "INSERT INTO index_head (id, head, recorded_at) VALUES (1, 'deadbeef', 1)",
            [],
        )
        .unwrap();
    let same = store(tmp.path(), "same", 1, 1);
    commit_all(&same);
    index(&same)
        .execute(
            "INSERT INTO index_head (id, head, recorded_at) VALUES (1, ?1, 1)",
            [head(&same)],
        )
        .unwrap();

    assert!(row_of(&other).stale);
    assert!(!row_of(&same).stale);
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

/// Catches: the listing opening a store as a writer or rewriting `repos.json`
/// (migrating an old store, touching the map), and the JSON surface losing a
/// column.
#[test]
fn cli_json_lists_every_field_and_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let current = store(tmp.path(), "current", 1, 1);
    let old = store(tmp.path(), "old", 1, 1);
    index(&old)
        .execute("UPDATE schema_version SET version = 22", [])
        .unwrap();
    let map = mdkb::daemon::repo_map::RepoMap::open(Some(home.join(".mdkb/repos.json")), &[]);
    map.record(&current);
    map.record(&old);
    let watched = [
        home.join(".mdkb/repos.json"),
        current.join(".mdkb/index.sqlite"),
        old.join(".mdkb/index.sqlite"),
    ];
    let before: Vec<_> = watched.iter().map(|p| mtime(p)).collect();
    let old_bytes = std::fs::read(&watched[2]).unwrap();

    // Not `tmp`: it holds git repositories, where the CLI refuses to anchor.
    let cwd = tmp.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let out = Command::new(BIN)
        .args(["--format", "json", "repos", "list"])
        .env("HOME", &home)
        .env_remove("MDKB_NAMESPACE")
        .current_dir(&cwd)
        .output()
        .expect("run mdkb");

    assert!(out.status.success(), "{out:?}");
    let rows: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let rows = rows.as_array().expect("array");
    assert_eq!(rows.len(), 2);
    for key in [
        "name",
        "path",
        "kind",
        "docs",
        "memory",
        "symbols",
        "docs_indexed_at",
        "code_indexed_at",
        "head_at",
        "schema",
        "health",
        "stale",
    ] {
        assert!(rows[0].get(key).is_some(), "missing {key}: {}", rows[0]);
    }
    let health: Vec<_> = rows.iter().map(|r| r["health"].as_str().unwrap()).collect();
    assert!(
        health.contains(&"Healthy") && health.contains(&"SchemaOutdated"),
        "{health:?}"
    );
    let after: Vec<_> = watched.iter().map(|p| mtime(p)).collect();
    assert_eq!(before, after, "listing must not write");
    assert_eq!(
        old_bytes,
        std::fs::read(&watched[2]).unwrap(),
        "no migration"
    );
}

/// Catches (#210-b83b): the parallel listing returning rows in the order they
/// finish rather than the order of `roots`, or attributing a row to the wrong
/// root. Rows differ in cost: a gone root answers at once, a store with more
/// documents takes longer.
#[test]
fn parallel_listing_keeps_the_order_of_the_roots() {
    let dir = TempDir::new().unwrap();
    let mut roots = Vec::new();
    for i in 0..24usize {
        if i % 3 == 0 {
            roots.push(dir.path().join(format!("gone-{i}")));
        } else {
            roots.push(store(dir.path(), &format!("repo-{i}"), 60 - i, 1));
        }
    }

    let rows = list_repos(&roots);

    assert_eq!(rows.len(), roots.len());
    for (i, (row, root)) in rows.iter().zip(&roots).enumerate() {
        assert_eq!(&row.path, root, "row {i} belongs to another root");
        if i % 3 == 0 {
            assert_eq!(row.health, Health::Gone, "row {i}");
        } else {
            assert_eq!(row.docs, Some((60 - i) as i64), "row {i} carries another store");
        }
    }
}
