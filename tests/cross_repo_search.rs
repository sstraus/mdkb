//! A cross-repo search covers every repo the daemon knows, and says what it read.
//!
//! Story 125-7419. `cross_repo_search_impl` fanned out over
//! `RepoRegistry::all_handles()` — the repos that happen to be OPEN, at most
//! `max_active_repos` of them, LRU-evicted, and empty after a restart. A repo
//! that was known but closed was absent from the answer without being
//! mentioned, so "No results across repos" meant both "nothing matched" and
//! "never looked". Same false-negative class as stories 106-b12a, 113-d6df,
//! 127-fcaf and 128-870a.
//!
//! The fixtures below keep exactly one repo open (`max_active_repos = 1`) and
//! put the thing being searched for in the repo that is closed.

use std::path::{Path, PathBuf};

use mdkb::core::Context;
use mdkb::daemon::config::DaemonConfig;
use mdkb::daemon::registry::RepoRegistry;
use mdkb::mcp::dispatch::cross_repo_search_impl;
use mdkb::mcp::tools::SearchParams;
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use serde_json::json;

/// A real store under `parent/name`, with one memory entry whose title carries
/// `needle` — a nonsense token, so a match cannot come from anywhere else.
///
/// The needles below are identifier-shaped (`zonk_harvest`, with the
/// underscore) on purpose. No ONNX model runs in the test environment, so the vector leg is
/// absent and every candidate has to pass the lexical arm of
/// `mdkb::store::hybrid::admits`. `strong_lexical_match` admits on a verbatim
/// identifier, on a three-word phrase, or on two distinct rare terms — a single
/// bare token is none of those, and a fixture built on one matches nothing even
/// in a repo that IS open.
fn repo_with_entry(parent: &Path, name: &str, needle: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("create repo root");
    // Plain, not `\\?\C:\...`: the registry keys a root the way
    // `canonicalize_plain` spells it, and a prefixed path names a repo it
    // would never find.
    let root = mdkb::domain::canonicalize_plain(&root).expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");

    let ctx = Context::open(&root).expect("open store");
    let now = chrono::Utc::now().timestamp();
    add_entry(
        &ctx.conn,
        &MemoryEntry {
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

/// A daemon config whose state and whitelist both stay inside the test's temp
/// directories: nothing here may read or write the real `~/.mdkb`.
///
/// `max_active_repos = 1` is the point of the fixture — the second repo opened
/// evicts the first, which is what a real daemon does to the sixth repo.
fn one_slot_config(state: &Path) -> DaemonConfig {
    DaemonConfig {
        max_active_repos: 1,
        whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().to_string()],
        state_dir: Some(state.to_path_buf()),
        ..DaemonConfig::default()
    }
}

/// The same search over documents. `scope` decides whether the empty-document
/// -registry probe is a question about the answer at all.
fn docs_search(query: &str) -> SearchParams {
    serde_json::from_value(json!({
        "query": query,
        "root": "*",
        "scope": "docs",
        "limit": 10,
    }))
    .expect("search params")
}

fn memory_search(query: &str) -> SearchParams {
    serde_json::from_value(json!({
        "query": query,
        "root": "*",
        "scope": "memory",
        "limit": 10,
    }))
    .expect("search params")
}

/// The control: the fan-out still searches the repo that IS open. If this ever
/// fails, the fixture is broken and the tests below prove nothing.
#[tokio::test]
async fn an_open_repo_is_searched() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let open = repo_with_entry(repos.path(), "open", "zonk_harvest");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&open).expect("open the repo");

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"), &[])
        .await
        .expect("search");

    assert!(count >= 1, "the open repo must be searched: {output}");
    assert!(output.contains("zonk_harvest"), "{output}");
}

/// The defect: a repo the daemon KNOWS but has not got open must still be
/// searched. Here it is the only repo holding the match.
#[tokio::test]
async fn a_known_repo_that_is_not_open_is_still_searched() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let closed = repo_with_entry(repos.path(), "closed", "zonk_harvest");
    let open = repo_with_entry(repos.path(), "open", "unrelated");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    // Both roots become known; the second open evicts the first, so the repo
    // holding the match is known-but-closed — the daemon's steady state.
    registry.get_or_open(&closed).expect("open the first repo");
    registry.get_or_open(&open).expect("open the second repo");
    assert!(
        registry.get(&closed).is_none(),
        "the fixture needs the first repo evicted"
    );
    assert_eq!(registry.known_roots().len(), 2, "both roots are known");

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"), &[])
        .await
        .expect("search");

    assert!(
        count >= 1 && output.contains("zonk_harvest"),
        "a known repo must be searched even with no handle open for it: {output}"
    );
    assert!(
        output.contains("Searched 2 of 2 known repos"),
        "the answer must state its coverage: {output}"
    );
}

