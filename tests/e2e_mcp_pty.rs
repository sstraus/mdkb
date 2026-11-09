//! End-to-end tests for MCP server via PTY isolation.
//!
//! These tests spawn the actual `mdkb serve` process and communicate with it
//! via JSON-RPC over stdin/stdout. This tests the full protocol stack:
//! - Process spawning and lifecycle
//! - JSON-RPC serialization/deserialization
//! - MCP protocol handshake (initialize/initialized)
//! - Tool discovery (tools/list)
//! - Tool execution (tools/call)
//! - Error handling and timeouts
//!
//! Unlike the unit tests in server.rs that call methods directly, these tests
//! exercise the actual wire protocol as Claude Code would use it.

#[path = "common/cli.rs"]
mod cli;
mod common;
#[path = "common/fixture.rs"]
mod fixture;

use common::McpTestHarness;
use fixture::McpFixtureSupport;
use serde_json::json;

// =============================================================================
// Protocol Tests
// =============================================================================

/// Test: Server responds to initialize with correct protocol version and capabilities.
#[test]
fn test_mcp_initialize_handshake() {
    let mut harness = McpTestHarness::new();
    let response = harness.initialize();

    // Check response structure
    assert!(response["result"].is_object(), "Should have result object");

    let result = &response["result"];
    assert_eq!(
        result["protocolVersion"], "2024-11-05",
        "Protocol version mismatch"
    );
    assert!(
        result["capabilities"]["tools"].is_object(),
        "Should have tools capability"
    );
    assert_eq!(result["serverInfo"]["name"], "mdkb", "Server name mismatch");
}

/// Test: Server lists all expected tools.
#[test]
fn test_mcp_tools_list() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    let tools = harness.send_request("tools/list", json!({}))["result"]["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    // Expected tools based on server implementation (7 tools)
    let expected_tools = vec![
        "search",
        "get",
        "status",
        "update",
        "memory_write",
        "memory_delete",
        "code_graph",
    ];

    let tool_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();

    for expected in &expected_tools {
        assert!(
            tool_names.contains(expected),
            "Missing tool: {}. Found: {:?}",
            expected,
            tool_names
        );
    }
}

// =============================================================================
// Tool Execution Tests
// =============================================================================

/// Test: status tool returns valid index status.
#[test]
fn test_mcp_tool_status() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    let result = harness.call_tool("status", json!({}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("## Index Status"),
        "Should contain Index Status section"
    );
    assert!(
        text.contains("Documents:"),
        "Should contain Documents count"
    );
    assert!(
        text.contains("## Collections"),
        "Should contain Collections section"
    );
}

/// Test: search tool with documents.
#[test]
fn test_mcp_tool_search() {
    let mut harness = McpTestHarness::new();

    // Create test documents
    harness.create_file(
        "docs/rust-guide.md",
        "# Rust Programming\n\nRust is a systems programming language.",
    );
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    let result = harness.call_tool(
        "search",
        json!({
            "query": "rust programming",
            "limit": 5
        }),
    );
    let text = McpTestHarness::get_text_content(&result);

    // Should find the rust guide
    assert!(
        text.contains("rust-guide.md") || text.contains("Rust"),
        "Should find rust document. Got: {}",
        text
    );
}

