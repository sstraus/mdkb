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

mod common;

use common::McpTestHarness;
use serde_json::json;

// =============================================================================
// Server Instructions Tests
// =============================================================================

/// Server instructions must contain all required workflow guidance.
#[test]
fn test_instructions_content() {
    let mut harness = McpTestHarness::new();
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
    assert!(
        instructions.contains("filters, not starting points"),
        "Instructions must explain scopes are filters. Got: {}",
        &instructions[..instructions.len().min(300)]
    );
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
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/guide.md", "# User Guide\n\nHow to use the system.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "user guide", "limit": 5}));
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("Use get("),
        "Search results must include get() hint. Got: {}",
        text
    );
}

/// Search results must NOT have collection prefix (e.g. `docs:path`).
#[test]
fn test_search_results_no_collection_prefix() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/api.md", "# API Reference\n\nEndpoints documentation.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "API", "limit": 5}));
    let text = McpTestHarness::get_text_content(&result);

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
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/setup.md", "# Setup Guide\n\nInstallation steps.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "setup installation", "limit": 5}));
    let text = McpTestHarness::get_text_content(&result);

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

/// Multiple results must show both single-get and all-get hints.
#[test]
fn test_search_results_multiple_get_hint() {
    let mut harness = McpTestHarness::new();

    harness.create_file("docs/intro.md", "# Rust Introduction\n\nGetting started with Rust programming language basics.");
    harness.create_file("docs/advanced.md", "# Rust Advanced\n\nAdvanced Rust programming patterns and techniques.");
    harness.create_file("docs/tutorial.md", "# Rust Tutorial\n\nA hands-on Rust programming tutorial for beginners.");
    harness.create_file("docs/unrelated.md", "# Python Guide\n\nPython is a dynamic language.");
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    let result = harness.call_tool("search", json!({"query": "rust programming", "limit": 10}));
    let text = McpTestHarness::get_text_content(&result);

    let result_lines: Vec<&str> = text.lines().filter(|l| l.starts_with('[')).collect();
    assert!(
        result_lines.len() >= 2,
        "Expected at least 2 results for query 'rust'. Got: {}",
        text
    );
    assert!(
        text.contains("for all"),
        "Multiple results should include 'for all' get hint. Got: {}",
        text
    );
}

/// Search with no results returns appropriate message.
#[test]
fn test_search_no_results_message() {
    let mut harness = McpTestHarness::new();
    harness.initialize();

    let result = harness.call_tool(
        "search",
        json!({"query": "xyzzy_nonexistent_term_12345", "limit": 5}),
    );
    let text = McpTestHarness::get_text_content(&result);

    assert!(
        text.contains("No results"),
        "Empty search should return 'No results...' message. Got: {}",
        text
    );
}

// =============================================================================
// Search -> Get Workflow Tests
// =============================================================================

/// Full workflow: search returns IDs, get() retrieves content by those IDs.
#[test]
fn test_search_then_get_workflow() {
    let mut harness = McpTestHarness::new();

    harness.create_file(
        "docs/workflow.md",
        "# Workflow Guide\n\nStep-by-step workflow documentation.",
    );
    harness.add_collection("docs", "docs", "**/*.md");
    harness.update_index();
    harness.initialize();

    // Step 1: search
    let search_result = harness.call_tool("search", json!({"query": "workflow", "limit": 5}));
    let search_text = McpTestHarness::get_text_content(&search_result);

    // Extract ID from get() hint
    let id = search_text
        .lines()
        .find(|l| l.starts_with('['))
        .and_then(|l| l.trim_start_matches('[').split(']').next())
        .and_then(|s| s.parse::<i64>().ok())
        .expect("Should find document ID in search results");

    // Step 2: get by ID (as the hint suggests)
    let get_result = harness.call_tool("get", json!({"id": id.to_string()}));
    let get_text = McpTestHarness::get_text_content(&get_result);

    assert!(
        get_text.contains("Workflow Guide"),
        "get() should retrieve the document found by search. Got: {}",
        get_text
    );
}

/// Memory search -> get workflow: search with memory scope, then get by slug.
#[test]
fn test_memory_search_then_get_workflow() {
    let mut harness = McpTestHarness::new();
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
    let search_text = McpTestHarness::get_text_content(&search_result);

    assert!(
        search_text.contains("deploy") || search_text.contains("Deployment"),
        "Should find memory entry via search. Got: {}",
        search_text
    );

    // Get by slug
    let get_result = harness.call_tool("get", json!({"id": "deploy-checklist"}));
    let get_text = McpTestHarness::get_text_content(&get_result);

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
    let mut harness = McpTestHarness::with_env(&[("MDKB_INSTRUCTIONS_VARIANT", "default")]);
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
        McpTestHarness::with_env(&[("MDKB_INSTRUCTIONS_VARIANT", "nonexistent_v99")]);
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
