//! PTY tests for search result format and server instruction behaviour.
//!
//! Verifies that search results guide the model toward `get()` usage and that
//! server instructions contain the correct workflow hints. These tests exercise
//! the real MCP wire protocol to catch format regressions that affect model
//! behaviour (e.g. models bypassing `get()` in favour of `Read`).
//!
//! ## A/B Testing Instructions Variants
//!
//! Set `MDKB_INSTRUCTIONS_VARIANT` to swap the base instructions:
//!
//! ```bash
//! MDKB_INSTRUCTIONS_VARIANT=v2 cargo test --test e2e_search_behaviour
//! ```
//!
//! Current variants: `default` (only). Add new variants in `server.rs`
//! and test them here to measure impact before committing.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test harness (reusable MCP server spawner)
// ---------------------------------------------------------------------------

struct SearchTestHarness {
    child: Child,
    _temp_dir: TempDir,
    root: PathBuf,
    next_id: u64,
}

impl SearchTestHarness {
    fn new() -> Self {
        Self::with_env(&[])
    }

    fn with_env(env_vars: &[(&str, &str)]) -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();

        // Initialize mdkb
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .arg("init")
            .current_dir(&root)
            .status()
            .expect("Failed to run mdkb init");
        assert!(status.success(), "mdkb init failed");

        // Spawn mdkb serve with optional env vars
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkb"));
        cmd.arg("serve")
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, val) in env_vars {
            cmd.env(key, val);
        }
        let child = cmd.spawn().expect("Failed to spawn mdkb serve");

        Self {
            child,
            _temp_dir: temp_dir,
            root,
            next_id: 1,
        }
    }

    fn create_file(&self, relative_path: &str, content: &str) {
        let full_path = self.root.join(relative_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).expect("Failed to create parent dirs");
        }
        fs::write(&full_path, content).expect("Failed to write file");
    }

    fn add_collection(&self, name: &str, path: &str, pattern: &str) {
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .args(["collection", "add", name, path, "--pattern", pattern])
            .current_dir(&self.root)
            .status()
            .expect("Failed to run mdkb collection add");
        assert!(status.success(), "mdkb collection add failed");
    }

    fn update_index(&self) {
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .arg("update")
            .current_dir(&self.root)
            .status()
            .expect("Failed to run mdkb update");
        assert!(status.success(), "mdkb update failed");
    }

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

        let stdout = self.child.stdout.as_mut().expect("No stdout");
        let mut reader = BufReader::new(stdout);
        let mut response_line = String::new();

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
            .unwrap_or_else(|e| panic!("Invalid JSON: {} - {}", e, response_line));
        assert_eq!(response["id"], id, "Response ID mismatch");
        response
    }

    fn send_notification(&mut self, method: &str, params: Value) {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let stdin = self.child.stdin.as_mut().expect("No stdin");
        let s = serde_json::to_string(&notification).unwrap();
        writeln!(stdin, "{}", s).expect("Failed to write notification");
        stdin.flush().expect("Failed to flush");
    }

    fn initialize(&mut self) -> Value {
        let response = self.send_request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "search-test", "version": "1.0.0" }
            }),
        );
        self.send_notification("notifications/initialized", json!({}));
        response
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.send_request("tools/call", json!({ "name": name, "arguments": arguments }))
    }

    fn get_text_content(result: &Value) -> String {
        result["result"]["content"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|c| c["text"].as_str())
            .unwrap_or("")
            .to_string()
    }
}

impl Drop for SearchTestHarness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// =============================================================================
// Server Instructions Tests
// =============================================================================

/// Server instructions must tell the model to start with `search(query)`.
#[test]
fn test_instructions_contain_search_first_workflow() {
    let mut harness = SearchTestHarness::new();
    let response = harness.initialize();

    let instructions = response["result"]["instructions"]
        .as_str()
        .unwrap_or("");

    assert!(
        instructions.contains("search(query)"),
        "Instructions must mention search(query). Got: {}",
        &instructions[..instructions.len().min(200)]
    );
    assert!(
        instructions.contains("ALWAYS start here"),
        "Instructions must tell model to ALWAYS start with search. Got: {}",
        &instructions[..instructions.len().min(200)]
    );
}

