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
    let root = root.canonicalize().expect("canonicalize");
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

    let registry = RepoRegistry::new(one_slot_config(state.path()));
    registry.get_or_open(&open).expect("open the repo");

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"))
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

    let registry = RepoRegistry::new(one_slot_config(state.path()));
    // Both roots become known; the second open evicts the first, so the repo
    // holding the match is known-but-closed — the daemon's steady state.
    registry.get_or_open(&closed).expect("open the first repo");
    registry.get_or_open(&open).expect("open the second repo");
    assert!(
        registry.get(&closed).is_none(),
        "the fixture needs the first repo evicted"
    );
    assert_eq!(registry.known_roots().len(), 2, "both roots are known");

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"))
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

    let registry = RepoRegistry::new(one_slot_config(state.path()));
    registry.get_or_open(&closed).expect("open the first repo");
    registry.get_or_open(&working_in).expect("open the second");

    let before: Vec<PathBuf> = registry.list().into_iter().map(|(p, _)| p).collect();
    let _ = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"))
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

    let registry = RepoRegistry::new(one_slot_config(state.path()));
    registry.get_or_open(&from_the_future).expect("first");
    registry.get_or_open(&healthy).expect("second");
    {
        let ctx = Context::open(&from_the_future).expect("open");
        ctx.conn
            .execute("UPDATE schema_version SET version = ?", [9999])
            .expect("write a schema version this binary cannot serve");
    }

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("zonk_harvest"))
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

    let registry = RepoRegistry::new(one_slot_config(state.path()));
    registry.get_or_open(&first).expect("first");
    registry.get_or_open(&second).expect("second");

    let (output, count) = cross_repo_search_impl(&registry, &memory_search("quelli_frast"))
        .await
        .expect("search");

    assert_eq!(count, 0, "nothing matches that token: {output}");
    assert!(
        output.contains("Searched 2 of 2 known repos"),
        "an empty answer must still state its coverage: {output}"
    );
}
