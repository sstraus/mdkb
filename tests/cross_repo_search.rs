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
use mdkb::mcp::dispatch::{cross_repo_search_impl, resolve_root_selector};
use mdkb::mcp::tools::SearchParams;
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use serde_json::json;

#[path = "common/cli.rs"]
mod cli;

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
    assert!(
        output.contains("Searched 2 of 2 repos (1 known, 1 discovered)"),
        "{output}"
    );
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

/// Catches: canonical literals taking the legacy docs/memory fan-out, dropping
/// a closed repo, or using a per-repo rather than global result limit.
#[tokio::test]
async fn canonical_literal_fan_out_reports_both_roots_and_global_limit() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let first = repo_with_entry(repos.path(), "first", "unrelated");
    let second = repo_with_entry(repos.path(), "second", "unrelated");
    for root in [&first, &second] {
        std::fs::write(root.join("labels.js"), "const label = 'orbitLantern';\n").unwrap();
        let output = cli::run(&["code", "index", "labels.js"], root);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&first).expect("open first");
    registry.get_or_open(&second).expect("open second");
    assert!(
        registry.get(&first).is_none(),
        "the first root must be closed"
    );

    for (limit, expected_truncated) in [(1, true), (2, false)] {
        let params: SearchParams = serde_json::from_value(json!({
            "scope": "literals", "query": "orbitLantern", "root": "*", "limit": limit,
        }))
        .unwrap();
        let (output, count) = cross_repo_search_impl(&registry, &params, &[])
            .await
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).expect("canonical JSON");
        assert_eq!(value["scope"], "literals", "{value}");
        assert_eq!(value["unit"], "ast_literal", "{value}");
        assert_eq!(count, limit, "{value}");
        assert_eq!(value["hits"].as_array().unwrap().len(), limit, "{value}");
        assert_eq!(value["truncated"], expected_truncated, "{value}");
        assert_eq!(
            value["coverage"]["roots"].as_array().unwrap().len(),
            2,
            "{value}"
        );
        assert_eq!(value["status"], "complete", "{value}");
        for hit in value["hits"].as_array().unwrap() {
            assert_eq!(hit["content"], "orbitLantern", "{value}");
        }
        if limit == 2 {
            let repos: std::collections::HashSet<_> = value["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["repo"].as_str().unwrap())
                .collect();
            assert_eq!(repos.len(), 2, "{value}");
        }
    }
}

/// Catches: CLI and MCP disagreeing about global truncation or silently
/// calling an initialized root with no code index a complete no-match.
#[tokio::test]
async fn cli_and_mcp_multi_root_literals_agree_with_unavailable_index() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let home = repos.path().join("home");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let first = repo_with_entry(repos.path(), "first", "unrelated");
    let second = repo_with_entry(repos.path(), "second", "unrelated");
    std::fs::write(
        first.join("labels.js"),
        "const a = 'orbitLantern'; const b = 'orbitLantern';\n",
    )
    .unwrap();
    let indexed = cli::run(&["code", "index", "labels.js"], &first);
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(first.join(".mdkb/code.sqlite").exists());
    // Init creates an empty code database. Remove only this fixture's database
    // to exercise an unavailable index rather than an empty indexed corpus.
    assert!(second.join(".mdkb/code.sqlite").exists());
    std::fs::remove_file(second.join(".mdkb/code.sqlite")).unwrap();
    let map_roots = [first.as_path(), second.as_path()]
        .iter()
        .map(|root| json!({"root": root}))
        .collect::<Vec<_>>();
    std::fs::write(
        home.join(".mdkb/repos.json"),
        json!({"version": 1, "repos": map_roots}).to_string(),
    )
    .unwrap();
    let selected = format!("{},{}", first.display(), second.display());
    let cli_output = cli::command()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&first)
        .args([
            "--format",
            "json",
            "search",
            "orbitLantern",
            "--scope",
            "literals",
            "--root",
            &selected,
            "--limit",
            "1",
        ])
        .output()
        .unwrap();
    assert!(
        cli_output.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_output.stderr)
    );
    let cli_value: serde_json::Value = serde_json::from_slice(&cli_output.stdout).unwrap();

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&first).unwrap();
    registry.get_or_open(&second).unwrap();
    let params: SearchParams = serde_json::from_value(json!({
        "scope": "literals", "query": "orbitLantern", "root": "*", "limit": 1,
    }))
    .unwrap();
    let (mcp_output, count) = cross_repo_search_impl(&registry, &params, &[])
        .await
        .unwrap();
    let mcp_value: serde_json::Value = serde_json::from_str(&mcp_output).unwrap();
    assert_eq!(cli_value, mcp_value, "CLI/MCP multi-root literals diverged");
    assert_eq!(count, 1);
    assert_eq!(cli_value["hits"].as_array().unwrap().len(), 1);
    assert_eq!(cli_value["truncated"], true);
    assert_eq!(cli_value["status"], "partial");
    assert_eq!(cli_value["freshness"]["state"], "unknown");
    let roots = cli_value["coverage"]["roots"].as_array().unwrap();
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[0]["status"], "complete");
    assert_eq!(roots[1]["status"], "unavailable");
}