/// Catches: global stdio JSON-RPC silently routing canonical memory to the
/// legacy pseudo-result fan-out although direct MCP dispatch matches the CLI.
#[test]
fn canonical_multi_root_memory_json_rpc_matches_cli() {
    let mut harness = McpTestHarness::global();
    let first = mdkb::domain::canonicalize_plain(&harness.root).unwrap();
    let second = mdkb::domain::canonicalize_plain(&harness.root.join("second")).unwrap();
    for (root, id) in [(&first, "first"), (&second, "second")] {
        let result = cli::run(
            &[
                "memory",
                "add",
                id,
                "-t",
                "orbit_lantern decision",
                "-c",
                &format!("orbit_lantern {id} records a separate decision"),
            ],
            root,
        );
        assert!(result.status.success(), "{result:?}");
    }
    harness.initialize();
    for query in ["orbit_lantern", "orbit_lantern decision"] {
        let response = harness.call_tool(
            "search",
            json!({
                "query": query, "scope": "memory", "contract": "v2",
                "root": "*", "limit": 2,
            }),
        );
        assert!(response.get("error").is_none(), "{response}");
        let mcp: serde_json::Value =
            serde_json::from_str(&McpTestHarness::get_text_content(&response)).unwrap();
        let selected = format!("{},{}", first.display(), second.display());
        let cli_output = cli::command()
            .current_dir(&first)
            .env("HOME", first.parent().unwrap().join("home"))
            .env("USERPROFILE", first.parent().unwrap().join("home"))
            .args([
                "--format",
                "json",
                "search",
                query,
                "--scope",
                "memory",
                "--search-contract",
                "v2",
                "--root",
                &selected,
                "--limit",
                "2",
            ])
            .output()
            .unwrap();
        assert!(cli_output.status.success(), "{cli_output:?}");
        let cli: serde_json::Value = serde_json::from_slice(&cli_output.stdout).unwrap();
        assert_eq!(mcp, cli, "stdio MCP and CLI disagree");
        assert_eq!(mcp["hits"].as_array().unwrap().len(), 2, "{mcp}");
        assert_eq!(mcp["coverage"]["roots"].as_array().unwrap().len(), 2);
    }
}

/// Test: get tool retrieves document content.
#[test]
fn test_mcp_tool_get() {
    let mut harness = McpTestHarness::new();

    let content = "# Test Document\n\nThis is test content.";
    harness.create_file("docs/test.md", content);
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // First search to get document ID
    let search_result = harness.call_tool("search", json!({"query": "test document", "limit": 1}));
    let search_text = McpTestHarness::get_text_content(&search_result);

    // Extract ID from search results (format: [ID] path - title ...)
    let id: i64 = search_text
        .lines()
        .find(|l| l.starts_with('['))
        .and_then(|l| l.trim_start_matches('[').split(']').next())
        .and_then(|s| s.parse().ok())
        .expect("Should find document ID in search results");

    // Get the document
    let get_result = harness.call_tool("get", json!({"id": id.to_string()}));
    let get_text = McpTestHarness::get_text_content(&get_result);

    assert!(
        get_text.contains("Test Document"),
        "Should contain document title. Got: {}",
        get_text
    );
    assert!(
        get_text.contains("test content"),
        "Should contain document content. Got: {}",
        get_text
    );
}

/// Test: memory_write and unified get for memory entries.
#[test]
fn test_mcp_memory_tools() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    // Write a memory entry
    let write_result = harness.call_tool(
        "memory_write",
        json!({
            "id": "test-memory-entry",
            "title": "Test Memory Entry",
            "content": "# Test\n\nThis is a test memory entry.",
            "entry_type": "topic",
            "tags": ["test", "memory"]
        }),
    );

    // Check write succeeded
    assert!(
        write_result["result"].is_object(),
        "memory_write should succeed"
    );

    // The write projects to disk on its own, like `mdkb memory add` does. Until
    // it did, the git-tracked projection silently diverged from the index after
    // every MCP write until someone happened to run a sync.
    let projected = harness
        .root
        .join(".mdkb/memory/entries/test-memory-entry.md");
    assert!(
        projected.is_file(),
        "memory_write must project the entry: {}",
        projected.display()
    );

    // Get the memory entry back via unified get (slug resolution)
    let get_result = harness.call_tool("get", json!({"id": "test-memory-entry"}));
    let get_text = McpTestHarness::get_text_content(&get_result);

    assert!(
        get_text.contains("Test Memory Entry"),
        "Should retrieve memory entry via unified get. Got: {}",
        get_text
    );
}

