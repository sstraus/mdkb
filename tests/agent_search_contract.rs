//! Black-box comparison of the explicit live-source contract on CLI and MCP.
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
            "--format", "json", "search", "--scope", "source", "-e", "alpha", "-e", "beta",
            "--limit", "1",
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

/// Catches: a failed directory walk being mistaken for an exhaustive freshness
/// check, so an indexed no-match falsely reports complete coverage.
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
    let indexed = cli::run(&["code", "index", "src"], dir.path());
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let output = cli::run(
        &[
            "--format", "json", "search", "--scope", "literals", "-e", "alpha", "-e", "beta",
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

/// Catches: a v2 opt-in that is silently ignored, or a legacy JSON search
/// whose array shape changes merely because its scope is also canonical.
#[test]
fn overlapping_scopes_require_opt_in_for_the_coverage_envelope() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/value.rs"), "fn lookup() {}\n").unwrap();
    let indexed = cli::run(&["code", "index", "src"], dir.path());
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );

    for scope in ["docs", "memory", "symbols"] {
        let legacy = cli::run(
            &[
                "--format",
                "json",
                "search",
                "no-match-needle",
                "--scope",
                scope,
            ],
            dir.path(),
        );
        assert!(
            legacy.status.success(),
            "{scope}: {}",
            String::from_utf8_lossy(&legacy.stderr)
        );
        let legacy: serde_json::Value = serde_json::from_slice(&legacy.stdout).unwrap();
        assert!(
            legacy.is_array(),
            "legacy {scope} must remain an array: {legacy}"
        );

        let v2 = cli::run(
            &[
                "--format",
                "json",
                "search",
                "no-match-needle",
                "--scope",
                scope,
                "--search-contract",
                "v2",
            ],
            dir.path(),
        );
        assert!(
            v2.status.success(),
            "{scope}: {}",
            String::from_utf8_lossy(&v2.stderr)
        );
        let v2: serde_json::Value = serde_json::from_slice(&v2.stdout).unwrap();
        assert_eq!(v2["scope"], scope, "{v2}");
        assert!(v2["hits"].is_array(), "{v2}");
        assert!(v2["coverage"].is_object(), "{v2}");
        assert_ne!(
            v2["status"], "complete",
            "index freshness is unverified: {v2}"
        );
    }
}

/// Catches: MCP silently ignoring the v2 selector while the CLI emits its
/// opt-in envelope, or changing legacy human-readable MCP output to JSON.
#[tokio::test]
async fn mcp_v2_overlapping_scopes_match_cli_empty_corpus_envelopes() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    let server = McpServer::new(dir.path().to_path_buf());
    for scope in ["docs", "memory", "symbols"] {
        let cli = cli::run(
            &[
                "--format",
                "json",
                "search",
                "unmatched-needle",
                "--scope",
                scope,
                "--search-contract",
                "v2",
            ],
            dir.path(),
        );
        assert!(
            cli.status.success(),
            "{}",
            String::from_utf8_lossy(&cli.stderr)
        );
        let cli: serde_json::Value = serde_json::from_slice(&cli.stdout).unwrap();
        let request = SearchParams {
            query: "unmatched-needle".into(),
            patterns: None,
            path: None,
            regex: false,
            ignore_case: false,
            contract: Some("v2".into()),
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: Some(scope.into()),
            kind: None,
            threshold: None,
            file: None,
            min_confidence: None,
            since: None,
        };
        let v2 = server.search(Parameters(request.clone())).await.unwrap();
        let v2: serde_json::Value =
            serde_json::from_str(&v2.content[0].as_text().unwrap().text).unwrap();
        for field in [
            "scope",
            "unit",
            "status",
            "hits",
            "coverage",
            "freshness",
            "truncated",
        ] {
            assert_eq!(
                cli[field], v2[field],
                "{scope}.{field}: CLI {cli}, MCP {v2}"
            );
        }
        let mut legacy = request;
        legacy.contract = None;
        let legacy = server.search(Parameters(legacy)).await.unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&legacy.content[0].as_text().unwrap().text)
                .is_err(),
            "legacy {scope} MCP response must remain human-readable"
        );
    }
}

/// Catches: CLI/MCP v2 parity appearing correct only for empty indexes while
/// populated document and memory hits use different serialization paths.
#[tokio::test]
async fn populated_v2_docs_and_memory_hits_have_cli_mcp_parity() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("docs")).unwrap();
    std::fs::write(
        dir.path().join("docs/guide.md"),
        "# Orbit Lantern\n\nOrbit Lantern is a project decision.\n",
    )
    .unwrap();
    let update = cli::run(&["update"], dir.path());
    assert!(
        update.status.success(),
        "{}",
        String::from_utf8_lossy(&update.stderr)
    );
    let added = cli::run(
        &[
            "memory",
            "add",
            "orbit-lantern",
            "-t",
            "Orbit Lantern",
            "-c",
            "Orbit Lantern is remembered",
        ],
        dir.path(),
    );
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    let server = McpServer::new(dir.path().to_path_buf());
    for scope in ["docs", "memory"] {
        let output = cli::run(
            &[
                "--format",
                "json",
                "search",
                "Orbit Lantern",
                "--scope",
                scope,
                "--search-contract",
                "v2",
            ],
            dir.path(),
        );
        assert!(
            output.status.success(),
            "{scope}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            !cli["hits"].as_array().unwrap().is_empty(),
            "{scope}: {cli}"
        );
        let mcp = server
            .search(Parameters(SearchParams {
                query: "Orbit Lantern".into(),
                patterns: None,
                path: None,
                regex: false,
                ignore_case: false,
                contract: Some("v2".into()),
                root: None,
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some(scope.into()),
                kind: None,
                threshold: None,
                file: None,
                min_confidence: None,
                since: None,
            }))
            .await
            .unwrap();
        let mcp: serde_json::Value =
            serde_json::from_str(&mcp.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(cli, mcp, "{scope}: CLI {cli}, MCP {mcp}");
    }
}