/// Catches: legacy fan-out collapsing two matching memory entries into one
/// pseudo-hit, applying per-root limits, or hiding a selected root in JSON.
#[tokio::test]
async fn canonical_indexed_multi_root_results_are_per_entry_and_match_cli_mcp() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let home = repos.path().join("home");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let mut roots = Vec::new();
    for (name, memory_count) in [("first", 2), ("second", 1)] {
        let root = repos.path().join(name);
        std::fs::create_dir(&root).unwrap();
        let root = mdkb::domain::canonicalize_plain(&root).unwrap();
        mdkb::cli::handlers::handle_init(&root).unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs/guide.md"),
            "# Orbit Lantern\n\nOrbit Lantern is a project decision.\n",
        )
        .unwrap();
        let update = cli::run(&["update"], &root);
        assert!(update.status.success(), "{update:?}");
        for n in 0..memory_count {
            let id = format!("orbit-lantern-{name}-{n}");
            let title = format!("Orbit Lantern {name} decision {n}");
            let content = format!("Orbit Lantern {name} records decision {n} for this repository.");
            let entry_type = if n == 0 { "decision" } else { "topic" };
            let source_type = "user_statement";
            let added = cli::run(
                &[
                    "memory",
                    "add",
                    &id,
                    "-t",
                    &title,
                    "-c",
                    &content,
                    "--entry-type",
                    entry_type,
                    "--source-type",
                    source_type,
                ],
                &root,
            );
            assert!(added.status.success(), "{added:?}");
        }
        roots.push(root);
    }
    let map_roots = roots
        .iter()
        .map(|root| json!({"root": root}))
        .collect::<Vec<_>>();
    std::fs::write(
        home.join(".mdkb/repos.json"),
        json!({"version": 1, "repos": map_roots}).to_string(),
    )
    .unwrap();
    let selected = format!("{},{}", roots[0].display(), roots[1].display());
    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    for root in &roots {
        registry.get_or_open(root).unwrap();
    }
    assert!(
        registry.get(&roots[0]).is_none(),
        "first root must be closed"
    );

    for (scope, limit, unit) in [
        ("docs", 1, "document"),
        ("memory", 2, "memory_entry"),
        ("knowledge", 2, "knowledge_result"),
    ] {
        let limit_arg = limit.to_string();
        let mut args = vec![
            "--format",
            "json",
            "search",
            "Orbit Lantern",
            "--scope",
            scope,
            "--root",
            selected.as_str(),
            "--limit",
            limit_arg.as_str(),
        ];
        if scope != "knowledge" {
            args.extend(["--search-contract", "v2"]);
        }
        let cli_output = cli::command()
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .current_dir(&roots[0])
            .args(&args)
            .output()
            .unwrap();
        assert!(cli_output.status.success(), "{scope}: {cli_output:?}");
        let cli_value: serde_json::Value = serde_json::from_slice(&cli_output.stdout).unwrap();
        let mut params: SearchParams = serde_json::from_value(json!({
            "scope": scope, "query": "Orbit Lantern", "root": "*", "limit": limit,
        }))
        .unwrap();
        if scope != "knowledge" {
            params.contract = Some("v2".into());
        }
        let (mcp_output, count) = cross_repo_search_impl(&registry, &params, &[])
            .await
            .unwrap();
        let mcp_value: serde_json::Value = serde_json::from_str(&mcp_output).unwrap();
        assert_eq!(cli_value, mcp_value, "{scope}: transport mismatch");
        assert_eq!(cli_value["scope"], scope);
        assert_eq!(cli_value["unit"], unit);
        assert_eq!(cli_value["status"], "partial");
        assert_eq!(cli_value["freshness"]["state"], "unknown");
        assert_eq!(cli_value["truncated"], true);
        assert_eq!(cli_value["hits"].as_array().unwrap().len(), limit);
        assert_eq!(count, limit);
        assert_eq!(cli_value["coverage"]["roots"].as_array().unwrap().len(), 2);
        if scope == "memory" {
            let ids: Vec<_> = cli_value["hits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| hit["value"]["id"].as_str().unwrap())
                .collect();
            assert_eq!(ids, ["orbit-lantern-first-0", "orbit-lantern-first-1"]);
        }
        for hit in cli_value["hits"].as_array().unwrap() {
            assert_eq!(hit["repo"], roots[0].display().to_string());
            if scope == "knowledge" {
                assert!(hit["corpus"] == "docs" || hit["corpus"] == "memory");
            }
        }
    }
    // Catches: the MCP canonical branch dropping its existing memory kind filter.
    let typed_cli = cli::command()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&roots[0])
        .args([
            "--format",
            "json",
            "search",
            "Orbit Lantern",
            "--scope",
            "memory",
            "--root",
            &selected,
            "--limit",
            "3",
            "--search-contract",
            "v2",
            "--entry-type",
            "decision",
        ])
        .output()
        .unwrap();
    assert!(typed_cli.status.success(), "{typed_cli:?}");
    let typed_cli: serde_json::Value = serde_json::from_slice(&typed_cli.stdout).unwrap();
    let typed_params: SearchParams = serde_json::from_value(json!({
        "scope": "memory", "contract": "v2", "query": "Orbit Lantern",
        "root": "*", "limit": 3, "kind": "decision",
    }))
    .unwrap();
    let (typed_mcp, _) = cross_repo_search_impl(&registry, &typed_params, &[])
        .await
        .unwrap();
    assert_eq!(
        typed_cli,
        serde_json::from_str::<serde_json::Value>(&typed_mcp).unwrap()
    );
    let typed_ids: Vec<_> = typed_cli["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["value"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        typed_ids,
        ["orbit-lantern-first-0", "orbit-lantern-second-0"]
    );
    assert_eq!(typed_cli["truncated"], false);

    let legacy_cli = cli::command()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&roots[0])
        .args([
            "--format",
            "json",
            "search",
            "Orbit Lantern",
            "--scope",
            "memory",
            "--root",
            &selected,
        ])
        .output()
        .unwrap();
    assert!(
        !legacy_cli.status.success(),
        "legacy multi-root JSON must refuse"
    );
    let (legacy_mcp, _) = cross_repo_search_impl(&registry, &memory_search("Orbit Lantern"), &[])
        .await
        .unwrap();
    assert!(
        !legacy_mcp.starts_with('{'),
        "legacy MCP must stay human text"
    );

    // Catches: an unavailable indexed store being counted as an empty search.
    std::fs::remove_file(roots[1].join(".mdkb/index.sqlite")).unwrap();
    let unavailable_cli = cli::command()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&roots[0])
        .args([
            "--format",
            "json",
            "search",
            "Orbit Lantern",
            "--scope",
            "memory",
            "--root",
            &selected,
            "--limit",
            "3",
            "--search-contract",
            "v2",
        ])
        .output()
        .unwrap();
    assert!(unavailable_cli.status.success(), "{unavailable_cli:?}");
    let unavailable_cli: serde_json::Value =
        serde_json::from_slice(&unavailable_cli.stdout).unwrap();
    let unavailable_params: SearchParams = serde_json::from_value(json!({
        "scope": "memory", "contract": "v2", "query": "Orbit Lantern",
        "root": "*", "limit": 3,
    }))
    .unwrap();
    let (unavailable_mcp, _) = cross_repo_search_impl(&registry, &unavailable_params, &[])
        .await
        .unwrap();
    assert_eq!(
        unavailable_cli,
        serde_json::from_str::<serde_json::Value>(&unavailable_mcp).unwrap()
    );
    assert_eq!(unavailable_cli["status"], "partial");
    assert_eq!(
        unavailable_cli["coverage"]["roots"][1]["status"],
        "unavailable"
    );
    assert_eq!(unavailable_cli["hits"].as_array().unwrap().len(), 2);
}