/// Test: status tool includes collection listing with source tags.
#[test]
fn test_mcp_tool_status_includes_collections() {
    let mut harness = McpTestHarness::new();

    harness.create_file("notes/readme.md", "# Notes");
    harness.add_collection("notes", "notes", "**/*.md");
    harness.update_index();

    harness.initialize();

    let result = harness.call_tool("status", json!({}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("notes"),
        "Should list notes collection in status"
    );
    assert!(
        text.contains("[manual]") || text.contains("[convention]"),
        "Should show source tags. Got: {}",
        text
    );
}

/// Test: update tool triggers reindex and new files become searchable.
///
/// Note: we verify the end state (file is searchable) rather than the diff
/// output because the MCP server's startup auto-index may have already
/// picked up files created before the explicit update call.
#[test]
fn test_mcp_tool_update() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/v1.md", "# Version 1");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Add a new file with unique content
    harness.create_file("docs/v2.md", "# Version 2\n\nZebra unicorn content.");

    // Trigger update via MCP
    let result = harness.call_tool("update", json!({}));
    let text = McpTestHarness::get_text_content(&result);

    // Update should complete successfully with document stats
    assert!(
        text.contains("## Documents"),
        "Should contain Documents section. Got: {}",
        text
    );

    // The new file should be searchable after update
    let search_result = harness.call_tool("search", json!({"query": "zebra unicorn", "limit": 5}));
    let search_text = McpTestHarness::get_text_content(&search_result);
    assert!(
        search_text.contains("v2.md") || search_text.contains("Version 2"),
        "New file should be searchable after update. Got: {}",
        search_text
    );
}

// =============================================================================
// Error Handling Tests
// =============================================================================

/// Test: Invalid tool name returns error.
#[test]
fn test_mcp_invalid_tool() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    let result = harness.call_tool("nonexistent_tool", json!({}));

    // Should have error in response
    assert!(
        result["error"].is_object() || result["result"]["isError"] == true,
        "Should return error for invalid tool. Got: {}",
        result
    );
}

/// Test: get tool with nonexistent document returns error.
#[test]
fn test_mcp_get_nonexistent() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    let result = harness.call_tool("get", json!({"id": "99999"}));

    // Error can be in result.isError or in error field (JSON-RPC error)
    let has_error =
        result["error"].is_object() || result["result"]["isError"].as_bool().unwrap_or(false);

    let error_msg = result["error"]["message"].as_str().unwrap_or("");

    assert!(
        has_error && error_msg.to_lowercase().contains("not found"),
        "Should indicate document not found. Got: {}",
        result
    );
}

// =============================================================================
// Concurrency & Stability Tests
// =============================================================================

/// Test: Multiple rapid tool calls don't cause issues.
#[test]
fn test_mcp_rapid_tool_calls() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    // Make 10 rapid status calls
    for i in 0..10 {
        let result = harness.call_tool("status", json!({}));
        assert!(result["result"].is_object(), "Call {} should succeed", i);
    }
}

/// Test: Tool calls work after documents are modified.
#[test]
fn test_mcp_tools_after_file_changes() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/original.md", "# Original");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Verify initial state
    let status1 = harness.call_tool("status", json!({}));
    let text1 = McpTestHarness::get_text_content(&status1);
    assert!(
        text1.contains("Documents: 1"),
        "Should have 1 document initially"
    );

    // Modify files while server is running
    harness.create_file("docs/new.md", "# New Document");

    // Update via MCP
    harness.call_tool("update", json!({}));

    // Verify updated state
    let status2 = harness.call_tool("status", json!({}));
    let text2 = McpTestHarness::get_text_content(&status2);
    assert!(
        text2.contains("Documents: 2"),
        "Should have 2 documents after update"
    );
}

// =============================================================================
// Additional Tool Coverage Tests
// =============================================================================

