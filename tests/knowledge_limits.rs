//! Story 092-9289: `scope = knowledge` gives documents and memory a limit each.
//!
//! Before the fix both corpora shared one limit with documents first, so a query
//! with `limit` or more matching documents returned zero memory hits.

use std::path::{Path, PathBuf};

use mdkb::daemon::config::DaemonConfig;
use mdkb::daemon::registry::RepoRegistry;
use mdkb::mcp::dispatch::cross_repo_search_impl;
use mdkb::mcp::tools::SearchParams;
use serde_json::{Value, json};

#[path = "common/cli.rs"]
mod cli;

const QUERY: &str = "Orbit Lantern Observatory";
const LIMIT: usize = 2;

/// A store with three matching documents (more than `LIMIT`) and one matching memory entry.
fn fixture(parent: &Path) -> PathBuf {
    let root = parent.join("repo");
    std::fs::create_dir(&root).unwrap();
    let root = mdkb::domain::canonicalize_plain(&root).unwrap();
    mdkb::cli::handlers::handle_init(&root).unwrap();
    std::fs::create_dir(root.join("docs")).unwrap();
    for n in 0..3 {
        std::fs::write(
            root.join(format!("docs/guide{n}.md")),
            format!("# Orbit Lantern Observatory {n}\n\nOrbit Lantern Observatory is a project decision.\n"),
        )
        .unwrap();
    }
    let update = cli::run(&["update"], &root);
    assert!(update.status.success(), "{update:?}");
    let added = cli::run(
        &[
            "memory",
            "add",
            "orbit-lantern-memory",
            "-t",
            "Orbit Lantern Observatory memory",
            "-c",
            "Orbit Lantern Observatory records a decision for this repository.",
            "--entry-type",
            "decision",
            "--source-type",
            "user_statement",
        ],
        &root,
    );
    assert!(added.status.success(), "{added:?}");
    root
}

fn corpus_counts(value: &Value) -> (usize, usize) {
    let hits = value["hits"].as_array().unwrap();
    let count = |corpus: &str| hits.iter().filter(|hit| hit["corpus"] == corpus).count();
    (count("docs"), count("memory"))
}

/// Catches: memory starved to zero hits when documents fill the shared limit.
#[test]
fn cli_knowledge_returns_memory_when_documents_fill_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    let root = fixture(dir.path());
    let out = cli::run(
        &[
            "--format",
            "json",
            "search",
            QUERY,
            "--scope",
            "knowledge",
            "--limit",
            "2",
        ],
        &root,
    );
    assert!(out.status.success(), "{out:?}");
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(corpus_counts(&value), (LIMIT, 1), "{value}");
    assert_eq!(value["truncated"], true);
}

/// Catches: the MCP tool and `--root` keeping the shared limit.
#[tokio::test]
async fn mcp_and_root_knowledge_return_memory_when_documents_fill_the_limit() {
    let state = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let root = fixture(dir.path());
    std::fs::write(
        home.join(".mdkb/repos.json"),
        json!({"version": 1, "repos": [{"root": root}]}).to_string(),
    )
    .unwrap();
    let registry = std::sync::Arc::new(RepoRegistry::new(DaemonConfig {
        whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().to_string()],
        state_dir: Some(state.path().to_path_buf()),
        ..DaemonConfig::default()
    }));
    registry.get_or_open(&root).unwrap();
    let params: SearchParams = serde_json::from_value(json!({
        "scope": "knowledge", "query": QUERY, "root": "*", "limit": LIMIT,
    }))
    .unwrap();
    let (output, _) = cross_repo_search_impl(&registry, &params, &[])
        .await
        .unwrap();
    let mcp: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(corpus_counts(&mcp), (LIMIT, 1), "mcp: {mcp}");
    assert_eq!(mcp["truncated"], true);

    let root_arg = root.display().to_string();
    let cli_out = cli::command()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&root)
        .args([
            "--format",
            "json",
            "search",
            QUERY,
            "--scope",
            "knowledge",
            "--root",
            &root_arg,
            "--limit",
            "2",
        ])
        .output()
        .unwrap();
    assert!(cli_out.status.success(), "{cli_out:?}");
    let cli_value: Value = serde_json::from_slice(&cli_out.stdout).unwrap();
    assert_eq!(
        corpus_counts(&cli_value),
        (LIMIT, 1),
        "cli --root: {cli_value}"
    );
}