/// Instructions must warn that scopes are filters, not starting points.
#[test]
fn test_instructions_scope_guidance() {
    let mut harness = SearchTestHarness::new();
    let response = harness.initialize();

    let instructions = response["result"]["instructions"]
        .as_str()
        .unwrap_or("");

    assert!(
        instructions.contains("filters, not starting points"),
        "Instructions must explain scopes are filters. Got: {}",
        &instructions[..instructions.len().min(300)]
    );
}

/// Instructions must include memory_write in the workflow.
#[test]
fn test_instructions_contain_memory_write_hint() {
    let mut harness = SearchTestHarness::new();
    let response = harness.initialize();

    let instructions = response["result"]["instructions"]
        .as_str()
        .unwrap_or("");

    assert!(
        instructions.contains("memory_write"),
        "Instructions must mention memory_write for persisting knowledge. Got: {}",
        &instructions[..instructions.len().min(300)]
    );
}

// =============================================================================
// Search Result Format Tests
// =============================================================================

/// Search results must include a `get()` hint with document IDs.
#[test]
fn test_search_results_include_get_hint() {
    let mut harness = SearchTestHarness::new();

    harness.create_file("docs/guide.md", "# User Guide\n\nHow to use the system.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "user guide", "limit": 5}));
    let text = SearchTestHarness::get_text_content(&result);

    assert!(
        text.contains("Use get("),
        "Search results must include get() hint. Got: {}",
        text
    );
}

/// Search results must NOT have collection prefix (e.g. `docs:path`).
#[test]
fn test_search_results_no_collection_prefix() {
    let mut harness = SearchTestHarness::new();

    harness.create_file("docs/api.md", "# API Reference\n\nEndpoints documentation.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "API", "limit": 5}));
    let text = SearchTestHarness::get_text_content(&result);

    // Result lines start with [id] — none should have "docs:" prefix
    for line in text.lines().filter(|l| l.starts_with('[')) {
        assert!(
            !line.contains("docs:"),
            "Result line must not have collection prefix. Got: {}",
            line
        );
    }
}

/// Search results format: `[ID] path - title (score: X.XX)`.
#[test]
fn test_search_results_format() {
    let mut harness = SearchTestHarness::new();

    harness.create_file("docs/setup.md", "# Setup Guide\n\nInstallation steps.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "setup installation", "limit": 5}));
    let text = SearchTestHarness::get_text_content(&result);

    let result_lines: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).collect();
    assert!(!result_lines.is_empty(), "Should have at least one result");

    for line in &result_lines {
        // Verify format: [ID] path - title (score: X.XX)
        assert!(
            line.contains("] ") && line.contains(" - ") && line.contains("(score:"),
            "Result line must match format [ID] path - title (score: X.XX). Got: {}",
            line
        );
    }
}

/// Multiple results should show both single-get and all-get hints.
#[test]
fn test_search_results_multiple_get_hint() {
    let mut harness = SearchTestHarness::new();

    harness.create_file("docs/intro.md", "# Rust Introduction\n\nGetting started with Rust.");
    harness.create_file("docs/advanced.md", "# Advanced Rust\n\nAdvanced Rust patterns.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "rust", "limit": 10}));
    let text = SearchTestHarness::get_text_content(&result);

    let result_lines: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).collect();

    if result_lines.len() >= 2 {
        // Multiple results: should show "for all" hint with comma-separated IDs
        assert!(
            text.contains("for all"),
            "Multiple results should include 'for all' get hint. Got: {}",
            text
        );
    }
}

/// Search with no results returns appropriate message.
#[test]
fn test_search_no_results_message() {
    let mut harness = SearchTestHarness::new();
    harness.initialize();

    let result = harness.call_tool(
        "search",
        json!({"query": "xyzzy_nonexistent_term_12345", "limit": 5}),
    );
    let text = SearchTestHarness::get_text_content(&result);

    assert!(
        text.to_lowercase().contains("no results") || text.to_lowercase().contains("no matching") || text.to_lowercase().contains("not found"),
        "Empty search should indicate no results. Got: {}",
        text
    );
}

// =============================================================================
// Search → Get Workflow Tests
// =============================================================================

/// Full workflow: search returns IDs, get() retrieves content by those IDs.
#[test]
fn test_search_then_get_workflow() {
    let mut harness = SearchTestHarness::new();

    harness.create_file(
        "docs/workflow.md",
        "# Workflow Guide\n\nStep-by-step workflow documentation.",
    );
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    // Step 1: search
    let search_result = harness.call_tool("search", json!({"query": "workflow", "limit": 5}));
    let search_text = SearchTestHarness::get_text_content(&search_result);

    // Extract ID from get() hint
    let id = search_text
        .lines()
        .find(|l| l.starts_with('['))
        .and_then(|l| l.trim_start_matches('[').split(']').next())
        .and_then(|s| s.parse::<i64>().ok())
        .expect("Should find document ID in search results");

    // Step 2: get by ID (as the hint suggests)
    let get_result = harness.call_tool("get", json!({"id": id.to_string()}));
    let get_text = SearchTestHarness::get_text_content(&get_result);

    assert!(
        get_text.contains("Workflow Guide"),
        "get() should retrieve the document found by search. Got: {}",
        get_text
    );
}

/// Memory search → get workflow: search with memory scope, then get by slug.
#[test]
fn test_memory_search_then_get_workflow() {
    let mut harness = SearchTestHarness::new();
    harness.initialize();

    // Create memory entry
    harness.call_tool(
        "memory_write",
        json!({
            "id": "deploy-checklist",
            "title": "Deployment Checklist",
            "content": "# Deploy\n\n1. Run tests\n2. Build\n3. Deploy to staging",
            "entry_type": "topic",
            "tags": ["deploy", "ops"]
        }),
    );

    // Search for it
    let search_result = harness.call_tool(
        "search",
        json!({"query": "deployment checklist", "limit": 5}),
    );
    let search_text = SearchTestHarness::get_text_content(&search_result);

    assert!(
        search_text.contains("deploy") || search_text.contains("Deployment"),
        "Should find memory entry via search. Got: {}",
        search_text
    );

    // Get by slug
    let get_result = harness.call_tool("get", json!({"id": "deploy-checklist"}));
    let get_text = SearchTestHarness::get_text_content(&get_result);

    assert!(
        get_text.contains("Deployment Checklist") || get_text.contains("Run tests"),
        "get() should retrieve memory entry by slug. Got: {}",
        get_text
    );
}

// =============================================================================
// Instructions Variant (A/B) Tests
// =============================================================================

/// Default instructions variant loads successfully.
#[test]
fn test_instructions_variant_default() {
    let mut harness = SearchTestHarness::with_env(&[("MDKB_INSTRUCTIONS_VARIANT", "default")]);
    let response = harness.initialize();

    let instructions = response["result"]["instructions"]
        .as_str()
        .unwrap_or("");

    assert!(
        !instructions.is_empty(),
        "Default variant should produce non-empty instructions"
    );
    assert!(
        instructions.contains("search(query)"),
        "Default variant must contain search(query)"
    );
}

/// Unknown variant falls back to default without error.
#[test]
fn test_instructions_variant_unknown_falls_back() {
    let mut harness =
        SearchTestHarness::with_env(&[("MDKB_INSTRUCTIONS_VARIANT", "nonexistent_v99")]);
    let response = harness.initialize();

    let instructions = response["result"]["instructions"]
        .as_str()
        .unwrap_or("");

    // Should still get valid instructions (fallback to default)
    assert!(
        instructions.contains("search(query)"),
        "Unknown variant should fall back to default. Got: {}",
        &instructions[..instructions.len().min(200)]
    );
}