/// Test: get tool with glob pattern retrieves multiple documents.
#[test]
fn test_mcp_tool_get_glob() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/chapter1.md", "# Chapter 1\n\nIntroduction");
    harness.create_file("docs/chapter2.md", "# Chapter 2\n\nContent");
    harness.create_file("docs/chapter3.md", "# Chapter 3\n\nConclusion");
    harness.create_file("docs/appendix.md", "# Appendix\n\nReferences");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Get all chapters via glob
    let result = harness.call_tool("get", json!({"id": "chapter*.md"}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(text.contains("Chapter 1"), "Should find chapter 1");
    assert!(text.contains("Chapter 2"), "Should find chapter 2");
    assert!(text.contains("Chapter 3"), "Should find chapter 3");
    assert!(!text.contains("Appendix"), "Should not find appendix");
}

/// Test: get tool with line range.
#[test]
fn test_mcp_tool_get_with_lines() {
    let mut harness = McpTestHarness::new();

    let content = (1..=20)
        .map(|i| format!("Line {}", i))
        .collect::<Vec<_>>()
        .join("\n");
    harness.create_file("docs/long.md", &content);
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Search to get document ID
    let search_result = harness.call_tool("search", json!({"query": "Line", "limit": 1}));
    let search_text = McpTestHarness::get_text_content(&search_result);

    let id: i64 = search_text
        .lines()
        .find(|l| l.starts_with('['))
        .and_then(|l| l.trim_start_matches('[').split(']').next())
        .and_then(|s| s.parse().ok())
        .expect("Should find document ID");

    // Get lines 5-10
    let result = harness.call_tool("get", json!({"id": id.to_string(), "lines": "5:10"}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(text.contains("Line 5"), "Should contain line 5");
    assert!(text.contains("Line 10"), "Should contain line 10");
    assert!(!text.contains("Line 1\n"), "Should not contain line 1");
    assert!(!text.contains("Line 15"), "Should not contain line 15");
}

/// Test: search with collection filter.
#[test]
fn test_mcp_tool_search_with_collection() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/rust.md", "# Rust Guide\n\nRust programming");
    harness.create_file("notes/rust.md", "# Rust Notes\n\nRust learning notes");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.add_collection("notes", "notes", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Search only in docs
    let result = harness.call_tool(
        "search",
        json!({"query": "rust", "limit": 10, "collection": "docs"}),
    );
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("rust.md"),
        "Should find docs collection result"
    );
    // Collection filter should return only one result (docs, not notes)
    let result_lines: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).collect();
    assert_eq!(
        result_lines.len(),
        1,
        "Should find exactly one result with collection filter. Got: {:?}",
        result_lines
    );
}

/// Test: search tool with memory scope.
#[test]
fn test_mcp_tool_search_memory_scope() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    // Create several memory entries
    harness.call_tool(
        "memory_write",
        json!({
            "id": "auth-jwt-tokens",
            "title": "JWT Token Authentication",
            "content": "How to implement JWT authentication",
            "entry_type": "topic",
            "tags": ["auth", "jwt"]
        }),
    );

    harness.call_tool(
        "memory_write",
        json!({
            "id": "auth-oauth2",
            "title": "OAuth2 Flow",
            "content": "OAuth2 authorization code flow implementation",
            "entry_type": "topic",
            "tags": ["auth", "oauth"]
        }),
    );

    harness.call_tool(
        "memory_write",
        json!({
            "id": "database-setup",
            "title": "Database Setup",
            "content": "How to set up PostgreSQL",
            "entry_type": "topic",
            "tags": ["database"]
        }),
    );

    // Search for auth topics using scope="memory"
    let result = harness.call_tool(
        "search",
        json!({"query": "authentication", "limit": 10, "scope": "memory"}),
    );
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("JWT") || text.contains("auth"),
        "Should find auth-related entries. Got: {}",
        text
    );
}

/// Test: get tool retrieves document by ID (from search).
///
/// Note: get-by-path has a known issue in PTY tests where the MCP server process
/// doesn't see DB updates made by CLI commands. Search works because it uses FTS5
/// which has different cache behavior. For now, we test get-by-ID which is reliable.
#[test]
fn test_mcp_tool_get_by_id() {
    let mut harness = McpTestHarness::new();

    harness.create_file(
        "docs/specific.md",
        "# Specific Document\n\nUnique content here",
    );
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Search to get document ID
    let search_result =
        harness.call_tool("search", json!({"query": "Specific Document", "limit": 1}));
    let search_text = McpTestHarness::get_text_content(&search_result);

    // Extract ID from search results
    let id: i64 = search_text
        .lines()
        .find(|l| l.starts_with('['))
        .and_then(|l| l.trim_start_matches('[').split(']').next())
        .and_then(|s| s.parse().ok())
        .expect("Should find document ID in search results");

    // Get by ID
    let result = harness.call_tool("get", json!({"id": id.to_string()}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("Specific Document") || text.contains("Unique content"),
        "Should find document by ID. Got: {}",
        text
    );
}

/// Test: memory_write updates existing entry.
#[test]
fn test_mcp_memory_update() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    // Create initial entry
    harness.call_tool(
        "memory_write",
        json!({
            "id": "update-test",
            "title": "Original Title",
            "content": "Original content",
            "entry_type": "topic",
            "tags": ["v1"]
        }),
    );

    // Verify original via unified get
    let get1 = harness.call_tool("get", json!({"id": "update-test"}));
    let text1 = McpTestHarness::get_text_content(&get1);
    assert!(text1.contains("Original"), "Should have original content");

    // Update entry
    harness.call_tool(
        "memory_write",
        json!({
            "id": "update-test",
            "title": "Updated Title",
            "content": "Updated content with new information",
            "entry_type": "topic",
            "tags": ["v2"]
        }),
    );

    // Verify updated via unified get
    let get2 = harness.call_tool("get", json!({"id": "update-test"}));
    let text2 = McpTestHarness::get_text_content(&get2);
    assert!(
        text2.contains("Updated"),
        "Should have updated content. Got: {}",
        text2
    );
}