/// Criterion 2: reading is not mounting. The fan-out must leave the active-handle
/// set exactly as it found it — no new handle, so no watcher and no eviction of
/// the repo the caller is actually working in.
#[tokio::test]
async fn the_fan_out_takes_no_handle_and_evicts_nothing() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let closed = repo_with_entry(repos.path(), "closed", "zonk_harvest");
    let working_in = repo_with_entry(repos.path(), "open", "unrelated");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&closed).expect("open the first repo");
    registry.get_or_open(&working_in).expect("open the second");

    let before: Vec<PathBuf> = registry.list().into_iter().map(|(p, _)| p).collect();
    let _ = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"), &[])
        .await
        .expect("search");
    let after: Vec<PathBuf> = registry.list().into_iter().map(|(p, _)| p).collect();

    assert_eq!(
        before, after,
        "a cross-repo read must not change which repos are open"
    );
    assert_eq!(registry.active_count(), 1, "the one slot is still the one");
    assert_eq!(
        after,
        vec![working_in],
        "the repo the caller works in must survive the fan-out"
    );
    assert!(
        registry.get(&closed).is_none(),
        "the searched-but-closed repo must not have been mounted"
    );
}

/// Criterion 4, the one that matters: a store this binary cannot read is
/// reported as NOT SEARCHED with its reason. Never counted as empty.
///
/// A schema from the future is not hypothetical — the installed binary
/// understood v27 while the store was v28, and 21 hook runs died on it.
#[tokio::test]
async fn a_store_this_binary_cannot_read_is_reported_not_counted_as_empty() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let from_the_future = repo_with_entry(repos.path(), "future", "zonk_harvest");
    let healthy = repo_with_entry(repos.path(), "healthy", "zonk_harvest");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&from_the_future).expect("first");
    registry.get_or_open(&healthy).expect("second");
    {
        let ctx = Context::open(&from_the_future).expect("open");
        ctx.conn
            .execute("UPDATE schema_version SET version = ?", [9999])
            .expect("write a schema version this binary cannot serve");
    }

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"), &[])
        .await
        .expect("one unreadable repo must not abort the fan-out");

    assert!(count >= 1, "the healthy repo is still searched: {output}");
    assert!(
        output.contains("Searched 1 of 2 known repos"),
        "the coverage must exclude the repo that was not read: {output}"
    );
    assert!(
        output.contains("Not searched (1)"),
        "the skipped repo must be named: {output}"
    );
    assert!(
        output.contains(&from_the_future.display().to_string()),
        "by its root: {output}"
    );
    assert!(
        output.contains("9999"),
        "and with the reason it could not be read: {output}"
    );
}

/// An empty answer must say how much was looked at. "No results" over one of
/// five repos and over five of five are different facts.
#[tokio::test]
async fn an_empty_result_states_how_much_was_searched() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let first = repo_with_entry(repos.path(), "first", "zonk_harvest");
    let second = repo_with_entry(repos.path(), "second", "zonk_harvest");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&first).expect("first");
    registry.get_or_open(&second).expect("second");

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("quelli_frast"), &[])
        .await
        .expect("search");

    assert_eq!(count, 0, "nothing matches that token: {output}");
    assert!(
        output.contains("Searched 2 of 2 known repos"),
        "an empty answer must still state its coverage: {output}"
    );
}

/// Story 140-822c: an empty collection registry is not the same fact as a
/// healthy corpus with no matching document. The answer must name both the
/// condition and the command that normally repairs it.
#[tokio::test]
async fn a_store_with_no_collections_is_not_reported_as_a_plain_no_match() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let empty = repo_with_entry(repos.path(), "empty", "unrelated");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&empty).expect("open empty store");

    let (output, count) = cross_repo_search_impl(&registry, &docs_search("quelli_frast"), &[])
        .await
        .expect("search");

    assert_eq!(count, 0, "the fixture has no match: {output}");
    assert!(output.contains("No registered collections (1)"), "{output}");
    assert!(output.contains(&empty.display().to_string()), "{output}");
    assert!(output.contains("mdkb update"), "{output}");
}

/// The same store, the same empty document registry, a memory query: the
/// notice must not appear. It answers "are there documents to search here",
/// which says nothing about a memory query — and the repo's own MCP rule is
/// that a token has to help the caller act.
#[tokio::test]
async fn a_memory_query_is_not_advised_to_run_mdkb_update() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let empty = repo_with_entry(repos.path(), "empty", "unrelated");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&empty).expect("open empty store");

    let (output, _) = cross_repo_search_impl(&registry, &memory_search("quelli_frast"), &[])
        .await
        .expect("search");

    assert!(!output.contains("No registered collections"), "{output}");
    assert!(!output.contains("mdkb update"), "{output}");
}

