//! Critic round 1 for story 221-8fb6: the edges of `.tmp` skipping, the
//! `ignore` list and `root=<suffix>` resolution.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use mdkb::daemon::config::DaemonConfig;
use mdkb::daemon::registry::RepoRegistry;
use mdkb::daemon::repo_map::discover_nested_stores;
use mdkb::mcp::dispatch::resolve_root_selector;
use mdkb::mcp::tools::RootSelector;

#[path = "common/cli.rs"]
mod cli;

/// A store as discovery sees one: a directory holding `.mdkb/index.sqlite`.
/// Returns the plain canonical spelling discovery keys it under.
fn plant(at: &Path) -> PathBuf {
    std::fs::create_dir_all(at.join(".mdkb")).unwrap();
    std::fs::write(at.join(".mdkb/index.sqlite"), b"").unwrap();
    mdkb::domain::canonicalize_plain(at).unwrap()
}

fn paths(raw: &[&str]) -> Vec<PathBuf> {
    raw.iter().map(PathBuf::from).collect()
}

fn config_ignoring(state: &Path, ignore: Vec<String>) -> DaemonConfig {
    DaemonConfig {
        max_active_repos: 4,
        whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().to_string()],
        state_dir: Some(state.to_path_buf()),
        ignore,
        ..DaemonConfig::default()
    }
}

fn resolve_name(name: &str, known: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    RootSelector::parse(Some(name)).unwrap().resolve(known, &[])
}

// ── root=<suffix> ───────────────────────────────────────────────────────────

/// Catches: suffix matching done on the string (`ends_with` on text) instead of
/// on path components, so `ommander/plugins` picks `tuicommander/plugins`.
#[test]
fn a_suffix_that_cuts_a_component_in_half_names_no_repo() {
    let known = paths(&["/x/tuicommander/plugins"]);

    assert!(resolve_name("ommander/plugins", &known).is_err());
    assert_eq!(
        resolve_name("tuicommander/plugins", &known).unwrap(),
        known,
        "the whole component still resolves"
    );
}

/// Catches: a suffix that fits two roots being resolved to whichever sorts
/// first, silently.
#[test]
fn a_suffix_shared_by_two_roots_is_refused_with_both_named() {
    let known = paths(&["/a/work/plugins", "/b/work/plugins"]);

    let err = resolve_name("work/plugins", &known).unwrap_err();

    assert!(
        err.contains("/a/work/plugins") && err.contains("/b/work/plugins"),
        "{err}"
    );
}

/// Catches: a trailing slash turning a valid suffix into "no such repo" (or, the
/// other way, into a match on the wrong root).
#[test]
fn a_suffix_with_a_trailing_slash_resolves_like_the_bare_form() {
    let known = paths(&["/a/work/plugins", "/b/other/plugins"]);

    assert_eq!(
        resolve_name("work/plugins/", &known).unwrap(),
        paths(&["/a/work/plugins"])
    );
}

/// Catches: `..` or `.` components in a name being matched literally or
/// ignored, so `../plugins` resolves to a repo the caller did not name.
#[test]
fn a_suffix_with_dot_components_names_no_repo() {
    let known = paths(&["/a/work/plugins"]);

    assert!(resolve_name("../plugins", &known).is_err());
    assert!(resolve_name("./plugins", &known).is_err());
    assert!(resolve_name("work/../plugins", &known).is_err());
}

/// Catches: the suffix branch matching a name that is longer than the root's
/// own path (`/plugins` has fewer components than `a/b/plugins`).
#[test]
fn a_suffix_longer_than_the_root_path_names_no_repo() {
    let known = paths(&["/plugins"]);

    assert!(resolve_name("work/plugins", &known).is_err());
}

// ── .tmp ────────────────────────────────────────────────────────────────────

/// Catches: the skip matching on a substring or suffix of the name, so a real
/// repo called `my.tmp`, `.tmp-keep`, `tmp` or `x.tmp` vanishes from discovery.
#[test]
fn a_directory_that_only_contains_tmp_in_its_name_is_still_walked() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let kept: BTreeSet<PathBuf> = ["my.tmp", ".tmp-keep", "tmp", "x.tmp", ".tmpl"]
        .iter()
        .map(|name| plant(&parent.join(name)))
        .collect();

    assert_eq!(discover_nested_stores(&[parent], &[]), kept);
}

