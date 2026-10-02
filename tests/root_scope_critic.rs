//! Critic cases for story 222-596a: `root="*"` must keep a HOME store out of a
//! WORK answer on every path, and the hand-written scopes must survive.
//!
//! Every store lives in a temp dir, the registry keeps its state in another,
//! and the CLI runs with a throwaway `HOME`: nothing reads the real `~/.mdkb`.
//! Memory entries carry an identifier-shaped needle (no ONNX model runs here,
//! so only the lexical leg admits a hit; see `cross_repo_search.rs`).

#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use mdkb::core::Context;
use mdkb::daemon::config::{DaemonConfig, ScopeRule};
use mdkb::daemon::registry::RepoRegistry;
use mdkb::daemon::repo_map::{RepoMap, read_scope_overrides};
use mdkb::daemon::scope::ScopePolicy;
use mdkb::mcp::dispatch::cross_repo_search_impl;
use mdkb::mcp::tools::SearchParams;
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
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

/// A store at `parent/name` whose only document is a `claude_sessions` chunk
/// about `needle`.
fn store_with_session(parent: &Path, name: &str, needle: &str) -> PathBuf {
    let root = dir(parent, name);
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open store");
    let now = chrono::Utc::now().timestamp();
    mdkb::store::collections::add_collection(
        &ctx.conn,
        &mdkb::domain::Collection {
            name: mdkb::domain::COLLECTION_CLAUDE_SESSIONS.to_string(),
            path: "./no-such-dir".to_string(),
            pattern: "**/*".to_string(),
            source: mdkb::domain::COLLECTION_SOURCE_SESSIONS.to_string(),
            created_at: now,
            updated_at: now,
        },
    )
    .expect("add collection");
    let content = format!("The session discussed {needle} at length: {needle} decided.");
    mdkb::store::documents::index_document(
        &ctx.conn,
        &mdkb::domain::Document {
            id: 0,
            collection: mdkb::domain::COLLECTION_CLAUDE_SESSIONS.to_string(),
            relative_path: "sid-chunk-001".to_string(),
            hash: mdkb::store::documents::compute_hash(&content),
            title: Some(format!("Session about {needle}")),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
            status: Some("current".to_string()),
        },
        &content,
    )
    .expect("index session chunk");
    root
}

fn config(state: &Path, work: &Path, home: &Path) -> DaemonConfig {
    DaemonConfig {
        max_active_repos: 8,
        whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().to_string()],
        state_dir: Some(state.to_path_buf()),
        scopes: vec![
            ScopeRule {
                prefix: work.to_string_lossy().to_string(),
                scope: "work".to_string(),
            },
            ScopeRule {
                prefix: home.to_string_lossy().to_string(),
                scope: "home".to_string(),
            },
        ],
        ..DaemonConfig::default()
    }
}

fn search(root: &str, scope: &str, query: &str) -> SearchParams {
    serde_json::from_value(json!({
        "query": query,
        "root": root,
        "scope": scope,
        "limit": 10,
    }))
    .expect("search params")
}

/// The answer text, or the error text: callers below assert on what the human
/// reads, not on which of the two it came as.
async fn answer(
    registry: &Arc<RepoRegistry>,
    params: &SearchParams,
    caller: &[PathBuf],
) -> (String, usize) {
    match cross_repo_search_impl(registry, params, caller).await {
        Ok((text, count)) => (text, count),
        Err(e) => (format!("{e:?}"), 0),
    }
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

// ---------------------------------------------------------------------------
// ScopePolicy
// ---------------------------------------------------------------------------

/// Catches: prefix matching on the string (`starts_with` on `&str`) — a rule
/// for `/t/work` would put `/t/workshop/r` in WORK, so a HOME-ish repo whose
/// directory merely begins with the same letters is filed under the wrong scope.
#[test]
fn a_rule_does_not_claim_a_sibling_directory_sharing_its_string_prefix() {
    let policy = ScopePolicy::new(
        vec![(PathBuf::from("/t/work"), "work".to_string())],
        BTreeMap::new(),
    );
    assert_eq!(policy.scope_of(Path::new("/t/work")), Some("work"));
    assert_eq!(policy.scope_of(Path::new("/t/work/r")), Some("work"));
    assert_eq!(policy.scope_of(Path::new("/t/workshop/r")), None);
    assert_eq!(policy.scope_of(Path::new("/t/wor")), None);
}

/// Catches: a nearer rule losing to a farther override — `repos.json` says the
/// override beats the rule AT THE SAME PATH, but a nested rule below an
/// overridden ancestor is nearer and must win.
#[test]
fn the_nearest_declaration_wins_over_an_override_on_an_ancestor() {
    let mut overrides = BTreeMap::new();
    overrides.insert(PathBuf::from("/t/a"), "work".to_string());
    let policy = ScopePolicy::new(
        vec![(PathBuf::from("/t/a/private"), "home".to_string())],
        overrides,
    );
    assert_eq!(policy.scope_of(Path::new("/t/a/x")), Some("work"));
    assert_eq!(policy.scope_of(Path::new("/t/a/private/x")), Some("home"));
}

/// Catches: a hand-written `"scope": ""` becoming the scope `""`. It shadows
/// the prefix rule that would decide, and under `*` a caller in WORK drops the
/// repo as "scope differs" although the operator declared none.
#[test]
fn a_blank_scope_in_repos_json_is_not_a_scope() {
    let tmp = TempDir::new().unwrap();
    let repo = dir(tmp.path(), "r");
    let map = tmp.path().join("repos.json");
    write_map(
        &map,
        &[(&repo, Some("")), (&dir(tmp.path(), "s"), Some("   "))],
    );

    let overrides = read_scope_overrides(&map);

    assert!(
        overrides.values().all(|s| !s.trim().is_empty()),
        "blank scopes must read as none, got {overrides:?}"
    );
}

/// Catches: `daemon.toml` rules trimming the scope name while a `repos.json`
/// entry does not — `"scope": " home"` is then a third scope that equals
/// neither the rule's `home` nor the caller's, so the repo is excluded from
/// every `*` and selected by no `scope:home`.
#[test]
fn a_repos_json_scope_is_trimmed_like_a_daemon_toml_scope() {
    let tmp = TempDir::new().unwrap();
    let repo = dir(tmp.path(), "r");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&repo, Some(" home "))]);

    let overrides = read_scope_overrides(&map);

    assert_eq!(overrides.get(&repo).map(String::as_str), Some("home"));
}

