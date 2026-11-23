//! Black-box comparison of the explicit live-source contract on CLI (`code grep`) and MCP.
//! The CLI no longer has `--search-contract`; the MCP v2 envelope keeps its own tests.
use mdkb::mcp::server::McpServer;
use mdkb::mcp::tools::SearchParams;
use rmcp::handler::server::wrapper::Parameters;

#[path = "common/cli.rs"]
mod cli;

#[tokio::test]
async fn cli_and_mcp_source_or_share_locations_and_coverage() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::write(
        dir.path().join("sample.rs"),
        "// alpha beta\nfn alpha() {}\n",
    )
    .unwrap();
    let output = cli::run(
        &[
            "--format", "json", "code", "grep", "-e", "alpha", "-e", "beta", "--limit", "1",
        ],
        dir.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = McpServer::new(dir.path().to_path_buf())
        .search(Parameters(SearchParams {
            query: String::new(),
            patterns: Some(vec!["alpha".into(), "beta".into()]),
            path: None,
            regex: false,
            ignore_case: false,
            contract: None,
            root: None,
            limit: 1,
            collection: None,
            include_superseded: false,
            scope: Some("source".into()),
            kind: None,
            threshold: None,
            file: None,
            min_confidence: None,
            since: None,
        }))
        .await
        .unwrap();
    let mcp: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(cli["hits"], mcp["hits"]);
    assert_eq!(cli["status"], mcp["status"]);
    assert_eq!(cli["truncated"], mcp["truncated"]);
    assert_eq!(
        cli["hits"][0]["matched_patterns"],
        serde_json::json!([0, 1])
    );
    assert_eq!(cli["hits"][0]["line"], 1);
    assert_eq!(cli["truncated"], true);
}

/// Catches: CLI and MCP parsing the same OR alternatives into different AST
/// result units, locations, or coverage statements.
#[tokio::test]
async fn cli_and_mcp_literals_or_share_locations_and_coverage() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/labels.js"),
        "const first = 'alpha'; const second = 'beta';\n",
    )
    .unwrap();
    let indexed = cli::run(&["update", "--code"], dir.path());
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let output = cli::run(
        &[
            "--format",
            "json",
            "code",
            "grep",
            "--literals",
            "-e",
            "alpha",
            "-e",
            "beta",
        ],
        dir.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let result = McpServer::new(dir.path().to_path_buf())
        .search(Parameters(SearchParams {
            query: String::new(),
            patterns: Some(vec!["alpha".into(), "beta".into()]),
            path: None,
            regex: false,
            ignore_case: false,
            contract: None,
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: Some("literals".into()),
            kind: None,
            threshold: None,
            file: None,
            min_confidence: None,
            since: None,
        }))
        .await
        .unwrap();
    let mcp: serde_json::Value =
        serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(cli["hits"], mcp["hits"]);
    assert_eq!(cli["status"], mcp["status"]);
    assert_eq!(cli["freshness"], mcp["freshness"]);
    assert_eq!(cli["hits"].as_array().unwrap().len(), 2, "{cli}");
    assert_eq!(cli["hits"][0]["matched_patterns"], serde_json::json!([0]));
    assert_eq!(cli["hits"][1]["matched_patterns"], serde_json::json!([1]));
    assert_eq!(cli["hits"][0]["line"], 1);
    assert!(
        cli["hits"][1]["column"].as_u64().unwrap() > cli["hits"][0]["column"].as_u64().unwrap()
    );
}

/// Catches: MCP accepting the opt-in discriminator on a new scope and
/// returning a different response while claiming the overlapping v2 contract.
#[tokio::test]
async fn mcp_v2_discriminator_rejects_nonoverlapping_scopes() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    let result = McpServer::new(dir.path().to_path_buf())
        .search(Parameters(SearchParams {
            query: "needle".into(),
            patterns: None,
            path: None,
            regex: false,
            ignore_case: false,
            contract: Some("v2".into()),
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: Some("source".into()),
            kind: None,
            threshold: None,
            file: None,
            min_confidence: None,
            since: None,
        }))
        .await;
    let error = result.expect_err("v2 is reserved for overlapping scopes");
    assert!(error.to_string().contains("contract"), "{error}");
}

#[test]
fn indexed_literal_negative_tracks_added_edited_and_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let original = dir.path().join("src/original.js");
    std::fs::write(&original, "const label = 'initial';\n").unwrap();
    let indexed = cli::run(&["update", "--code"], dir.path());
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let search = || {
        let output = cli::run(
            &[
                "--format",
                "json",
                "search",
                "--scope",
                "literals",
                "missing phrase",
            ],
            dir.path(),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let baseline = search();
    assert!(
        baseline["coverage"]["indexed_files"].as_u64().unwrap() > 0,
        "{baseline}"
    );
    assert_eq!(baseline["status"], "complete", "{baseline}");

    std::fs::write(&original, "const label = 'edited';\n").unwrap();
    let edited = search();
    assert_eq!(edited["freshness"]["state"], "stale", "{edited}");
    assert_ne!(edited["status"], "complete", "{edited}");
    std::fs::write(&original, "const label = 'initial';\n").unwrap();

    let added_path = dir.path().join("src/added.js");
    std::fs::write(&added_path, "const label = 'added';\n").unwrap();
    let added = search();
    assert_eq!(added["freshness"]["state"], "stale", "{added}");
    std::fs::remove_file(&added_path).unwrap();

    std::fs::remove_file(&original).unwrap();
    let deleted = search();
    assert_eq!(deleted["freshness"]["state"], "stale", "{deleted}");
}

/// Catches: a failed directory walk being mistaken for an exhaustive freshness
/// check, so an indexed no-match falsely reports complete coverage.
#[cfg(unix)]
#[test]
fn unreadable_source_directory_does_not_prove_literal_absence() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/value.js"),
        "const label = 'old value';\n",
    )
    .unwrap();
    let indexed = cli::run(&["update", "--code"], dir.path());
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let baseline = cli::run(
        &[
            "--format",
            "json",
            "search",
            "--scope",
            "literals",
            "not present",
        ],
        dir.path(),
    );
    let baseline: serde_json::Value = serde_json::from_slice(&baseline.stdout).unwrap();
    assert!(
        baseline["coverage"]["indexed_files"].as_u64().unwrap() > 0,
        "{baseline}"
    );
    assert_eq!(baseline["status"], "complete", "{baseline}");

    let blocked = dir.path().join("src/blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("new.js"), "const label = 'not present';\n").unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(
        std::fs::read_dir(&blocked).is_err(),
        "fixture must deny the directory walk"
    );
    let output = cli::run(
        &[
            "--format",
            "json",
            "search",
            "--scope",
            "literals",
            "not present",
        ],
        dir.path(),
    );
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["hits"], serde_json::json!([]));
    assert_ne!(result["status"], "complete", "unreadable walk: {result}");
    assert_ne!(
        result["freshness"]["state"], "current",
        "unreadable walk: {result}"
    );
}