/// Catches: the skip testing every component of the path instead of the entry
/// name, so a known root that LIVES under a `.tmp` directory (a worktree kept in
/// `~/Gits/.tmp/`) loses all the stores nested beneath it.
#[test]
fn a_known_root_that_lives_under_tmp_still_finds_its_nested_stores() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join(".tmp/workspace");
    let nested = plant(&workspace.join("nested"));

    let found = discover_nested_stores(&[workspace], &[]);

    assert_eq!(found, BTreeSet::from([nested]));
}

/// Catches: a `.tmp` FILE (not a directory) or a `.tmp` that is the store itself
/// hiding its siblings: only the directory subtree is skipped.
#[test]
fn a_tmp_file_next_to_a_store_does_not_hide_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let kept = plant(&parent.join("kept"));
    std::fs::write(parent.join(".tmp"), b"not a directory").unwrap();

    assert_eq!(
        discover_nested_stores(&[parent], &[]),
        BTreeSet::from([kept])
    );
}

// ── ignore list ─────────────────────────────────────────────────────────────

/// Catches: the ignore test being a string prefix, so ignoring `…/silenced`
/// also hides `…/silenced-too`.
#[test]
fn an_ignored_path_does_not_hide_a_sibling_that_shares_its_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let silenced = plant(&parent.join("silenced"));
    let sibling = plant(&parent.join("silenced-too"));

    let found = discover_nested_stores(&[parent], &[silenced]);

    assert_eq!(found, BTreeSet::from([sibling]));
}

/// Catches: an operator writing `ignore = ["/path/to/dir/"]` (trailing slash)
/// or naming the directory through a symlink and the entry silencing nothing.
#[cfg(unix)]
#[test]
fn an_ignore_entry_with_a_trailing_slash_or_through_a_symlink_still_matches() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let kept = plant(&parent.join("kept"));
    let silenced = plant(&parent.join("silenced"));
    let link = dir.path().join("link-to-silenced");
    std::os::unix::fs::symlink(&silenced, &link).unwrap();

    for entry in [
        format!("{}/", silenced.display()),
        link.display().to_string(),
    ] {
        let config = DaemonConfig {
            ignore: vec![entry.clone()],
            ..DaemonConfig::default()
        };
        let found = discover_nested_stores(std::slice::from_ref(&parent), &config.ignored_paths());
        assert_eq!(found, BTreeSet::from([kept.clone()]), "ignore = {entry:?}");
    }
}

/// Catches: `~/` left unexpanded, so the entry is a relative path that matches
/// nothing and the operator's store stays listed.
#[cfg(unix)]
#[test]
fn a_tilde_ignore_entry_expands_to_an_absolute_path_under_home() {
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let config = DaemonConfig {
        ignore: vec!["~/mdkb-critic-221-no-such-dir".to_string()],
        ..DaemonConfig::default()
    };

    let paths = config.ignored_paths();

    assert_eq!(paths.len(), 1);
    assert!(paths[0].is_absolute(), "{:?}", paths[0]);
    assert!(
        paths[0].starts_with(&home),
        "{:?} not under {home:?}",
        paths[0]
    );
}

/// Catches: an ignored ANCESTOR silencing only itself: every store below an
/// ignored directory must drop out, including ones nested two levels down.
#[test]
fn an_ignored_ancestor_silences_every_store_below_it() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let kept = plant(&parent.join("kept"));
    let ignored = parent.join("vendor");
    plant(&ignored.join("a"));
    plant(&ignored.join("deep/er/b"));
    let ignored = mdkb::domain::canonicalize_plain(&ignored).unwrap();

    assert_eq!(
        discover_nested_stores(&[parent], &[ignored]),
        BTreeSet::from([kept])
    );
}