/// Catches: CLI rejecting canonical multi-root JSON, or applying its legacy
/// per-repository result limit instead of one global literal limit.
#[test]
fn cli_multi_root_literals_have_one_json_response_and_global_limit() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let mut roots = Vec::new();
    for name in ["first", "second"] {
        let root = dir.path().join(name);
        std::fs::create_dir(&root).unwrap();
        mdkb::cli::handlers::handle_init(&root).unwrap();
        std::fs::write(root.join("labels.js"), "const label = 'orbitLantern';\n").unwrap();
        let indexed = cli::run(&["code", "index", "labels.js"], &root);
        assert!(
            indexed.status.success(),
            "{}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        roots.push(mdkb::domain::canonicalize_plain(&root).unwrap());
    }
    std::fs::write(
        home.join(".mdkb/repos.json"),
        serde_json::json!({
            "version": 1,
            "repos": roots.iter().map(|root| serde_json::json!({"root": root})).collect::<Vec<_>>(),
        })
        .to_string(),
    )
    .unwrap();
    let selected = format!("{},{}", roots[0].display(), roots[1].display());
    let output = cli::command()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&roots[0])
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
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("one JSON object");
    assert_eq!(value["scope"], "literals", "{value}");
    assert_eq!(value["hits"].as_array().unwrap().len(), 1, "{value}");
    assert_eq!(value["truncated"], true, "{value}");
    assert_eq!(
        value["coverage"]["roots"].as_array().unwrap().len(),
        2,
        "{value}"
    );
    let legacy = cli::command()
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .current_dir(&roots[0])
        .args([
            "--format",
            "json",
            "search",
            "orbitLantern",
            "--scope",
            "text",
            "--root",
            &selected,
        ])
        .output()
        .unwrap();
    assert!(
        !legacy.status.success(),
        "legacy multi-root text JSON must still refuse"
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

/// Catches: CLI/MCP symbol adapters disagreeing on populated v2 hits or
/// changing the existing zero-based symbol location on only one transport.
#[tokio::test]
async fn populated_v2_symbol_hit_has_cli_mcp_parity() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::write(dir.path().join("symbols.js"), "function lookupValue() {}\n").unwrap();
    let indexed = cli::run(&["code", "index", "symbols.js"], dir.path());
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let output = cli::run(
        &[
            "--format",
            "json",
            "search",
            "lookupValue",
            "--scope",
            "symbols",
            "--search-contract",
            "v2",
            "--limit",
            "1",
        ],
        dir.path(),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(cli["hits"].as_array().unwrap().len(), 1, "{cli}");
    let mcp = McpServer::new(dir.path().to_path_buf())
        .search(Parameters(SearchParams {
            query: "lookupValue".into(),
            patterns: None,
            path: None,
            regex: false,
            ignore_case: false,
            contract: Some("v2".into()),
            root: None,
            limit: 1,
            collection: None,
            include_superseded: false,
            scope: Some("symbols".into()),
            kind: None,
            threshold: None,
            file: None,
            min_confidence: None,
            since: None,
        }))
        .await
        .unwrap();
    let mcp: serde_json::Value =
        serde_json::from_str(&mcp.content[0].as_text().unwrap().text).unwrap();
    assert_eq!(cli, mcp);
    assert_eq!(cli["hits"][0]["range"]["start_line"], 0, "{cli}");
}

/// Catches: an exact-limit v2 response flagged as truncated although no
/// additional indexed symbol exists; a second match must flip the flag.
#[test]
fn v2_symbol_limit_reports_actual_truncation() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::write(dir.path().join("symbols.js"), "function findOne() {}\n").unwrap();
    let indexed = cli::run(&["code", "index", "symbols.js"], dir.path());
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
                "find",
                "--scope",
                "symbols",
                "--search-contract",
                "v2",
                "--limit",
                "1",
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
    let one = search();
    assert_eq!(one["hits"].as_array().unwrap().len(), 1, "{one}");
    assert_eq!(one["truncated"], false, "{one}");

    std::fs::write(
        dir.path().join("symbols.js"),
        "function findOne() {}\nfunction findTwo() {}\n",
    )
    .unwrap();
    let indexed = cli::run(&["code", "index", "symbols.js", "--force"], dir.path());
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let two = search();
    assert_eq!(two["hits"].as_array().unwrap().len(), 1, "{two}");
    assert_eq!(two["truncated"], true, "{two}");
}

#[test]
fn indexed_literal_negative_tracks_added_edited_and_deleted_files() {
    let dir = tempfile::tempdir().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    let original = dir.path().join("src/original.js");
    std::fs::write(&original, "const label = 'initial';\n").unwrap();
    let indexed = cli::run(&["code", "index", "src"], dir.path());
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
    let indexed = cli::run(&["code", "index", "src"], dir.path());
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
