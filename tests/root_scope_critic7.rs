//! Critic round 7 for story 222-596a: `read_known_roots` now triages. The
//! readers of `repos.json` that stay raw (CLI reporting, scope lookup) must
//! agree with the daemon's map and must not change the file they read.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use mdkb::daemon::repo_map::{RepoMap, read_known_roots, read_scope_overrides};
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

fn live(parent: &Path, name: &str) -> PathBuf {
    let p = dir(parent, name);
    std::fs::create_dir_all(p.join(".mdkb")).unwrap();
    p
}

fn gone(parent: &Path, name: &str) -> PathBuf {
    let p = dir(parent, name);
    std::fs::remove_dir_all(&p).unwrap();
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

/// Catches: a reporting reader that quarantines (renames away) a repos.json
/// that fails to parse because its owner is mid-edit. `read_known_roots` is
/// documented as read-only; moving the file loses the scopes in it and the
/// edit the operator's editor is about to save over.
#[test]
fn reading_known_roots_does_not_move_a_mid_edit_repos_json() {
    let tmp = TempDir::new().unwrap();
    let a = live(tmp.path(), "a");
    let map = tmp.path().join("repos.json");
    let half = format!(
        "{{\"version\":1,\"repos\":[{{\"root\":\"{}\",\"scope\":\"work\"",
        a.display()
    );
    std::fs::write(&map, &half).unwrap();

    let _ = read_known_roots(&map);

    assert_eq!(
        std::fs::read_to_string(&map).ok().as_deref(),
        Some(half.as_str()),
        "the reader moved or rewrote the file"
    );
}

/// Catches: `mdkb repos list` (a reporting command) renaming a repos.json that
/// is mid-edit, so the scopes in it silently vanish for the next command.
#[test]
fn repos_list_leaves_a_mid_edit_repos_json_in_place() {
    let tmp = TempDir::new().unwrap();
    let home = dir(tmp.path(), "h");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let map = home.join(".mdkb/repos.json");
    let half = "{\"version\":1,\"repos\":[{\"root\":\"/x\",\"scope\":\"home\"";
    std::fs::write(&map, half).unwrap();

    let _ = cli::command()
        .env("HOME", &home)
        .env_remove("MDKB_NAMESPACE")
        .args(["repos", "list"])
        .output()
        .expect("run mdkb");

    assert_eq!(std::fs::read_to_string(&map).ok().as_deref(), Some(half));
}

/// Catches: a read that drops a gone scoped root for good: once the directory
/// is back with its store, the reader lists it again and its scope still applies.
#[test]
fn a_scoped_gone_root_that_returns_is_listed_again_with_its_scope() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "g");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&g, Some("home"))]);
    assert!(!read_known_roots(&map).contains(&g));

    std::fs::create_dir_all(g.join(".mdkb")).unwrap();

    assert!(read_known_roots(&map).contains(&g));
    assert_eq!(
        read_scope_overrides(&map).get(&g).map(String::as_str),
        Some("home")
    );
}

/// Catches: the read path writing back (a prune on read): a map with gone
/// scoped and gone plain entries must be byte-identical afterwards.
#[test]
fn reading_known_roots_leaves_gone_entries_in_the_file() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g1 = gone(&base, "g1");
    let g2 = gone(&base, "g2");
    let a = live(&base, "a");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&g1, Some("home")), (&g2, None), (&a, None)]);
    let before = std::fs::read(&map).unwrap();

    let known = read_known_roots(&map);

    assert_eq!(known, vec![a]);
    assert_eq!(std::fs::read(&map).unwrap(), before);
}

/// Catches: the CLI reader and the daemon map disagreeing about one file: the
/// roots a daemon would answer from are the roots `read_known_roots` lists.
#[test]
fn the_daemon_map_and_the_persisted_read_list_the_same_roots() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let g = gone(&base, "g");
    let plain_dir = dir(&base, "nostore");
    let a = live(&base, "a");
    let c = live(&base, "c");
    let map = tmp.path().join("repos.json");
    write_map(
        &map,
        &[
            (&g, Some("home")),
            (&plain_dir, Some("work")),
            (&a, None),
            (&c, Some("home")),
        ],
    );

    let from_file = read_known_roots(&map);
    let from_daemon = RepoMap::open(Some(map.clone()), &[]).roots();

    assert_eq!(from_file, from_daemon);
}

/// Catches: a symlinked spelling of a repo listed beside its real path, so
/// `root="*"` reads (and reports) one store twice.
#[test]
fn a_symlink_spelling_and_the_real_path_are_one_known_root() {
    let tmp = TempDir::new().unwrap();
    let base = dir(tmp.path(), "b");
    let real = live(&base, "real");
    let link = base.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&real, None), (&link, None)]);

    let known = read_known_roots(&map);

    assert_eq!(known, vec![real], "{known:?}");
}