/// "Searched 2 of 2" for a two-item list reads as complete coverage on a
/// daemon that knows thirty stores — the same false confidence the footer was
/// added to destroy. The denominator is what exists; the sentence says which
/// of it the selector asked for.
#[tokio::test]
async fn a_named_selection_does_not_report_itself_as_full_coverage() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let alpha = repo_with_entry(repos.path(), "alpha", "zonk_harvest");
    let beta = repo_with_entry(repos.path(), "beta", "zonk_harvest");
    let gamma = repo_with_entry(repos.path(), "gamma", "zonk_harvest");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    for root in [&alpha, &beta, &gamma] {
        registry.get_or_open(root).expect("open");
    }

    let mut params = memory_search("quelli_frast");
    params.root = Some("alpha,beta".to_string());
    let (output, _) = cross_repo_search_impl(&registry, &params, &[])
        .await
        .expect("search");

    assert!(
        output.contains("_Searched 2 of 2 repos named (3 known)._"),
        "{output}"
    );
}

/// The same for a workspace: three stores exist, the client declared one.
#[tokio::test]
async fn a_rootless_search_names_the_workspace_as_its_denominator() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let workspace = repo_with_entry(repos.path(), "workspace", "zonk_harvest");
    let elsewhere = repo_with_entry(repos.path(), "elsewhere", "zonk_harvest");
    let further = repo_with_entry(repos.path(), "further", "zonk_harvest");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    for root in [&workspace, &elsewhere, &further] {
        registry.get_or_open(root).expect("open");
    }

    let mut params = memory_search("quelli_frast");
    params.root = None;
    let (output, _) = cross_repo_search_impl(&registry, &params, std::slice::from_ref(&workspace))
        .await
        .expect("search");

    assert!(
        output.contains("_Searched 1 of 1 repos in this workspace (3 known)._"),
        "{output}"
    );
}

/// Every path in the footer is charged on the turn, and on a "no results"
/// answer it is charged for nothing. A workspace of stores this binary cannot
/// read would otherwise print one absolute path per store.
#[tokio::test]
async fn the_not_searched_list_is_capped_and_says_how_many_it_left_out() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let readable = repo_with_entry(repos.path(), "readable", "zonk_harvest");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&readable).expect("open readable");
    for n in 0..7 {
        let future = repo_with_entry(repos.path(), &format!("future{n}"), "unrelated");
        registry
            .get_or_open(&future)
            .expect("register before breaking it");
        let ctx = Context::open(&future).expect("open");
        ctx.conn
            .execute("UPDATE schema_version SET version = ?", [9999])
            .expect("a schema version this binary cannot serve");
    }

    let (output, _) = cross_repo_search_impl(&registry, &memory_search("quelli_frast"), &[])
        .await
        .expect("search");

    assert!(output.contains("**Not searched (7):**"), "{output}");
    assert_eq!(
        output.matches("store schema is v9999").count(),
        5,
        "five named, the rest counted: {output}"
    );
    assert!(output.contains("…and 2 more"), "{output}");
}

/// Story 141-2032: a store below a known root must not depend on a client
/// having opened it first. Discovery is filesystem-only; opening for search is
/// still the existing read-only path.
#[tokio::test]
async fn a_nested_store_is_discovered_without_being_opened_first() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let parent = repo_with_entry(repos.path(), "parent", "unrelated");
    let nested = repo_with_entry(&parent, "nested", "nested_signal");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&parent).expect("open only the parent");
    assert_eq!(registry.known_roots(), vec![parent.clone()]);

    let before = std::fs::metadata(nested.join(".mdkb/index.sqlite"))
        .expect("nested index")
        .modified()
        .expect("mtime");
    let (output, count) = cross_repo_search_impl(&registry, &memory_search("nested_signal"), &[])
        .await
        .expect("search");
    let after = std::fs::metadata(nested.join(".mdkb/index.sqlite"))
        .expect("nested index")
        .modified()
        .expect("mtime");

    assert!(count >= 1 && output.contains("nested_signal"), "{output}");
    assert!(output.contains("Searched 2 of 2 known repos"), "{output}");
    assert_eq!(
        before, after,
        "discovery and read-only search must not mutate the store"
    );
    assert_eq!(
        registry.known_roots(),
        vec![parent],
        "discovery must not register the child"
    );
}

