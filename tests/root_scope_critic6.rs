//! Critic round 6 for story 222-596a: a hand-written scope outlives a missing
//! store (`write_atomic` unions roots and scope keys), so the union must not
//! prune what it should, keep what it should not, duplicate, or rewrite forever.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use mdkb::daemon::repo_map::{RepoMap, read_known_roots, read_scope_overrides};
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use mdkb::core::Context;
use serde_json::json;
use tempfile::TempDir;

#[path = "common/cli.rs"]
mod cli;

fn plain(path: &Path) -> PathBuf {
    mdkb::domain::canonicalize_plain(path).expect("canonicalize")
}

fn dir(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(&path).expect("create dir");
    plain(&path)
}

/// A store at `parent/name` with one memory entry about `needle`.
fn store(parent: &Path, name: &str, needle: &str) -> PathBuf {
    let root = dir(parent, name);
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open store");
    let now = chrono::Utc::now().timestamp();
    add_entry(
        &ctx.conn,
        &MemoryEntry {
            triggers: Vec::new(),
            id: format!("{needle}-entry"),
            title: format!("The {needle} decision"),
            content: format!("Body mentioning {needle} so the lexical leg has something to match."),
            entry_type: EntryType::Decision,
            tags: vec![needle.to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        },
    )
    .expect("seed memory entry");
    root
}


/// A directory that was a repo and is deleted now.
fn gone(parent: &Path, name: &str) -> PathBuf {
    let p = dir(parent, name);
    std::fs::remove_dir_all(&p).unwrap();
    p
}

fn live(parent: &Path, name: &str) -> PathBuf {
    let p = dir(parent, name);
    std::fs::create_dir_all(p.join(".mdkb")).unwrap();
    p
}

fn write_map(path: &Path, entries: &[(&Path, Option<&str>)]) {
    let repos: Vec<_> = entries
        .iter()
        .map(|(root, scope)| match scope {
            Some(s) => json!({"root": root.to_string_lossy(), "scope": s}),
            None => json!({"root": root.to_string_lossy()}),
        })
        .collect();
    std::fs::write(
        path,
        serde_json::to_string_pretty(&json!({"version": 1, "repos": repos})).unwrap(),
    )
    .expect("write repos.json");
}

fn file_roots(path: &Path) -> Vec<(String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["root"].as_str().unwrap().to_string(),
                r["scope"].as_str().map(str::to_string),
            )
        })
        .collect()
}

/// Catches: `dropped_for_good` using `all`/ignoring the file, or `write_atomic`
/// keeping every dropped root: the unscoped gone root must still be pruned
/// while the scoped one stays.
#[test]
fn a_gone_root_without_scope_is_pruned_beside_a_gone_root_with_one() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let scoped = gone(&base, "scoped");
    let plain_gone = gone(&base, "plain");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&scoped, Some("home")), (&plain_gone, None), (&a, None)]);

    let _m = RepoMap::open(Some(map.clone()), &[]);

    let roots = file_roots(&map);
    let names: Vec<_> = roots.iter().map(|(r, _)| r.clone()).collect();
    assert!(names.contains(&scoped.to_string_lossy().to_string()), "{roots:?}");
    assert!(!names.contains(&plain_gone.to_string_lossy().to_string()), "{roots:?}");
    assert!(names.contains(&a.to_string_lossy().to_string()), "{roots:?}");
}

/// Catches: a rewrite on every open. Only a gone entry with a scope is in the
/// file; the open must not touch it (reformatted bytes stay reformatted).
#[test]
fn opening_a_map_whose_only_gap_is_a_scoped_gone_root_does_not_rewrite_it() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let scoped = gone(&base, "scoped");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    let compact = format!(
        "{{\"version\":1,\"repos\":[{{\"root\":\"{}\",\"scope\":\"home\"}},{{\"root\":\"{}\"}}]}}",
        scoped.display(),
        a.display()
    );
    std::fs::write(&map, &compact).unwrap();

    let _m = RepoMap::open(Some(map.clone()), &[]);
    let _m2 = RepoMap::open(Some(map.clone()), &[]);

    assert_eq!(std::fs::read_to_string(&map).unwrap(), compact);
}

/// Catches: the operator removing a stale scope by hand and the daemon
/// resurrecting it from a cached read — the entry has no scope any more, so
/// the next open must prune the gone root.
#[test]
fn a_gone_root_whose_scope_was_removed_by_hand_is_pruned() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "g");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&g, Some("home")), (&a, None)]);
    let _m = RepoMap::open(Some(map.clone()), &[]);
    assert_eq!(read_scope_overrides(&map).len(), 1);

    write_map(&map, &[(&g, None), (&a, None)]);
    let _m = RepoMap::open(Some(map.clone()), &[]);

    let names: Vec<_> = file_roots(&map).into_iter().map(|(r, _)| r).collect();
    assert!(!names.contains(&g.to_string_lossy().to_string()), "{names:?}");
}

