//! Critic round 8 for story 222-596a: the read-only reader answers `Err` for a
//! file it cannot parse. Callers that used to get an empty list must fail
//! cleanly and leave the file alone; the daemon's own load must still recover.

#![cfg(unix)]

use std::path::{Path, PathBuf};

use mdkb::daemon::repo_map::{RepoMap, read_known_roots, try_read_known_roots};
use serde_json::json;
use tempfile::TempDir;

#[path = "common/cli.rs"]
mod cli;

fn plain(path: &Path) -> PathBuf {
    mdkb::domain::canonicalize_plain(path).expect("canonicalize")
}

fn live(parent: &Path, name: &str) -> PathBuf {
    let path = parent.join(name);
    std::fs::create_dir_all(path.join(".mdkb")).unwrap();
    plain(&path)
}

fn write_roots(map: &Path, roots: &[String]) {
    let repos: Vec<_> = roots.iter().map(|r| json!({ "root": r })).collect();
    std::fs::write(
        map,
        serde_json::to_string(&json!({"version": 1, "repos": repos})).unwrap(),
    )
    .unwrap();
}

fn sidecars(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("repos.json") && n != "repos.json")
        .collect();
    v.sort();
    v
}

/// Catches: the read-only reader quarantining (renaming) an unparsable file, or
/// answering `Ok(empty)` so the caller cannot tell "no repos" from "unreadable".
#[test]
fn try_read_known_roots_errs_on_garbage_and_leaves_no_sidecar() {
    let tmp = TempDir::new().unwrap();
    let map = tmp.path().join("repos.json");
    std::fs::write(&map, "{ not json").unwrap();

    assert!(try_read_known_roots(&map).is_err());
    assert_eq!(std::fs::read_to_string(&map).unwrap(), "{ not json");
    assert_eq!(sidecars(tmp.path()), Vec::<String>::new());
}

/// Catches: an absent repos.json treated as an error, so `mdkb repos list` on a
/// fresh machine fails instead of listing nothing.
#[test]
fn try_read_known_roots_of_an_absent_file_is_ok_and_empty() {
    let tmp = TempDir::new().unwrap();
    let got = try_read_known_roots(&tmp.path().join("repos.json"));
    assert_eq!(got, Ok(Vec::new()));
}

/// Catches: a zero-byte file (truncate in flight) read as "no repos" rather
/// than as unreadable, which would hide every scope in it.
#[test]
fn try_read_known_roots_errs_on_an_empty_file() {
    let tmp = TempDir::new().unwrap();
    let map = tmp.path().join("repos.json");
    std::fs::write(&map, "").unwrap();
    assert!(try_read_known_roots(&map).is_err());
}

/// Catches: a directory (or other unreadable thing) at the map path panicking
/// or being reported as an absent file.
#[test]
fn try_read_known_roots_errs_when_the_path_is_a_directory() {
    let tmp = TempDir::new().unwrap();
    let map = tmp.path().join("repos.json");
    std::fs::create_dir(&map).unwrap();
    assert!(try_read_known_roots(&map).is_err());
    assert!(read_known_roots(&map).is_empty());
}

/// Catches: bytes that are not UTF-8 collapsing into an empty list instead of
/// an error.
#[test]
fn try_read_known_roots_errs_on_non_utf8_bytes() {
    let tmp = TempDir::new().unwrap();
    let map = tmp.path().join("repos.json");
    std::fs::write(&map, [0xff, 0xfe, 0x00, 0x7b]).unwrap();
    assert!(try_read_known_roots(&map).is_err());
    assert_eq!(std::fs::read(&map).unwrap(), vec![0xff, 0xfe, 0x00, 0x7b]);
}