/// A `root`-less search means the declared workspace and every store nested
/// beneath it — not whatever happens to hold a live handle.
///
/// Story 141-2032 fixed the wiring; the test that came with it called
/// `default_roots` with synthetic paths, which `src/mcp/tools.rs` already
/// covers. What had no test was the wiring itself: `resolve_root_selector`
/// feeding `default_roots` the union of discovery and the open handles, and
/// the fan-out receiving a non-empty `client_scope`. Change that call to pass
/// the open handles as `known` — the exact regression the story fixed — and
/// only this test goes red.
#[tokio::test]
async fn a_rootless_search_reaches_the_stores_nested_under_the_declared_workspace() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let workspace = repo_with_entry(repos.path(), "workspace", "unrelated");
    let _nested = repo_with_entry(&workspace, "nested", "nested_signal");
    let elsewhere = repo_with_entry(repos.path(), "elsewhere", "zonk_harvest");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    // The repo OUTSIDE the workspace is the only one with a live handle: the
    // state that made the old default answer about it.
    registry.get_or_open(&elsewhere).expect("open elsewhere");

    let mut params = memory_search("nested_signal");
    params.root = None;
    let (output, count) =
        cross_repo_search_impl(&registry, &params, std::slice::from_ref(&workspace))
            .await
            .expect("search");

    assert!(
        count >= 1 && output.contains("nested_signal"),
        "the store nested under the declared workspace must answer: {output}"
    );
    assert!(
        !output.contains(&elsewhere.display().to_string()),
        "an open repo outside the workspace must not be searched: {output}"
    );
}

/// A tool that cannot fan out gets the workspace itself, not a refusal.
///
/// `default_roots` answers "every store in this workspace", which `search`
/// wants and `get` cannot use. When the declared path is itself a store, the
/// caller has already named its repo and there is nothing to disambiguate.
#[test]
fn a_rootless_single_target_call_means_the_declared_workspace() {
    use mdkb::mcp::dispatch::resolve_single_root;

    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let workspace = repo_with_entry(repos.path(), "workspace", "unrelated");
    let nested = repo_with_entry(&workspace, "nested", "nested_signal");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));

    let chosen = resolve_single_root(&registry, None, std::slice::from_ref(&workspace))
        .expect("the declared workspace is the answer");

    assert_eq!(chosen, workspace, "the workspace anchors the call");
    assert_ne!(
        chosen, nested,
        "a store nested under the workspace is the fan-out's business, not a \
         single-target call's"
    );
}

/// A container of repositories anchors no store, so it names no repo either.
/// The refusal is the answer, and it has to be one the caller can act on: the
/// count, and a sample of the paths.
#[test]
fn a_rootless_single_target_call_refuses_a_container_that_anchors_no_store() {
    use mdkb::mcp::dispatch::resolve_single_root;

    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let container = repos.path().join("container");
    std::fs::create_dir_all(&container).expect("container");
    let container = mdkb::domain::canonicalize_plain(&container).expect("canonicalize");
    let alpha = repo_with_entry(&container, "alpha", "unrelated");
    let beta = repo_with_entry(&container, "beta", "unrelated");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));

    let refusal = resolve_single_root(&registry, None, &[container])
        .expect_err("two stores and no anchor is the caller's choice to make");
    let message = refusal.to_string();

    assert!(message.contains("2 repos are in scope"), "{message}");
    assert!(message.contains(&alpha.display().to_string()), "{message}");
    assert!(message.contains(&beta.display().to_string()), "{message}");
}

/// Two declared workspaces that are each a store stay ambiguous: the caller
/// declared both, and picking the first is an accident of ordering, not a
/// decision.
#[test]
fn two_declared_workspaces_that_are_both_stores_stay_ambiguous() {
    use mdkb::mcp::dispatch::resolve_single_root;

    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let first = repo_with_entry(repos.path(), "first", "unrelated");
    let second = repo_with_entry(repos.path(), "second", "unrelated");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));

    let refusal = resolve_single_root(&registry, None, &[first, second])
        .expect_err("two declared stores name no single repo");

    assert!(
        refusal.to_string().contains("2 repos are in scope"),
        "{refusal}"
    );
}

/// An omitted scope means documents AND memory, across repos as within one.
///
/// `SearchParams` documents `scope` as "omit to search docs+memory", and
/// single-repo `search_impl` has always had a `None` arm that does both. The
/// fan-out collapsed `None` into the document arm, so the same call across
/// repos returned documents only — and said nothing about the half it skipped.
#[tokio::test]
async fn an_omitted_scope_searches_documents_and_memory() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let root = repo_with_entry(repos.path(), "both", "zonk_harvest");
    std::fs::write(
        root.join("doc.md"),
        "# Quelli frast\n\nA document about quelli_frast, indexed so the document leg has something to match.\n",
    )
    .expect("write doc");
    let update = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .arg("update")
        .current_dir(&root)
        .env("MDKB_NO_DAEMON", "1")
        .output()
        .expect("run update");
    assert!(update.status.success(), "update failed: {update:?}");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&root).expect("open");

    let mut params = memory_search("zonk_harvest");
    params.scope = None;
    let (output, count) = cross_repo_search_impl(&registry, &params, &[])
        .await
        .expect("search");

    assert!(count >= 1, "the memory half must answer: {output}");
    assert!(
        output.contains("zonk_harvest"),
        "the memory entry is the half the document leg cannot supply: {output}"
    );
}
