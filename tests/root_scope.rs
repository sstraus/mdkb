//! `root="*"` respects the caller's scope (story 222-596a).
//!
//! Scope comes from `[[scopes]]` prefix rules in `daemon.toml` and from a
//! `scope` on a `repos.json` entry, which wins. Every store and every config
//! lives in a temp dir: nothing here touches the real `~/.mdkb`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mdkb::core::Context;
use mdkb::daemon::config::{DaemonConfig, ScopeRule};
use mdkb::daemon::registry::RepoRegistry;
use mdkb::mcp::dispatch::{cross_repo_search_impl, resolve_root_selector};
use mdkb::mcp::tools::SearchParams;
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use serde_json::json;

/// A store at `parent/name` with one memory entry about `needle`.
fn store(parent: &Path, name: &str, needle: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("create repo root");
    let root = mdkb::domain::canonicalize_plain(&root).expect("canonicalize");
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

/// Index one `claude_sessions` document about `needle` into the store at `root`.
fn seed_session(root: &Path, needle: &str) {
    let ctx = Context::open(root).expect("open store");
    let now = chrono::Utc::now().timestamp();
    mdkb::store::collections::add_collection(
        &ctx.conn,
        &mdkb::domain::Collection {
            name: "claude_sessions".to_string(),
            path: "./no-such-dir".to_string(),
            pattern: "**/*".to_string(),
            source: mdkb::domain::COLLECTION_SOURCE_SESSIONS.to_string(),
            created_at: now,
            updated_at: now,
        },
    )
    .expect("add collection");
    let content = format!("Transcript where we discussed {needle} at length.");
    let doc = mdkb::domain::Document {
        id: 0,
        collection: "claude_sessions".to_string(),
        relative_path: "sid-chunk-001".to_string(),
        hash: mdkb::store::documents::compute_hash(&content),
        title: Some(format!("Session about {needle}")),
        metadata: None,
        file_modified_at: now,
        indexed_at: now,
        status: Some("current".to_string()),
    };
    mdkb::store::documents::index_document(&ctx.conn, &doc, &content).expect("index session");
}

/// `home/` and `work/` prefixes, one store under each, and one under `free/`
/// that no rule covers.
struct World {
    _tmp: tempfile::TempDir,
    state: PathBuf,
    home_dir: PathBuf,
    work_dir: PathBuf,
    home_store: PathBuf,
    work_store: PathBuf,
    free_store: PathBuf,
}

fn world() -> World {
    let tmp = tempfile::tempdir().expect("tmp");
    let base = mdkb::domain::canonicalize_plain(tmp.path()).expect("canonicalize");
    let state = base.join("state");
    std::fs::create_dir_all(&state).expect("state");
    let (home_dir, work_dir) = (base.join("home"), base.join("work"));
    let home_store = store(&home_dir, "h", "home_needle");
    let work_store = store(&work_dir, "w", "work_needle");
    let free_store = store(&base.join("free"), "f", "free_needle");
    World {
        _tmp: tmp,
        state,
        home_dir,
        work_dir,
        home_store,
        work_store,
        free_store,
    }
}

fn registry(world: &World) -> Arc<RepoRegistry> {
    let config = DaemonConfig {
        whitelist_dirs: vec![world.state.parent().unwrap().to_string_lossy().to_string()],
        state_dir: Some(world.state.clone()),
        scopes: vec![
            ScopeRule {
                prefix: world.home_dir.to_string_lossy().to_string(),
                scope: "home".to_string(),
            },
            ScopeRule {
                prefix: world.work_dir.to_string_lossy().to_string(),
                scope: "work".to_string(),
            },
        ],
        ..DaemonConfig::default()
    };
    let registry = Arc::new(RepoRegistry::new(config));
    for root in [&world.home_store, &world.work_store, &world.free_store] {
        registry.get_or_open(root).expect("open");
    }
    registry
}

fn memory_search(query: &str, root: &str) -> SearchParams {
    serde_json::from_value(json!({
        "query": query,
        "root": root,
        "scope": "memory",
        "limit": 10,
    }))
    .expect("search params")
}

fn sorted(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots
}

/// Catches: HOME content leaking into a WORK answer. A work caller searching
/// `*` for an entry that lives in a home store must not see it, and the footer
/// must say a store was left out rather than let the empty answer read as
/// "nothing matched".
#[tokio::test]
async fn a_home_store_is_left_out_of_star_for_a_work_caller_and_the_footer_says_so() {
    let world = world();
    let registry = registry(&world);
    let work_caller = [world.work_store.clone()];

    let (leak, _) =
        cross_repo_search_impl(&registry, &memory_search("home_needle", "*"), &work_caller)
            .await
            .expect("search");
    assert!(
        !leak.contains("home_needle"),
        "home leaked into work: {leak}"
    );
    assert!(leak.contains("Excluded by scope: 1"), "{leak}");

    let (own, count) =
        cross_repo_search_impl(&registry, &memory_search("work_needle", "*"), &work_caller)
            .await
            .expect("search");
    assert!(count >= 1 && own.contains("work_needle"), "control: {own}");
}

/// Catches: no way to opt in. `*:all` and `scope:home` must reach the home
/// store from a work caller, and `*:all` must not report an exclusion.
#[tokio::test]
async fn star_all_and_a_named_scope_cross_the_boundary() {
    let world = world();
    let registry = registry(&world);
    let work_caller = [world.work_store.clone()];

    let (mixed, _) = cross_repo_search_impl(
        &registry,
        &memory_search("home_needle", "*:all"),
        &work_caller,
    )
    .await
    .expect("search");
    assert!(mixed.contains("home_needle"), "{mixed}");
    assert!(!mixed.contains("Excluded by scope"), "{mixed}");

    let (named, _) = cross_repo_search_impl(
        &registry,
        &memory_search("home_needle", "scope:home"),
        &work_caller,
    )
    .await
    .expect("search");
    assert!(named.contains("home_needle"), "{named}");

    let resolved =
        resolve_root_selector(&registry, Some("scope:home"), &work_caller).expect("resolve");
    assert_eq!(resolved.roots, vec![world.home_store.clone()]);
}

/// Catches: unscoped repos disappearing. A repo no rule covers stays in `*`
/// for a work caller and a home caller alike, and a caller with no scope of
/// its own loses nothing.
#[test]
fn a_repo_with_no_scope_is_always_in_star() {
    let world = world();
    let registry = registry(&world);

    for caller in [
        vec![world.work_store.clone()],
        vec![world.home_store.clone()],
        vec![world.free_store.clone()],
        Vec::new(),
    ] {
        let resolved = resolve_root_selector(&registry, Some("*"), &caller).expect("resolve");
        assert!(
            resolved.roots.contains(&world.free_store),
            "unscoped repo dropped for caller {caller:?}"
        );
    }

    let unscoped =
        resolve_root_selector(&registry, Some("*"), &[world.free_store.clone()]).expect("resolve");
    assert_eq!(
        sorted(unscoped.roots),
        sorted(vec![
            world.home_store.clone(),
            world.work_store.clone(),
            world.free_store.clone()
        ]),
        "a caller with no scope must exclude nothing"
    );
    assert_eq!(unscoped.excluded_by_scope, 0);
}

/// Catches: a wrong prefix with no override. A repos.json `scope` moves a
/// store the home prefix would claim into work, so a work caller keeps it.
#[test]
fn a_repos_json_scope_overrides_the_prefix_rule() {
    let world = world();
    let moved = store(&world.home_dir, "moved", "moved_needle");
    std::fs::write(
        world.state.join("repos.json"),
        json!({"version": 1, "repos": [{"root": moved, "scope": "work"}]}).to_string(),
    )
    .expect("write repos.json");
    let registry = registry(&world);

    let resolved =
        resolve_root_selector(&registry, Some("*"), &[world.work_store.clone()]).expect("resolve");
    assert!(resolved.roots.contains(&moved), "override ignored");
    assert!(!resolved.roots.contains(&world.home_store));
    assert_eq!(resolved.excluded_by_scope, 1);
}

/// Catches: the daemon erasing a hand-written scope. Recording a new root
/// rewrites repos.json from the in-memory set, which carries no scope; the
/// override must survive that rewrite.
#[test]
fn a_repos_json_scope_survives_the_map_being_rewritten() {
    let world = world();
    let moved = store(&world.home_dir, "moved", "moved_needle");
    let map = world.state.join("repos.json");
    std::fs::write(
        &map,
        json!({"version": 1, "repos": [{"root": moved, "scope": "work"}]}).to_string(),
    )
    .expect("write repos.json");

    let registry = registry(&world); // records three new roots: three rewrites
    drop(registry);

    let text = std::fs::read_to_string(&map).expect("read repos.json");
    let file: serde_json::Value = serde_json::from_str(&text).expect("json");
    let entry = file["repos"]
        .as_array()
        .expect("repos")
        .iter()
        .find(|r| r["root"] == json!(moved))
        .unwrap_or_else(|| panic!("moved entry missing: {text}"));
    assert_eq!(entry["scope"], "work", "{text}");
}

fn docs_search(query: &str, root: &str) -> SearchParams {
    serde_json::from_value(json!({"query": query, "root": root, "scope": "docs", "limit": 10}))
        .expect("search params")
}

/// Catches: Claude transcripts leaking into `*` (they are not documentation),
/// and `*:all` having no way to reach them.
#[tokio::test]
async fn star_leaves_claude_sessions_out_and_star_all_includes_them() {
    let world = world();
    seed_session(&world.free_store, "ledger_sync");
    let registry = registry(&world);

    let (default, _) = cross_repo_search_impl(&registry, &docs_search("ledger_sync", "*"), &[])
        .await
        .expect("search");
    assert!(!default.contains("sid-chunk-001"), "{default}");

    let (all, count) = cross_repo_search_impl(&registry, &docs_search("ledger_sync", "*:all"), &[])
        .await
        .expect("search");
    assert!(count >= 1 && all.contains("sid-chunk-001"), "{all}");
}

/// Catches: a `repos.json` that is mid-edit (or unreadable) silently dropping
/// every override for that call. A repo moved into work flips back to its home
/// prefix and `*` answers a work question from home. The last good scopes stay.
#[test]
fn a_repos_json_that_cannot_be_read_keeps_the_last_good_scopes() {
    let world = world();
    let moved = store(&world.home_dir, "moved", "moved_needle");
    let map = world.state.join("repos.json");
    std::fs::write(
        &map,
        json!({"version": 1, "repos": [{"root": moved, "scope": "work"}]}).to_string(),
    )
    .expect("write repos.json");
    let registry = registry(&world);
    assert_eq!(
        registry.scope_policy().scope_of(&moved),
        Some("work"),
        "control"
    );

    std::fs::write(&map, "{ \"version\": 1, \"repos\": [ {").expect("half-written edit");

    assert_eq!(
        registry.scope_policy().scope_of(&moved),
        Some("work"),
        "a half-written repos.json dropped the override"
    );
}

/// Catches: the CLI matching prefix rules against the main worktree as the
/// linked worktree's `.git` file spells it. When that path goes through a
/// symlink it is not the canonical spelling the rules use, so the caller has no
/// scope and `*` excludes nothing.
#[cfg(unix)]
#[test]
fn the_cli_caller_root_is_canonical_after_following_a_worktree_to_its_main() {
    let tmp = tempfile::tempdir().expect("tmp");
    let base = mdkb::domain::canonicalize_plain(tmp.path()).expect("canonicalize");
    let main = base.join("real/w");
    std::fs::create_dir_all(main.join(".git/worktrees/x")).expect("main .git");
    std::os::unix::fs::symlink(base.join("real"), base.join("link")).expect("symlink");
    let wt = base.join("elsewhere/wt");
    std::fs::create_dir_all(&wt).expect("worktree dir");
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}/link/w/.git/worktrees/x\n", base.display()),
    )
    .expect(".git file");

    assert_eq!(mdkb::cli::repos::caller_root(&wt), main);
}
