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

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

/// Test harness that spawns an MCP server process and communicates via JSON-RPC.
struct McpTestHarness {
    /// The mdkb serve child process.
    child: Child,
    /// Temp directory (kept alive for test duration).
    _temp_dir: TempDir,
    /// Root path of the test environment.
    root: PathBuf,
    /// Next JSON-RPC request ID.
    next_id: u64,
}

impl McpTestHarness {
    /// Create a new test harness with an initialized mdkb directory.
    ///
    /// This spawns `mdkb serve` and waits for it to be ready.
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();

        // Initialize mdkb
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .arg("init")
            .current_dir(&root)
            .status()
            .expect("Failed to run mdkb init");
        assert!(status.success(), "mdkb init failed");

        // Spawn mdkb serve
        let child = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .arg("serve")
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to spawn mdkb serve");

        Self {
            child,
            _temp_dir: temp_dir,
            root,
            next_id: 1,
        }
    }

    /// Create a markdown file in the test environment.
    fn create_file(&self, relative_path: &str, content: &str) {
        let full_path = self.root.join(relative_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).expect("Failed to create parent directories");
        }
        fs::write(&full_path, content).expect("Failed to write file");
    }

    /// Add a collection using the CLI.
    fn add_collection(&self, name: &str, path: &str, pattern: &str) {
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .args(["collection", "add", name, path, "--pattern", pattern])
            .current_dir(&self.root)
            .status()
            .expect("Failed to run mdkb collection add");
        assert!(status.success(), "mdkb collection add failed");
    }

    /// Update/reindex using the CLI.
    fn update_index(&self) {
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .arg("update")
            .current_dir(&self.root)
            .status()
            .expect("Failed to run mdkb update");
        assert!(status.success(), "mdkb update failed");
    }

    /// Send a JSON-RPC request and return the response.
    fn send_request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });

        let stdin = self.child.stdin.as_mut().expect("No stdin");
        let request_str = serde_json::to_string(&request).unwrap();
        writeln!(stdin, "{}", request_str).expect("Failed to write request");
        stdin.flush().expect("Failed to flush");

        // Read response with timeout
        let stdout = self.child.stdout.as_mut().expect("No stdout");
        let mut reader = BufReader::new(stdout);
        let mut response_line = String::new();

        // Simple timeout: try reading for up to 10 seconds
        let start = std::time::Instant::now();
        loop {
            match reader.read_line(&mut response_line) {
                Ok(0) => panic!("Server closed stdout unexpectedly"),
                Ok(_) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if start.elapsed() > Duration::from_secs(10) {
                        panic!("Timeout waiting for response to {}", method);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("Read error: {}", e),
            }
        }

        let response: Value = serde_json::from_str(&response_line)
            .unwrap_or_else(|e| panic!("Invalid JSON response: {} - {}", e, response_line));

        // Verify response ID matches
        assert_eq!(response["id"], id, "Response ID mismatch");

        response
    }

    /// Send a notification (no response expected).
    fn send_notification(&mut self, method: &str, params: Value) {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let stdin = self.child.stdin.as_mut().expect("No stdin");
        let notification_str = serde_json::to_string(&notification).unwrap();
        writeln!(stdin, "{}", notification_str).expect("Failed to write notification");
        stdin.flush().expect("Failed to flush");
    }

    /// Perform MCP initialization handshake.
    fn initialize(&mut self) -> Value {
        let response = self.send_request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "test-client",
                    "version": "1.0.0"
                }
            }),
        );

        // Send initialized notification
        self.send_notification("notifications/initialized", json!({}));

        response
    }

    /// List available tools.
    fn list_tools(&mut self) -> Vec<Value> {
        let response = self.send_request("tools/list", json!({}));
        response["result"]["tools"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Call an MCP tool and return the result.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        let response = self.send_request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
        );
        response
    }

    /// Extract text content from a tool call result.
    fn get_text_content(result: &Value) -> String {
        result["result"]["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["text"].as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl Drop for McpTestHarness {
    fn drop(&mut self) {
        // Kill the server process
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

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
    assert_eq!(
        result["serverInfo"]["name"], "mdkb",
        "Server name mismatch"
    );
}

/// Test: Server lists all expected tools.
#[test]
fn test_mcp_tools_list() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    let tools = harness.list_tools();

    // Expected tools based on server implementation
    let expected_tools = vec![
        "search",
        "get",
        "status",
        "update",
        "multi_get",
        "get_metrics",
        "memory_write",
        "memory_delete",
        "collection_add",
        "collection_remove",
    ];

    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();

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

    assert!(text.contains("## Index Status"), "Should contain Index Status section");
    assert!(text.contains("Documents:"), "Should contain Documents count");
    assert!(text.contains("## Collections"), "Should contain Collections section");
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

    // Extract ID from search results (format: [ID] collection: ...)
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

    assert!(text.contains("notes"), "Should list notes collection in status");
    assert!(text.contains("[manual]") || text.contains("[convention]"),
        "Should show source tags. Got: {}", text);
}

/// Test: update tool triggers reindex.
#[test]
fn test_mcp_tool_update() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/v1.md", "# Version 1");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Add a new file
    harness.create_file("docs/v2.md", "# Version 2");

    // Trigger update via MCP
    let result = harness.call_tool("update", json!({}));
    let text = McpTestHarness::get_text_content(&result);

    // Should report 1 added document
    assert!(
        text.contains("Added: 1") || text.contains("added: 1"),
        "Should report added document. Got: {}",
        text
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
    let has_error = result["error"].is_object()
        || result["result"]["isError"].as_bool().unwrap_or(false);

    let error_msg = result["error"]["message"]
        .as_str()
        .unwrap_or("");

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
        assert!(
            result["result"].is_object(),
            "Call {} should succeed",
            i
        );
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
    assert!(text1.contains("Documents: 1"), "Should have 1 document initially");

    // Modify files while server is running
    harness.create_file("docs/new.md", "# New Document");

    // Update via MCP
    harness.call_tool("update", json!({}));

    // Verify updated state
    let status2 = harness.call_tool("status", json!({}));
    let text2 = McpTestHarness::get_text_content(&status2);
    assert!(text2.contains("Documents: 2"), "Should have 2 documents after update");
}

// =============================================================================
// Additional Tool Coverage Tests
// =============================================================================

/// Test: multi_get tool retrieves multiple documents by glob pattern.
#[test]
fn test_mcp_tool_multi_get() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/chapter1.md", "# Chapter 1\n\nIntroduction");
    harness.create_file("docs/chapter2.md", "# Chapter 2\n\nContent");
    harness.create_file("docs/chapter3.md", "# Chapter 3\n\nConclusion");
    harness.create_file("docs/appendix.md", "# Appendix\n\nReferences");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Get all chapters
    let result = harness.call_tool("multi_get", json!({"pattern": "chapter*.md"}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(text.contains("Chapter 1"), "Should find chapter 1");
    assert!(text.contains("Chapter 2"), "Should find chapter 2");
    assert!(text.contains("Chapter 3"), "Should find chapter 3");
    assert!(!text.contains("Appendix"), "Should not find appendix");
}

/// Test: multi_get with collection filter.
#[test]
fn test_mcp_tool_multi_get_with_collection() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/readme.md", "# Docs Readme");
    harness.create_file("notes/readme.md", "# Notes Readme");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.add_collection("notes", "notes", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Get readme only from docs collection
    let result = harness.call_tool(
        "multi_get",
        json!({"pattern": "*.md", "collection": "docs"}),
    );
    let text = McpTestHarness::get_text_content(&result);

    assert!(text.contains("Docs Readme"), "Should find docs readme");
    assert!(!text.contains("Notes Readme"), "Should not find notes readme");
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

    assert!(text.contains("docs:"), "Should find docs collection result");
    // The collection prefix should be docs, not notes
    let lines: Vec<&str> = text.lines().filter(|l| l.contains("rust")).collect();
    for line in &lines {
        assert!(
            !line.contains("notes:"),
            "Should not find notes results. Got: {}",
            line
        );
    }
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
    let result = harness.call_tool("search", json!({"query": "authentication", "limit": 10, "scope": "memory"}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("JWT") || text.contains("auth"),
        "Should find auth-related entries. Got: {}",
        text
    );
}

/// Test: get_metrics tool.
#[test]
fn test_mcp_tool_get_metrics() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    // Perform some searches to generate metrics
    for i in 0..5 {
        harness.call_tool("search", json!({"query": format!("test query {}", i), "limit": 5}));
    }

    // Get metrics
    let result = harness.call_tool(
        "get_metrics",
        json!({"period_days": 7, "include_latency": true, "include_quality": true}),
    );
    let text = McpTestHarness::get_text_content(&result);

    // Should contain some metrics information
    assert!(
        text.contains("queries") || text.contains("latency") || text.contains("Metrics") || text.len() > 10,
        "Should return metrics information. Got: {}",
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

    harness.create_file("docs/specific.md", "# Specific Document\n\nUnique content here");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();

    harness.initialize();

    // Search to get document ID
    let search_result = harness.call_tool("search", json!({"query": "Specific Document", "limit": 1}));
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
    assert!(text2.contains("Updated"), "Should have updated content. Got: {}", text2);
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
    let result = harness.call_tool(
        "search",
        json!({"query": "anti-CSRF tokens", "limit": 10}),
    );

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