/// Catches: a confidence filter applied after a bounded candidate window
/// hiding an eligible indexed memory entry that ranked just outside it.
#[tokio::test]
async fn canonical_memory_confidence_filter_reaches_past_unqualified_prefix() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let root = repo_with_entry(repos.path(), "confidence", "unrelated");
    let ctx = Context::open(&root).unwrap();
    let now = chrono::Utc::now().timestamp();
    let seed = mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "unrelated-entry")
        .unwrap()
        .unwrap();
    let query = "verifier_lock";
    for (n, detail) in ["spruce", "basalt", "harbor"].into_iter().enumerate() {
        let mut entry = seed.clone();
        entry.id = format!("confidence-candidate-{n}");
        entry.title = format!("{query} {detail} decision");
        entry.content = format!("{query} {detail} records a separate maintenance decision.");
        entry.created_at = now;
        entry.updated_at = now;
        entry.source_type = SourceType::Inference;
        add_entry(&ctx.conn, &entry).unwrap();
    }
    let unfiltered = mdkb::core::memory::handle_memory_search(&ctx, query, 3, None).unwrap();
    assert_eq!(unfiltered.len(), 3, "all three entries must be indexed");
    let eligible_id = unfiltered[2].id.clone();
    let mut eligible = unfiltered[2].clone();
    eligible.source_type = SourceType::OfficialDocs;
    mdkb::store::memory::update_entry(&ctx.conn, &eligible).unwrap();
    let unfiltered = mdkb::core::memory::handle_memory_search(&ctx, query, 3, None).unwrap();
    assert_eq!(unfiltered.len(), 3);
    assert_eq!(
        unfiltered[2].id, eligible_id,
        "confidence changed retrieval order: {unfiltered:?}"
    );
    assert!(unfiltered[..2].iter().all(|entry| entry.confidence() < 0.4));
    assert!(unfiltered[2].confidence() >= 0.4);
    drop(ctx);

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&root).unwrap();
    let params: SearchParams = serde_json::from_value(json!({
        "scope": "memory", "contract": "v2", "query": query,
        "root": "*", "limit": 1, "min_confidence": 0.4,
    }))
    .unwrap();
    let (output, count) = cross_repo_search_impl(&registry, &params, &[])
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(count, 1, "qualifying indexed entry was hidden: {value}");
    assert_eq!(value["hits"][0]["value"]["id"], eligible_id);
    assert_eq!(value["truncated"], false);
}