/// Test: search handles special characters (FTS5 escape).
#[test]
fn test_mcp_search_special_characters() {
    let mut harness = McpTestHarness::new();

    harness.create_file(
        "docs/security.md",
        "# Security Guide\n\nImplement anti-CSRF tokens and XSS protection.",
    );
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Search with hyphenated term (was causing FTS5 parse error)
    let result = harness.call_tool("search", json!({"query": "anti-CSRF tokens", "limit": 10}));

    // Should not error
    assert!(
        result["error"].is_null(),
        "Should not error on hyphenated search. Got: {}",
        result
    );

    let text = McpTestHarness::get_text_content(&result);
    assert!(
        text.contains("security") || text.contains("CSRF"),
        "Should find security document. Got: {}",
        text
    );
}

// =============================================================================
// Namespace Tests
// =============================================================================

/// A namespaced MCP session (`MDKB_NAMESPACE`, or a test runner's marker in the
/// environment) writes to `.mdkb/namespaces/<name>/` and leaves the default
/// store alone — index and projection both. The second half is the one that
/// proves isolation: a consumer test that drives the MCP server must not be
/// able to leave a row or a file the default store's sessions would warm up
/// from.
///
/// The projection is written by the write itself, exactly as the CLI path does
/// it: no `memory sync` is run in between.
#[test]
fn test_mcp_namespaced_session_projects_under_the_namespace_only() {
    let mut harness = McpTestHarness::with_env(&[("MDKB_NAMESPACE", "test")]);
    harness.initialize();

    let write_result = harness.call_tool(
        "memory_write",
        json!({
            "id": "namespaced-mcp-entry",
            "title": "Namespaced MCP entry",
            "content": "# Scratch\n\nWritten by a namespaced MCP session.",
            "entry_type": "topic",
            "tags": ["test"]
        }),
    );
    assert!(
        write_result["result"].is_object(),
        "memory_write should succeed: {write_result}"
    );

    let cli = |args: &[&str], env: &[(&str, &str)]| {
        let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"));
        cmd.args(args)
            .current_dir(&harness.root)
            .env("MDKB_NO_DAEMON", "1");
        for (key, val) in env {
            cmd.env(key, val);
        }
        cmd.output().expect("spawn mdkb")
    };

    // Half one: the write is in the namespaced store and projects there.
    let namespaced = harness
        .root
        .join(".mdkb/namespaces/test/memory/entries/namespaced-mcp-entry.md");
    assert!(
        namespaced.is_file(),
        "projection must land in the namespaced store: {}",
        namespaced.display()
    );

    // Half two: the default store — created by `init`, still present — holds
    // neither the row nor the file.
    let default_file = harness
        .root
        .join(".mdkb/memory/entries/namespaced-mcp-entry.md");
    assert!(
        !default_file.exists(),
        "the default store's projection must stay untouched: {}",
        default_file.display()
    );
    assert!(
        harness.root.join(".mdkb/index.sqlite").is_file(),
        "the default store still exists; the write simply never reached it"
    );
    let out = cli(&["memory", "list", "--format", "json"], &[]);
    assert!(out.status.success(), "default memory list must succeed");
    let listed = String::from_utf8_lossy(&out.stdout);
    assert!(
        !listed.contains("namespaced-mcp-entry"),
        "the default index must not hold the namespaced row: {listed}"
    );
}