/// Catches: a blank scope ("  ") counted as declared by the change detector,
/// so a gone root with a blank scope is kept forever.
#[test]
fn a_gone_root_with_a_blank_scope_is_pruned() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "g");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&g, Some("  ")), (&a, None)]);

    let _m = RepoMap::open(Some(map.clone()), &[]);

    let names: Vec<_> = file_roots(&map).into_iter().map(|(r, _)| r).collect();
    assert!(!names.contains(&g.to_string_lossy().to_string()), "{names:?}");
}

/// Catches: a duplicate entry (one with the scope, one without) when the same
/// repo is spelled through a symlink in the file and canonically in memory.
#[test]
fn a_scope_written_through_a_symlink_does_not_duplicate_the_repo() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let a = live(&base, "a");
    let link = base.join("link");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    let other = live(&base, "other");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&link, Some("home"))]);

    let m = RepoMap::open(Some(map.clone()), &[]);
    m.record(&other);

    let roots = file_roots(&map);
    let a_entries: Vec<_> = roots
        .iter()
        .filter(|(r, _)| r == &a.to_string_lossy() || r == &link.to_string_lossy())
        .collect();
    assert_eq!(a_entries.len(), 1, "{roots:?}");
    assert_eq!(a_entries[0].1.as_deref(), Some("home"), "{roots:?}");
}

/// Catches: a scope parked on a missing path being lost or split when the path
/// becomes a live repo again and is recorded: one entry, scope intact.
#[test]
fn a_scoped_gone_root_that_comes_back_keeps_its_scope_once() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "g");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&g, Some("work")), (&a, None)]);
    let m = RepoMap::open(Some(map.clone()), &[]);
    assert!(!m.contains(&g), "a gone root is not a known repo in memory");

    std::fs::create_dir_all(g.join(".mdkb")).unwrap();
    m.record(&g);

    let roots = file_roots(&map);
    let g_entries: Vec<_> = roots.iter().filter(|(r, _)| r == &g.to_string_lossy()).collect();
    assert_eq!(g_entries.len(), 1, "{roots:?}");
    assert_eq!(g_entries[0].1.as_deref(), Some("work"), "{roots:?}");
    assert!(m.contains(&g));
}

/// Catches: a gone scoped entry that is also a daemon.toml seed forcing a
/// rewrite or being re-added to memory as a known repo.
#[test]
fn a_gone_seed_with_a_scope_in_the_file_is_neither_known_nor_rewritten() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "g");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    let compact = format!(
        "{{\"version\":1,\"repos\":[{{\"root\":\"{}\",\"scope\":\"home\"}},{{\"root\":\"{}\"}}]}}",
        g.display(),
        a.display()
    );
    std::fs::write(&map, &compact).unwrap();
    let seed = mdkb::daemon::config::RepoEntry { root: g.to_string_lossy().to_string(), scope: None };

    let m = RepoMap::open(Some(map.clone()), &[seed]);

    assert!(!m.contains(&g));
    assert_eq!(std::fs::read_to_string(&map).unwrap(), compact);
}

/// Catches: a stale scoped entry leaking into what `root="*"` reads without a
/// daemon (`read_known_roots` feeds the CLI): a gone path is not a repo.
#[test]
fn the_persisted_known_roots_do_not_list_a_gone_scoped_root() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "g");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&g, Some("home")), (&a, None)]);
    let _m = RepoMap::open(Some(map.clone()), &[]);

    let known = read_known_roots(&map);

    assert!(!known.contains(&g), "{known:?}");
}

/// Catches: `search --root '*'` (no daemon, reads repos.json) failing or
/// naming a gone path because a scoped gone entry stays in the file.
#[test]
fn star_search_without_a_daemon_ignores_a_scoped_gone_entry() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "ghost_repo_dir");
    let a = store(&base, "a", "zonk_a");
    let home = dir(tmp.path(), "cli-home");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    write_map(&home.join(".mdkb/repos.json"), &[(&g, Some("home")), (&a, None)]);

    let out = cli::command()
        .env("HOME", &home)
        .env_remove("MDKB_NAMESPACE")
        .args(["search", "--root", "*", "zonk_a"])
        .current_dir(&a)
        .output()
        .expect("run mdkb");

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("zonk_a"), "{stdout}\n{stderr}");
    assert!(!stderr.contains("ghost_repo_dir"), "{stderr}");
}