// ---------------------------------------------------------------------------
// repos.json rewrite
// ---------------------------------------------------------------------------

/// Control. Catches: the rewrite for a new root erasing a hand-written scope,
/// including one written through a symlinked spelling of the repo.
#[test]
fn recording_a_root_keeps_the_scope_written_through_a_symlink() {
    let tmp = TempDir::new().unwrap();
    let a = store(tmp.path(), "a", "zonk_a");
    let b = store(tmp.path(), "b", "zonk_b");
    let link = tmp.path().join("a-link");
    std::os::unix::fs::symlink(&a, &link).unwrap();
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&link, Some("work"))]);

    let repo_map = RepoMap::open(Some(map.clone()), &[]);
    repo_map.record(&b);

    let overrides = read_scope_overrides(&map);
    assert_eq!(
        overrides.get(&a).map(String::as_str),
        Some("work"),
        "{overrides:?}"
    );
}

/// Catches: the triage pass dropping an entry (no store, or a volume that is
/// not mounted right now) and, with it, the scope the operator wrote. The
/// policy honours that scope until the first unrelated rewrite, then it is
/// gone and every repo below that directory silently falls back to its prefix
/// rule.
#[test]
fn a_scope_on_an_entry_without_a_store_survives_a_rewrite() {
    let tmp = TempDir::new().unwrap();
    let container = dir(tmp.path(), "container"); // exists, holds no .mdkb
    let a = store(tmp.path(), "a", "zonk_a");
    let map = tmp.path().join("repos.json");
    write_map(&map, &[(&container, Some("home")), (&a, None)]);

    let _repo_map = RepoMap::open(Some(map.clone()), &[]);

    let overrides = read_scope_overrides(&map);
    assert_eq!(
        overrides.get(&container).map(String::as_str),
        Some("home"),
        "{overrides:?}"
    );
}

// ---------------------------------------------------------------------------
// MCP root="*", scope:NAME, *:all
// ---------------------------------------------------------------------------

/// Catches: `scope:NAME` naming no known scope answering "No repos registered.
/// Pass root=..." — a typo (`scope:hmoe`) reads as an empty registry and the
/// caller never learns that the name matched nothing.
#[tokio::test]
async fn a_scope_nobody_has_says_so() {
    let tmp = TempDir::new().unwrap();
    let state = dir(tmp.path(), "state");
    let work = dir(tmp.path(), "work");
    let home = dir(tmp.path(), "homes");
    let w = store(&work, "w", "zonk_w");
    let registry = Arc::new(RepoRegistry::new(config(&state, &work, &home)));
    registry.get_or_open(&w).unwrap();

    let (text, _) = answer(
        &registry,
        &search("scope:nope", "memory", "zonk_w"),
        &[work],
    )
    .await;

    assert!(
        text.contains("nope"),
        "the answer must name the scope: {text}"
    );
}

/// Catches: every known repo excluded by scope surfacing as "No repos
/// registered" — the footer line `Excluded by scope: N` is only written on the
/// success path, so the one answer that most needs it never carries it.
#[tokio::test]
async fn a_wildcard_that_excludes_everything_says_why() {
    let tmp = TempDir::new().unwrap();
    let state = dir(tmp.path(), "state");
    let work = dir(tmp.path(), "work");
    let home = dir(tmp.path(), "homes");
    let h = store(&home, "h", "zonk_h");
    let registry = Arc::new(RepoRegistry::new(config(&state, &work, &home)));
    registry.get_or_open(&h).unwrap();

    let (text, count) = answer(&registry, &search("*", "memory", "zonk_h"), &[work]).await;

    assert_eq!(count, 0, "{text}");
    assert!(text.contains("Excluded by scope"), "{text}");
}