/// Catches: treating a relevance-gated short page as corpus exhaustion while
/// a high-confidence lexical hit remains beyond the bounded BM25 prefix.
#[tokio::test]
async fn canonical_memory_confidence_filter_widens_after_relevance_gate_short_page() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    let root = repo_with_entry(repos.path(), "gated", "unrelated");
    let ctx = Context::open(&root).unwrap();
    let query = "verifier_lock";
    let now = chrono::Utc::now().timestamp();
    let seed = mdkb::store::memory::get_entry_without_tracking(&ctx.conn, "unrelated-entry")
        .unwrap()
        .unwrap();
    for n in 0..8 {
        let mut entry = seed.clone();
        entry.id = format!("gate-decoy-{n}");
        entry.title = format!("Verifier lock maintenance note {n}");
        entry.content = format!(
            "Verifier lock maintenance note {n} records verifier lock maintenance details."
        );
        entry.created_at = now;
        entry.updated_at = now;
        entry.source_type = SourceType::Inference;
        add_entry(&ctx.conn, &entry).unwrap();
    }
    let mut eligible = seed;
    eligible.id = "gate-eligible".into();
    eligible.title = "verifier_lock independent decision".into();
    eligible.content = "This entry records a separate decision about verifier_lock.".into();
    eligible.created_at = now;
    eligible.updated_at = now;
    eligible.source_type = SourceType::OfficialDocs;
    add_entry(&ctx.conn, &eligible).unwrap();
    let short = mdkb::core::memory::handle_memory_search(&ctx, query, 2, None).unwrap();
    assert!(
        short.len() < 2,
        "fixture must create a short gated page: {short:?}"
    );
    assert!(short.iter().all(|entry| entry.id != eligible.id));
    let wide = mdkb::core::memory::handle_memory_search(&ctx, query, 10, None).unwrap();
    assert!(wide.iter().any(|entry| entry.id == eligible.id), "{wide:?}");
    drop(ctx);

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&root).unwrap();
    let params: SearchParams = serde_json::from_value(json!({
        "scope": "memory", "contract": "v2", "query": query,
        "root": "*", "limit": 1, "min_confidence": 0.4,
    }))
    .unwrap();
    let (output, count) = cross_repo_search_impl(&registry, &params, &[])
        .await
        .unwrap();
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(count, 1, "qualifying entry was hidden: {value}");
    assert_eq!(value["hits"][0]["value"]["id"], eligible.id);
}