/// Catches: the ignore list applied to `*` only, so a duplicate name that the
/// operator silenced still makes `root=plugins` ambiguous (the very report
/// behind story 221).
#[test]
fn silencing_one_of_two_same_named_repos_makes_the_bare_name_resolve() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let real = plant(&repos.path().join("real/plugins"));
    let copy = plant(&repos.path().join("copy/plugins"));
    let config = config_ignoring(state.path(), vec![copy.to_string_lossy().to_string()]);
    let registry = RepoRegistry::new(config);
    // Map the two repos' parent so discovery finds both stores.
    let container = mdkb::domain::canonicalize_plain(repos.path()).unwrap();
    mdkb::cli::handlers::handle_init(&container).expect("init");
    registry
        .get_or_open(&container)
        .expect("open the container");

    let resolved = resolve_root_selector(&registry, Some("plugins"), &[]).expect("unambiguous");

    assert_eq!(resolved.roots, vec![real]);
}

/// Catches: the ignore list overriding an explicit absolute path. A path is
/// "one repo by path; need not be a known repo" — silencing is about discovery,
/// not about refusing a caller who names the store.
#[test]
fn an_ignored_store_is_still_reachable_by_its_absolute_path() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let silenced = plant(&repos.path().join("silenced"));
    let registry = RepoRegistry::new(config_ignoring(
        state.path(),
        vec![silenced.to_string_lossy().to_string()],
    ));

    let resolved =
        resolve_root_selector(&registry, Some(&silenced.to_string_lossy()), &[]).expect("path");

    assert_eq!(resolved.roots, vec![silenced]);
}

/// Catches: a `root`-less call falling back to the handles that happen to be
/// open and returning a store the operator ignored (`default_roots` is given the
/// UNFILTERED `open` list).
#[test]
fn a_root_less_call_does_not_fall_back_to_an_ignored_open_store() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let silenced = repos.path().join("silenced");
    std::fs::create_dir_all(&silenced).unwrap();
    let silenced = mdkb::domain::canonicalize_plain(&silenced).unwrap();
    mdkb::cli::handlers::handle_init(&silenced).expect("init");
    let registry = RepoRegistry::new(config_ignoring(
        state.path(),
        vec![silenced.to_string_lossy().to_string()],
    ));
    registry
        .get_or_open(&silenced)
        .expect("open the ignored store");

    let resolved = resolve_root_selector(&registry, None, &[]).expect("resolve");

    assert!(
        !resolved.roots.contains(&silenced),
        "an ignored store came back from the root-less fallback: {:?}",
        resolved.roots
    );
}

// ── mdkb daemon status ──────────────────────────────────────────────────────

/// Catches: `daemon status` aborting on an unparsable `daemon.toml`. The
/// diagnostic command is the one an operator runs when the daemon is broken; it
/// is documented to exit 0 (tests/cli_smoke.rs `smoke_daemon_status`) and the
/// socket lines after the repo listing are what they came for.
#[cfg(unix)]
#[test]
fn daemon_status_still_reports_when_daemon_toml_does_not_parse() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".mdkb")).unwrap();
    std::fs::write(
        home.path().join(".mdkb/daemon.toml"),
        "ignore = [unterminated",
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let out = cli::command()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args(["daemon", "status"])
        .current_dir(work.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit {:?}; stderr: {stderr}",
        out.status.code()
    );
    assert!(
        stdout.contains("hook sock:"),
        "the socket lines were lost: {stdout}"
    );
}

/// Catches: the CLI listing (`daemon status`) not honouring `ignore` or the
/// `.tmp` skip while the MCP path does: two answers to "which repos exist".
#[cfg(unix)]
#[test]
fn daemon_status_lists_neither_ignored_stores_nor_tmp_copies() {
    let home = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let parent = plant(&repos.path().join("parent"));
    let kept = plant(&parent.join("kept"));
    let silenced = plant(&parent.join("silenced"));
    let copy = plant(&parent.join(".tmp/cov-audit/kept"));
    std::fs::create_dir_all(home.path().join(".mdkb")).unwrap();
    std::fs::write(
        home.path().join(".mdkb/daemon.toml"),
        format!("ignore = [{:?}]\n", silenced.to_string_lossy()),
    )
    .unwrap();
    std::fs::write(
        home.path().join(".mdkb/repos.json"),
        format!(
            "{{\"version\":1,\"repos\":[{{\"root\":{:?}}}]}}",
            parent.to_string_lossy()
        ),
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let out = cli::command()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args(["daemon", "status"])
        .current_dir(work.path())
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&kept.display().to_string()), "{stdout}");
    assert!(
        !stdout.contains(&silenced.display().to_string()),
        "{stdout}"
    );
    assert!(!stdout.contains(&copy.display().to_string()), "{stdout}");
}

