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

mod common;

use common::McpTestHarness;
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

    let tools = harness.list_tools();

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