/// Catches: explicit source/semantic-code wildcard searches falling into
/// the legacy docs+memory fan-out and returning a false no-match.
#[tokio::test]
async fn unsupported_canonical_scopes_never_fall_through_to_legacy_cross_repo_corpus() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let first = repo_with_entry(repos.path(), "first", "zonk_harvest");
    let second = repo_with_entry(repos.path(), "second", "zonk_harvest");
    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&first).expect("open first");
    registry.get_or_open(&second).expect("open second");
    for scope in ["source", "semantic-code"] {
        let mut params = memory_search("zonk_harvest");
        params.scope = Some(scope.to_string());
        params.root = Some("*".to_string());
        let result = cross_repo_search_impl(&registry, &params, &[]).await;
        assert!(
            result.is_err(),
            "{scope} must not return the docs/memory corpus: {result:?}"
        );
    }
}

/// Roll a store's recorded schema back without opening it through `Context`,
/// which would migrate it straight away.
fn roll_back_schema(root: &Path, version: i32) {
    rusqlite::Connection::open(root.join(".mdkb/index.sqlite"))
        .expect("open store")
        .execute("UPDATE schema_version SET version = ?1", [version])
        .expect("roll back");
}

/// Story 219-9d67: `root="*"` skipped 24 of 38 stores and said so with a
/// paragraph each, while counting the stores it had merely found on disk as
/// "known".
///
/// Catches: discovered roots counted as known, and one paragraph per
/// outdated store instead of one line.
#[tokio::test]
async fn the_footer_counts_discovered_apart_and_names_outdated_stores_in_one_line() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let parent = repo_with_entry(repos.path(), "parent", "zonk_harvest");
    let _discovered = repo_with_entry(&parent, "nested", "unrelated");
    let outdated_a = repo_with_entry(repos.path(), "old_a", "unrelated");
    let outdated_b = repo_with_entry(repos.path(), "old_b", "unrelated");

    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    for root in [&parent, &outdated_a, &outdated_b] {
        registry.get_or_open(root).expect("open");
    }
    roll_back_schema(&outdated_a, 30);
    roll_back_schema(&outdated_b, 17);

    let (output, _) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"), &[])
        .await
        .expect("search");

    assert!(
        output.contains("_Searched 2 of 4 repos (3 known, 1 discovered)._"),
        "{output}"
    );
    assert!(
        output.contains("**Schema outdated, not searched (2):** old_a, old_b."),
        "{output}"
    );
    assert!(
        output.contains("mdkb repos refresh --only outdated"),
        "the line carries the fix: {output}"
    );
    assert!(
        !output.contains("Not searched") && !output.contains("A read-only command"),
        "no paragraph per store: {output}"
    );
}