// ── critic round 2 ──────────────────────────────────────────────────────────

/// Catches: an empty `ignore` entry (`ignore = [""]`, a stray comma-space or a
/// templated value that rendered empty) surviving as the empty path, which is a
/// prefix of every path, so one blank entry silences every store on the machine.
#[test]
fn an_empty_ignore_entry_silences_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent");
    let kept = plant(&parent.join("kept"));
    let config = DaemonConfig {
        ignore: vec![String::new()],
        ..DaemonConfig::default()
    };

    let found = discover_nested_stores(&[parent], &config.ignored_paths());

    assert_eq!(found, BTreeSet::from([kept]));
}

/// Catches: a relative `ignore` entry kept relative. It is then resolved
/// against whatever directory the process happens to run in: `daemon status`
/// (run from a shell) and the daemon (run from `/`) disagree, and a name that
/// does not exist yet silently matches nothing. Every path discovery compares
/// against must be absolute.
#[test]
fn every_ignored_path_is_absolute_whatever_was_written() {
    let config = DaemonConfig {
        ignore: ["cov-audit", ".tmp", "./x/y", "~someone", ""]
            .map(String::from)
            .to_vec(),
        ..DaemonConfig::default()
    };

    for path in config.ignored_paths() {
        assert!(path.is_absolute(), "relative ignored path {path:?}");
    }
}

/// Catches: a root-less call with two open stores dropping the wrong one, or
/// returning the ignored one now that the fallback list is filtered (round 1
/// only had a single open store, which an over-eager filter that empties the
/// list would also satisfy).
#[test]
fn a_root_less_call_keeps_the_open_store_that_is_not_ignored() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let mut made = Vec::new();
    for name in ["kept", "silenced"] {
        let root = repos.path().join(name);
        std::fs::create_dir_all(&root).unwrap();
        let root = mdkb::domain::canonicalize_plain(&root).unwrap();
        mdkb::cli::handlers::handle_init(&root).expect("init");
        made.push(root);
    }
    let (kept, silenced) = (made[0].clone(), made[1].clone());
    let registry = RepoRegistry::new(config_ignoring(
        state.path(),
        vec![silenced.to_string_lossy().to_string()],
    ));
    registry.get_or_open(&kept).expect("open kept");
    registry.get_or_open(&silenced).expect("open silenced");

    let resolved = resolve_root_selector(&registry, None, &[]).expect("resolve");

    assert_eq!(resolved.roots, vec![kept]);
}

/// Catches: `daemon status` swallowing the parse error (`Err(_) => Vec::new()`):
/// the listing then silently includes the stores the operator believes are
/// ignored, with nothing saying the config was not applied.
#[cfg(unix)]
#[test]
fn daemon_status_says_when_daemon_toml_was_not_applied() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".mdkb")).unwrap();
    std::fs::write(
        home.path().join(".mdkb/daemon.toml"),
        "ignore = [unterminated",
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let out = cli::command()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args(["daemon", "status"])
        .current_dir(work.path())
        .output()
        .unwrap();

    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        all.contains("daemon.toml") && all.to_lowercase().contains("warning"),
        "no warning about the unparsed config: {all}"
    );
}

/// Catches: `mdkb repos refresh --outdated` treating an unparsable
/// `daemon.toml` as "no ignore list" and migrating stores the operator
/// silenced. A mutating command refuses; only the read-only `status` degrades.
#[cfg(unix)]
#[test]
fn repos_refresh_refuses_when_daemon_toml_does_not_parse() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".mdkb")).unwrap();
    std::fs::write(
        home.path().join(".mdkb/daemon.toml"),
        "ignore = [unterminated",
    )
    .unwrap();

    let work = tempfile::tempdir().unwrap();
    let out = cli::command()
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .args(["repos", "refresh", "--only", "outdated"])
        .current_dir(work.path())
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "refreshed with an ignore list it could not read: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