/// Catches: the reader refusing a map written by a newer mdkb, which the old
/// reader listed (a newer binary's file is read, never replaced).
#[test]
fn try_read_known_roots_reads_a_newer_version_file() {
    let tmp = TempDir::new().unwrap();
    let a = live(tmp.path(), "a");
    let map = tmp.path().join("repos.json");
    std::fs::write(
        &map,
        serde_json::to_string(&json!({"version": 99, "repos": [{"root": a.to_string_lossy()}]}))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(try_read_known_roots(&map), Ok(vec![a]));
}

/// Catches: the daemon's load losing its recovery when the parse was moved into
/// a shared reader: a corrupt map must still open (no panic), be moved aside
/// with its bytes intact, and the map must then be writable again.
#[test]
fn the_daemon_map_still_quarantines_a_corrupt_file_and_recovers() {
    let tmp = TempDir::new().unwrap();
    let a = live(tmp.path(), "a");
    let map = tmp.path().join("repos.json");
    std::fs::write(&map, "{ not json").unwrap();

    let opened = RepoMap::open(Some(map.clone()), &[]);
    assert!(opened.roots().is_empty());
    let side = sidecars(tmp.path());
    assert_eq!(side.len(), 1, "{side:?}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(&side[0])).unwrap(),
        "{ not json"
    );

    opened.record(&a);
    assert_eq!(try_read_known_roots(&map), Ok(vec![a]));
}

/// Catches: the daemon map treating a directory at its path like garbage and
/// quarantining/replacing it, or panicking at start.
#[test]
fn the_daemon_map_opens_when_the_path_is_a_directory() {
    let tmp = TempDir::new().unwrap();
    let map = tmp.path().join("repos.json");
    std::fs::create_dir(&map).unwrap();
    let opened = RepoMap::open(Some(map.clone()), &[]);
    assert!(opened.roots().is_empty());
    assert!(map.is_dir());
}

/// Catches: canonicalizing a missing path (or an empty / relative-gone one)
/// panicking or leaking it into the list; a gone root under a symlinked parent
/// must be dropped, and weird spellings of a live root collapse to one.
/// Catches: an empty root reading the cwd worktree's `.git` and admitting its
/// main repo, even though that repo was never named in the map.
#[test]
fn missing_and_odd_spellings_do_not_panic_and_do_not_duplicate() {
    const FIXTURE_MAP: &str = "MDKB_CRITIC8_FIXTURE_MAP";
    if let Some(map) = std::env::var_os(FIXTURE_MAP) {
        let map = PathBuf::from(map);
        let real = map.parent().unwrap().join("real");
        let before = std::fs::read(&map).unwrap();
        assert_eq!(try_read_known_roots(&map), Ok(vec![real]));
        assert_eq!(std::fs::read(&map).unwrap(), before);
        return;
    }

    let tmp = TempDir::new().unwrap();
    let base = plain(tmp.path());
    let real = live(&base, "real");
    let link_parent = base.join("lp");
    std::os::unix::fs::symlink(&base, &link_parent).unwrap();
    let map = base.join("repos.json");
    write_roots(
        &map,
        &[
            String::new(),
            "no/such/relative/dir".into(),
            link_parent.join("gone").to_string_lossy().into_owned(),
            real.to_string_lossy().into_owned(),
            format!("{}/.", real.display()),
            format!("{}/../real", real.display()),
            link_parent.join("real").to_string_lossy().into_owned(),
        ],
    );
    let before = std::fs::read(&map).unwrap();

    // Record a real Git worktree pointer, rather than relying on whichever
    // repository happens to contain the test runner's cwd.
    let main = live(&base, "main");
    let worktree = base.join("worktree");
    for args in [
        vec!["init", "-q"],
        vec!["commit", "-q", "--allow-empty", "-m", "fixture"],
        vec![
            "worktree",
            "add",
            "-q",
            "--detach",
            worktree.to_str().unwrap(),
        ],
    ] {
        let output = std::process::Command::new("git")
            .current_dir(&main)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }
    assert!(worktree.join(".git").is_file());
    for cwd in [&base, &main, &worktree] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .current_dir(cwd)
            .env(FIXTURE_MAP, &map)
            .args([
                "--exact",
                "missing_and_odd_spellings_do_not_panic_and_do_not_duplicate",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "cwd={cwd:?}: {output:?}");
    }
    assert_eq!(std::fs::read(&map).unwrap(), before);
}

/// Catches: `mdkb repos list` on a corrupt repos.json exiting 0 with an empty
/// list (hiding every scope), moving the file, or failing without naming it.
#[test]
fn repos_list_fails_naming_the_file_and_keeps_it() {
    let tmp = TempDir::new().unwrap();
    let home = plain(tmp.path());
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let map = home.join(".mdkb/repos.json");
    std::fs::write(&map, "{ not json").unwrap();

    let out = cli::command()
        .env("HOME", &home)
        .env_remove("MDKB_NAMESPACE")
        .args(["repos", "list"])
        .output()
        .unwrap();

    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("repos.json"), "{err}");
    assert_eq!(std::fs::read_to_string(&map).unwrap(), "{ not json");
    assert_eq!(sidecars(&home.join(".mdkb")), Vec::<String>::new());
}

/// Catches: `mdkb repos refresh --only outdated` treating an unreadable map as
/// an empty one and reporting success having refreshed nothing.
#[test]
fn repos_refresh_fails_on_a_corrupt_repos_json() {
    let tmp = TempDir::new().unwrap();
    let home = plain(tmp.path());
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let map = home.join(".mdkb/repos.json");
    std::fs::write(&map, "{ not json").unwrap();

    let out = cli::command()
        .env("HOME", &home)
        .env_remove("MDKB_NAMESPACE")
        .args(["repos", "refresh", "--only", "outdated"])
        .output()
        .unwrap();

    assert!(!out.status.success());
    assert_eq!(std::fs::read_to_string(&map).unwrap(), "{ not json");
}

/// Catches: `mdkb repos list` failing when there is no repos.json at all.
#[test]
fn repos_list_succeeds_with_no_repos_json() {
    let tmp = TempDir::new().unwrap();
    let home = plain(tmp.path());
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();

    let out = cli::command()
        .env("HOME", &home)
        .env_remove("MDKB_NAMESPACE")
        .args(["repos", "list"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