/// Criterion 2 of story 219-9d67: after the refresh, `root="*"` reads the store
/// it used to skip.
///
/// Catches: a store that stays unreadable to the fan-out after it was migrated.
#[tokio::test]
async fn a_refreshed_store_is_read_by_the_next_cross_repo_search() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let old = repo_with_entry(repos.path(), "old", "zonk_harvest");
    let registry = std::sync::Arc::new(RepoRegistry::new(one_slot_config(state.path())));
    registry.get_or_open(&old).expect("open");
    roll_back_schema(&old, 30);

    let (before, found) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"), &[])
        .await
        .expect("search");
    assert_eq!(found, 0, "{before}");
    assert!(
        before.contains("Schema outdated, not searched (1)"),
        "{before}"
    );

    mdkb::core::refresh::refresh_store(&old, "t1").expect("refresh");

    let (after, found) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"), &[])
        .await
        .expect("search");
    assert!(found >= 1, "{after}");
    assert!(!after.contains("Schema outdated"), "{after}");
}

/// Catches: an operator who cannot silence a store. An `ignore` entry in
/// `daemon.toml` removes the root from discovery AND from `root="*"`, while the
/// stores next to it stay.
#[test]
fn an_ignored_root_is_absent_from_the_wildcard() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let parent = repo_with_entry(repos.path(), "parent", "unrelated");
    let kept = repo_with_entry(&parent, "kept", "unrelated");
    let silenced = repo_with_entry(&parent, "silenced", "unrelated");

    let config = DaemonConfig {
        ignore: vec![silenced.to_string_lossy().to_string()],
        ..one_slot_config(state.path())
    };
    let registry = RepoRegistry::new(config);
    registry.get_or_open(&parent).expect("open the parent");

    let resolved = resolve_root_selector(&registry, Some("*"), &[]).expect("resolve");

    let mut roots = resolved.roots;
    roots.sort();
    assert_eq!(roots, vec![parent, kept]);
    assert!(!registry.discoverable_roots().contains(&silenced));
}

/// Catches: a name collision with no short disambiguator. `plugins` names two
/// repos and is refused; `a/plugins` is the path suffix that picks one.
#[test]
fn a_path_suffix_resolves_a_name_that_is_ambiguous() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let first = repo_with_entry(&repos.path().join("a"), "plugins", "unrelated");
    let second = repo_with_entry(&repos.path().join("b"), "plugins", "unrelated");

    let registry = RepoRegistry::new(one_slot_config(state.path()));
    registry.get_or_open(&first).expect("open a/plugins");
    registry.get_or_open(&second).expect("open b/plugins");

    assert!(
        resolve_root_selector(&registry, Some("plugins"), &[]).is_err(),
        "the bare name stays ambiguous"
    );
    let resolved = resolve_root_selector(&registry, Some("a/plugins"), &[]).expect("suffix");
    assert_eq!(resolved.roots, vec![first]);
    let resolved = resolve_root_selector(&registry, Some("b/plugins"), &[]).expect("suffix");
    assert_eq!(resolved.roots, vec![second]);
}

/// Catches: a hygiene change that makes discovery write. Skipping `.tmp` and
/// honouring the ignore list must still open nothing, register nothing and
/// leave every store file untouched.
#[test]
fn discovery_with_hygiene_still_opens_and_registers_nothing() {
    let state = tempfile::tempdir().expect("state");
    let repos = tempfile::tempdir().expect("repos");
    let parent = repo_with_entry(repos.path(), "parent", "unrelated");
    let nested = repo_with_entry(&parent, "nested", "unrelated");
    let copy = repo_with_entry(&parent.join(".tmp/cov-audit"), "nested", "unrelated");

    let registry = RepoRegistry::new(one_slot_config(state.path()));
    registry.get_or_open(&parent).expect("open only the parent");
    let stamp = |root: &Path| {
        std::fs::metadata(root.join(".mdkb/index.sqlite"))
            .expect("index")
            .modified()
            .expect("mtime")
    };
    let (nested_before, copy_before) = (stamp(&nested), stamp(&copy));

    let resolved = resolve_root_selector(&registry, Some("*"), &[]).expect("resolve");

    assert!(resolved.roots.contains(&nested));
    assert!(!resolved.roots.contains(&copy), "the .tmp copy is skipped");
    assert_eq!(registry.known_roots(), vec![parent], "nothing registered");
    assert_eq!(registry.active_count(), 1, "nothing opened");
    assert_eq!((stamp(&nested), stamp(&copy)), (nested_before, copy_before));
}