/// `memory_delete` retires the projection the way `mdkb memory rm` does: the
/// file moves to `memory/archive/`. Left in `entries/`, the next reconciliation
/// would find a file with no row and import it — the deleted entry comes back.
#[test]
fn test_mcp_memory_delete_archives_the_projection() {
    let mut harness = McpTestHarness::new();
    harness.initialize();
    let write = harness.call_tool(
        "memory_write",
        json!({
            "id": "to-be-deleted",
            "title": "To be deleted",
            "content": "# Gone\n\nSoon.",
            "entry_type": "topic"
        }),
    );
    assert!(
        write["result"].is_object(),
        "memory_write should succeed: {write}"
    );
    let entry = harness.root.join(".mdkb/memory/entries/to-be-deleted.md");
    assert!(
        entry.is_file(),
        "precondition: the write projected the entry"
    );

    let deleted = harness.call_tool("memory_delete", json!({"id": "to-be-deleted"}));
    assert!(
        deleted["result"].is_object(),
        "memory_delete should succeed: {deleted}"
    );
    assert!(
        !entry.exists(),
        "a deleted entry must leave `entries/`, or the next sync re-imports it"
    );
    assert!(
        harness
            .root
            .join(".mdkb/memory/archive/to-be-deleted.md")
            .is_file(),
        "the projection is archived, not destroyed"
    );
}

/// Catches: canonical scopes accepted by tools/list dispatching to a different
/// corpus or root set than the real CLI, beyond the memory-only wire fixture.
#[test]
fn canonical_source_and_indexed_scopes_match_cli_on_the_wire() {
    let mut harness = McpTestHarness::global();
    let first = mdkb::domain::canonicalize_plain(&harness.root).unwrap();
    let second = mdkb::domain::canonicalize_plain(&first.join("second")).unwrap();
    for root in [&first, &second] {
        std::fs::create_dir(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs/guide.md"),
            "# orbit_lantern\n\nDistinct indexed document.\n",
        )
        .unwrap();
        std::fs::write(root.join("labels.js"), "const label = 'orbit_lantern';\n").unwrap();
        for args in [vec!["collection", "add", "docs", "docs"], vec!["update"]] {
            let result = cli::run(&args, root);
            assert!(result.status.success(), "{result:?}");
        }
    }
    harness.initialize();
    let tools = harness.send_request("tools/list", json!({}));
    assert!(
        tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "search")
    );
    for scope in ["source", "literals", "docs", "knowledge"] {
        let single = scope == "source";
        let mcp_root = if single { first.to_str().unwrap() } else { "*" };
        let mut params =
            json!({"query": "orbit_lantern", "scope": scope, "root": mcp_root, "limit": 10});
        if scope == "docs" {
            params["contract"] = json!("v2");
        }
        let response = harness.call_tool("search", params);
        assert!(response.get("error").is_none(), "{scope}: {response}");
        let mcp: serde_json::Value =
            serde_json::from_str(&McpTestHarness::get_text_content(&response)).unwrap();
        let selected = if single {
            first.display().to_string()
        } else {
            format!("{},{}", first.display(), second.display())
        };
        let mut command = cli::command();
        command
            .current_dir(&first)
            .env("HOME", first.parent().unwrap().join("home"))
            .env("USERPROFILE", first.parent().unwrap().join("home"))
            .args([
                "--format",
                "json",
                "search",
                "orbit_lantern",
                "--scope",
                scope,
                "--root",
                &selected,
                "--limit",
                "10",
            ]);
        if scope == "docs" {
            command.args(["--search-contract", "v2"]);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{scope}: {output:?}");
        let cli: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(mcp, cli, "{scope}: stdio MCP and CLI disagree");
        assert!(
            !mcp["hits"].as_array().unwrap().is_empty(),
            "{scope}: {mcp}"
        );
        if !single {
            assert_eq!(mcp["coverage"]["roots"].as_array().unwrap().len(), 2);
        }
    }
}