/// Catches: the policy being a snapshot taken when the registry was built. A
/// scope added to `repos.json` by hand while the daemon runs is ignored by
/// `*` (and the next rewrite preserves it), so a HOME repo stays in WORK
/// answers until a restart nobody knows is needed.
#[tokio::test]
async fn a_scope_written_to_repos_json_after_startup_applies() {
    let tmp = TempDir::new().unwrap();
    let state = dir(tmp.path(), "state");
    let work = dir(tmp.path(), "work");
    let home = dir(tmp.path(), "homes");
    let other = dir(tmp.path(), "other"); // under no rule
    let o = store(&other, "o", "zonk_o");
    let registry = Arc::new(RepoRegistry::new(config(&state, &work, &home)));
    registry.get_or_open(&o).unwrap();
    let caller = vec![work.clone()];

    let (_, before) = answer(&registry, &search("*", "memory", "zonk_o"), &caller).await;
    assert_eq!(before, 1, "control: an unscoped repo is in `*`");

    write_map(&state.join("repos.json"), &[(&o, Some("home"))]);

    let (text, after) = answer(&registry, &search("*", "memory", "zonk_o"), &caller).await;
    assert_eq!(after, 0, "the repo is now declared home: {text}");
}

/// Catches: `claude_sessions` reaching an answer through `scope:NAME` or `*`,
/// on the MCP surface. The control (`*:all`) proves the fixture finds the
/// session chunk at all; without it the negatives could pass on an empty store.
#[tokio::test]
async fn sessions_stay_out_of_star_and_scope_but_come_with_star_all() {
    let tmp = TempDir::new().unwrap();
    let state = dir(tmp.path(), "state");
    let work = dir(tmp.path(), "work");
    let home = dir(tmp.path(), "homes");
    let w = store_with_session(&work, "w", "zonk_session");
    let registry = Arc::new(RepoRegistry::new(config(&state, &work, &home)));
    registry.get_or_open(&w).unwrap();
    let caller = vec![work.clone()];

    let (text, all) = answer(&registry, &search("*:all", "docs", "zonk_session"), &caller).await;
    assert!(
        all >= 1,
        "control: `*:all` must find the session chunk: {text}"
    );

    for root in ["*", "scope:work"] {
        let (text, count) = answer(&registry, &search(root, "docs", "zonk_session"), &caller).await;
        assert_eq!(count, 0, "root={root} leaked a session chunk: {text}");
    }
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

fn git(cwd: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn star_search(home: &Path, cwd: &Path, query: &str) -> std::process::Output {
    cli::command()
        .env("HOME", home)
        .env_remove("MDKB_NAMESPACE")
        .args(["search", "--root", "*", query])
        .current_dir(cwd)
        .output()
        .expect("run mdkb")
}

/// Catches: the CLI taking the caller's scope from the raw working directory.
/// MCP resolves a linked worktree to its main worktree before it looks the
/// scope up; the CLI does not, so `mdkb search --root '*'` run from a worktree
/// that lives outside the prefix has no scope and excludes nothing — HOME
/// stores answer a WORK question.
#[test]
fn the_cli_scopes_a_linked_worktree_by_its_main_worktree() {
    let tmp = TempDir::new().unwrap();
    let work = dir(tmp.path(), "work");
    let homes = dir(tmp.path(), "homes");
    let elsewhere = dir(tmp.path(), "elsewhere");
    let w = store(&work, "w", "zonk_w");
    let _h = store(&homes, "h", "zonk_h");
    git(&w, &["init", "-q"]);
    git(&w, &["commit", "-q", "--allow-empty", "-m", "x"]);
    let wt = elsewhere.join("wt");
    git(&w, &["worktree", "add", "-q", wt.to_str().unwrap()]);

    let cli_home = dir(tmp.path(), "cli-home");
    std::fs::create_dir_all(cli_home.join(".mdkb")).unwrap();
    std::fs::write(
        cli_home.join(".mdkb/daemon.toml"),
        format!(
            "[[scopes]]\nprefix = \"{}\"\nscope = \"work\"\n\n[[scopes]]\nprefix = \"{}\"\nscope = \"home\"\n",
            work.display(),
            homes.display()
        ),
    )
    .unwrap();
    let map = RepoMap::open(Some(cli_home.join(".mdkb/repos.json")), &[]);
    map.record(&w);
    map.record(&homes.join("h"));

    let control = star_search(&cli_home, &w, "zonk_w");
    let control_err = String::from_utf8_lossy(&control.stderr).into_owned();
    assert!(
        control_err.contains("Excluded by scope: 1"),
        "control from the main worktree: {control_err}"
    );

    let from_wt = star_search(&cli_home, &wt, "zonk_w");
    let err = String::from_utf8_lossy(&from_wt.stderr).into_owned();
    assert!(
        err.contains("Excluded by scope: 1"),
        "from the linked worktree: {err}"
    );
}
