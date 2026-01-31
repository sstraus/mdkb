//! End-to-end tests simulating LLM interaction with MCP server.
//!
//! These tests simulate realistic workflows where an LLM agent:
//! 1. Initializes a knowledge base
//! 2. Adds collections to index documents
//! 3. Updates/reindexes the documents
//! 4. Searches for information using various search modes
//! 5. Retrieves specific documents
//! 6. Handles edge cases and error conditions
//!
//! The tests use the MCP server's tool implementations directly,
//! simulating the JSON-RPC calls an actual LLM client would make.

use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use mdkb::cli::handlers::{Context, handle_collection_add, handle_init, handle_update};
use mdkb::domain::{SearchQuery, SearchResult};
use mdkb::store::{collections, documents, search};

/// Test environment that simulates a complete mdkb installation.
struct TestEnvironment {
    /// Temporary directory containing the test environment.
    _temp_dir: TempDir,
    /// Root path of the test environment.
    root: PathBuf,
    /// Database context.
    ctx: Context,
}

impl TestEnvironment {
    /// Create a new test environment with an initialized mdkb directory.
    fn new() -> Self {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();

        // Initialize mdkb
        handle_init(&root).expect("Failed to initialize mdkb");
        let ctx = Context::open(&root).expect("Failed to open context");

        Self {
            _temp_dir: temp_dir,
            root,
            ctx,
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

    /// Add a collection and index its documents.
    fn add_and_index_collection(&self, name: &str, path: &str, pattern: &str) {
        handle_collection_add(&self.ctx, name, path, pattern)
            .expect("Failed to add collection");
        handle_update(&self.ctx, &self.root).expect("Failed to update index");
    }
}

/// MCP Client Simulator - represents an LLM agent calling MCP tools.
///
/// This struct wraps the underlying tool implementations to provide
/// a clean interface that matches the MCP tool call semantics.
struct McpClient<'a> {
    env: &'a TestEnvironment,
}

impl<'a> McpClient<'a> {
    fn new(env: &'a TestEnvironment) -> Self {
        Self { env }
    }

    /// Simulate calling mdkb_search tool.
    ///
    /// An LLM would call this to search for documents matching a query.
    fn search(&self, query: &str, limit: usize, collection: Option<&str>) -> Vec<SearchResult> {
        let search_query = SearchQuery {
            text: query.to_string(),
            limit,
            collection: collection.map(String::from),
            tags: vec![],
        };

        search::search(&self.env.ctx.conn, &search_query)
            .expect("Search failed")
    }

    /// Simulate calling mdkb_status tool.
    ///
    /// An LLM would call this to understand the current state of the knowledge base.
    fn status(&self) -> mdkb::domain::IndexStatus {
        search::get_status(&self.env.ctx.conn)
            .expect("Status failed")
    }

    /// Simulate calling mdkb_update tool.
    ///
    /// An LLM would call this to trigger a reindex after file changes.
    fn update(&self) -> mdkb::domain::UpdateResult {
        handle_update(&self.env.ctx, &self.env.root)
            .expect("Update failed")
    }

    /// Simulate calling mdkb_get tool.
    ///
    /// An LLM would call this to retrieve a specific document by ID.
    fn get(&self, doc_id: i64) -> Option<(mdkb::domain::Document, String)> {
        let doc = documents::get_document(&self.env.ctx.conn, doc_id)
            .expect("Get document failed")?;
        let content = documents::get_content(&self.env.ctx.conn, &doc.hash)
            .expect("Get content failed")?;
        Some((doc, content))
    }

    /// Simulate calling mdkb_get tool with line range.
    fn get_lines(&self, doc_id: i64, start: usize, end: usize) -> Option<String> {
        let (_, content) = self.get(doc_id)?;
        let lines: Vec<&str> = content.lines().collect();
        let start_idx = start.saturating_sub(1);
        let end_idx = end.min(lines.len());
        if start_idx >= lines.len() {
            return Some(String::new());
        }
        Some(lines[start_idx..end_idx].join("\n"))
    }

    /// Simulate calling mdkb_multi_get tool.
    ///
    /// An LLM would call this to retrieve multiple documents matching a pattern.
    fn multi_get(&self, pattern: &str, collection: Option<&str>) -> Vec<(mdkb::domain::Document, String)> {
        mdkb::cli::handlers::handle_mget(&self.env.ctx, pattern, collection)
            .expect("Multi-get failed")
    }

    /// List all collections.
    fn list_collections(&self) -> Vec<mdkb::domain::Collection> {
        collections::list_collections(&self.env.ctx.conn)
            .expect("List collections failed")
    }
}

// =============================================================================
// E2E Test Scenarios
// =============================================================================

/// Scenario 1: Basic knowledge base initialization and exploration.
///
/// Simulates an LLM agent that:
/// 1. Checks status of an empty knowledge base
/// 2. Understands there are no documents yet
#[test]
fn scenario_empty_knowledge_base() {
    let env = TestEnvironment::new();
    let client = McpClient::new(&env);

    // LLM checks status first to understand the knowledge base
    let status = client.status();

    assert_eq!(status.collections, 0, "Expected no collections in fresh KB");
    assert_eq!(status.documents, 0, "Expected no documents in fresh KB");
}

/// Scenario 2: Documentation search workflow.
///
/// Simulates an LLM agent that:
/// 1. Has a documentation collection set up
/// 2. Searches for specific topics
/// 3. Retrieves relevant documents
/// 4. Extracts specific line ranges
#[test]
fn scenario_documentation_search() {
    let env = TestEnvironment::new();

    // Create a documentation structure
    env.create_file("docs/getting-started.md", r#"---
title: Getting Started Guide
---

# Getting Started

Welcome to our project! This guide will help you get up and running quickly.

## Installation

To install the project, run:

```bash
cargo install myproject
```

## Quick Start

After installation, you can start using the CLI:

```bash
myproject init
myproject run
```

## Next Steps

- Read the API documentation
- Check out the examples
- Join our community
"#);

    env.create_file("docs/api/endpoints.md", r#"---
title: API Endpoints
---

# API Endpoints

This document describes all available API endpoints.

## Authentication

POST /auth/login - Login to get a token
POST /auth/logout - Invalidate current token

## Resources

GET /api/resources - List all resources
POST /api/resources - Create a new resource
GET /api/resources/:id - Get a specific resource
PUT /api/resources/:id - Update a resource
DELETE /api/resources/:id - Delete a resource
"#);

    env.create_file("docs/troubleshooting.md", r#"---
title: Troubleshooting Guide
---

# Troubleshooting

Common issues and their solutions.

## Installation Issues

### Error: Permission Denied

If you see a permission denied error, try:

```bash
sudo cargo install myproject
```

### Error: Missing Dependencies

Make sure you have the required dependencies installed.

## Runtime Issues

### Error: Connection Refused

Check that the server is running and the port is correct.
"#);

    // Add collection and index
    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // LLM checks status - should now have documents
    let status = client.status();
    assert_eq!(status.collections, 1);
    assert_eq!(status.documents, 3);

    // LLM searches for installation instructions
    let results = client.search("installation", 5, None);
    assert!(!results.is_empty(), "Should find results for 'installation'");

    // The getting-started doc should be in results
    let has_getting_started = results.iter().any(|r| r.path.contains("getting-started"));
    assert!(has_getting_started, "Should find getting-started.md for installation query");

    // LLM retrieves the document
    let first_result = &results[0];
    let (_doc, content) = client.get(first_result.id).expect("Should retrieve document");
    assert!(content.contains("Getting Started") || content.contains("installation"));

    // LLM searches for API information
    let api_results = client.search("API endpoints", 5, None);
    assert!(!api_results.is_empty(), "Should find results for 'API endpoints'");

    // Verify we can get content with line ranges
    if let Some((_, _)) = client.get(api_results[0].id) {
        let lines = client.get_lines(api_results[0].id, 1, 10);
        assert!(lines.is_some(), "Should get line range");
    }
}

/// Scenario 3: Multi-collection knowledge base.
///
/// Simulates an LLM agent working with multiple document collections:
/// 1. Documentation collection
/// 2. Notes collection
/// 3. Searches within specific collections
#[test]
fn scenario_multi_collection() {
    let env = TestEnvironment::new();

    // Create docs collection
    env.create_file("docs/readme.md", "# Project Documentation\n\nThis is the main docs.");
    env.create_file("docs/guide.md", "# User Guide\n\nHow to use the project.");

    // Create notes collection
    env.create_file("notes/ideas.md", "# Ideas\n\nSome project ideas to explore.");
    env.create_file("notes/meeting.md", "# Meeting Notes\n\nDiscussed project timeline.");

    // Add both collections
    handle_collection_add(&env.ctx, "docs", "docs", "**/*.md").unwrap();
    handle_collection_add(&env.ctx, "notes", "notes", "**/*.md").unwrap();
    handle_update(&env.ctx, &env.root).unwrap();

    let client = McpClient::new(&env);

    // Check we have both collections
    let collections = client.list_collections();
    assert_eq!(collections.len(), 2);

    let status = client.status();
    assert_eq!(status.documents, 4);

    // Search across all collections
    let all_results = client.search("project", 10, None);
    assert!(all_results.len() >= 2, "Should find results from multiple collections");

    // Search only in docs collection
    let docs_results = client.search("project", 10, Some("docs"));
    for result in &docs_results {
        // All results should be from docs path
        // (Note: the path is relative within collection)
        assert!(!result.path.contains("notes"), "Should only find docs");
    }

    // Multi-get from specific collection
    let notes_docs = client.multi_get("*.md", Some("notes"));
    assert_eq!(notes_docs.len(), 2, "Should find 2 notes documents");
    for (doc, _) in &notes_docs {
        assert_eq!(doc.collection, "notes");
    }
}

/// Scenario 4: Dynamic content updates.
///
/// Simulates an LLM agent that:
/// 1. Indexes initial documents
/// 2. Detects and handles content changes
/// 3. Handles new and deleted documents
#[test]
fn scenario_dynamic_updates() {
    let env = TestEnvironment::new();

    // Initial document
    env.create_file("docs/readme.md", "# Version 1\n\nInitial content.");
    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Initial state
    let status = client.status();
    assert_eq!(status.documents, 1);

    // Search finds version 1
    let results = client.search("Version 1", 5, None);
    assert!(!results.is_empty(), "Should find Version 1");

    // Simulate file modification (need to wait for mtime change)
    std::thread::sleep(std::time::Duration::from_secs(1));
    env.create_file("docs/readme.md", "# Version 2\n\nUpdated content with new features.");

    // LLM triggers update
    let update_result = client.update();
    assert_eq!(update_result.updated, 1, "Should have 1 updated document");

    // Search now finds version 2
    let results = client.search("Version 2", 5, None);
    assert!(!results.is_empty(), "Should find Version 2 after update");

    // Add a new document
    env.create_file("docs/changelog.md", "# Changelog\n\n## v2.0\n- Updated readme");
    let update_result = client.update();
    assert_eq!(update_result.added, 1, "Should have 1 added document");

    let status = client.status();
    assert_eq!(status.documents, 2);

    // Remove a document
    fs::remove_file(env.root.join("docs/changelog.md")).unwrap();
    let update_result = client.update();
    assert_eq!(update_result.removed, 1, "Should have 1 removed document");

    let status = client.status();
    assert_eq!(status.documents, 1);
}

/// Scenario 5: Complex search patterns.
///
/// Simulates an LLM agent using various search strategies:
/// 1. Exact phrase matching
/// 2. Boolean-style queries
/// 3. Handling no results gracefully
#[test]
fn scenario_complex_search() {
    let env = TestEnvironment::new();

    env.create_file("docs/rust.md", r#"# Rust Programming

Rust is a systems programming language focused on safety and performance.

## Key Features

- Memory safety without garbage collection
- Concurrency without data races
- Zero-cost abstractions

## Getting Started with Rust

Install rustup and start coding!
"#);

    env.create_file("docs/python.md", r#"# Python Programming

Python is a high-level, interpreted programming language.

## Key Features

- Easy to learn and read
- Extensive standard library
- Dynamic typing

## Getting Started with Python

Install Python and run your first script!
"#);

    env.create_file("docs/javascript.md", r#"# JavaScript Programming

JavaScript is the language of the web.

## Key Features

- Runs in browsers
- Event-driven programming
- Prototype-based inheritance

## Getting Started with JavaScript

Open your browser console and start coding!
"#);

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Search for memory-related content
    let memory_results = client.search("memory safety", 10, None);
    assert!(!memory_results.is_empty(), "Should find memory safety content");
    assert!(memory_results[0].path.contains("rust"), "Rust should be top result for memory safety");

    // Search for interpreted languages
    let interpreted_results = client.search("interpreted", 10, None);
    assert!(!interpreted_results.is_empty());
    assert!(interpreted_results[0].path.contains("python"), "Python should be top for interpreted");

    // Search for common term - should find in multiple docs
    let getting_started = client.search("Getting Started", 10, None);
    assert!(getting_started.len() >= 3, "Should find Getting Started in all docs");

    // Search for something that doesn't exist
    let no_results = client.search("xyzzy_nonexistent_term_12345", 10, None);
    assert!(no_results.is_empty(), "Should handle no results gracefully");

    // Search with limit
    let limited = client.search("programming", 2, None);
    assert!(limited.len() <= 2, "Should respect limit");
}

/// Scenario 6: Document retrieval patterns.
///
/// Simulates an LLM agent retrieving documents in various ways:
/// 1. By ID
/// 2. By pattern (multi_get)
/// 3. With line ranges
#[test]
fn scenario_document_retrieval() {
    let env = TestEnvironment::new();

    // Create a larger document for line range testing
    let mut long_content = String::from("# Long Document\n\n");
    for i in 1..=50 {
        long_content.push_str(&format!("Line {} of content with various words.\n", i));
    }
    env.create_file("docs/long.md", &long_content);

    env.create_file("docs/chapter1.md", "# Chapter 1\n\nIntroduction");
    env.create_file("docs/chapter2.md", "# Chapter 2\n\nMain content");
    env.create_file("docs/chapter3.md", "# Chapter 3\n\nConclusion");
    env.create_file("docs/appendix/a.md", "# Appendix A\n\nReferences");

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Get all chapters using pattern
    let chapters = client.multi_get("chapter*.md", None);
    assert_eq!(chapters.len(), 3, "Should find 3 chapters");

    // Get only appendix
    let appendix = client.multi_get("appendix/*.md", None);
    assert_eq!(appendix.len(), 1, "Should find 1 appendix");

    // Get all documents
    let all_docs = client.multi_get("**/*.md", None);
    assert_eq!(all_docs.len(), 5, "Should find all 5 documents");

    // Find the long document and test line ranges
    let long_doc_search = client.search("Long Document", 1, None);
    assert!(!long_doc_search.is_empty());

    let doc_id = long_doc_search[0].id;

    // Get lines 10-15
    let lines = client.get_lines(doc_id, 10, 15).expect("Should get lines");
    let line_count = lines.lines().count();
    assert!(line_count <= 6, "Should get approximately 6 lines");

    // Get lines at the end
    let end_lines = client.get_lines(doc_id, 48, 55).expect("Should get end lines");
    assert!(!end_lines.is_empty(), "Should get some lines near the end");

    // Get lines beyond document
    let beyond = client.get_lines(doc_id, 100, 110).expect("Should handle out of range");
    assert!(beyond.is_empty(), "Should return empty for lines beyond document");
}

/// Scenario 7: Error handling and edge cases.
///
/// Simulates an LLM agent encountering various edge cases:
/// 1. Non-existent documents
/// 2. Invalid patterns
/// 3. Edge case queries
#[test]
fn scenario_edge_cases() {
    let env = TestEnvironment::new();

    env.create_file("docs/test.md", "# Test\n\nContent");
    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Non-existent document ID
    let missing = client.get(99999);
    assert!(missing.is_none(), "Should return None for missing document");

    // Pattern matching no documents
    let no_match = client.multi_get("*.xyz", None);
    assert!(no_match.is_empty(), "Should return empty for no matches");

    // Non-existent collection filter
    let wrong_collection = client.search("test", 10, Some("nonexistent"));
    assert!(wrong_collection.is_empty(), "Should return empty for wrong collection");

    // Very long query
    let long_query = "word ".repeat(100);
    let _long_results = client.search(&long_query, 10, None);
    // Should not crash, may or may not find results

    // Single character query - tests minimum viable query
    let _single_char = client.search("a", 10, None);
    // Should not crash

    // Unicode query - tests international content
    let _unicode = client.search("日本語", 10, None);
    // Should not crash, even if no results
}

/// Scenario 8: Frontmatter and metadata extraction.
///
/// Simulates an LLM agent working with documents that have rich metadata.
#[test]
fn scenario_frontmatter_metadata() {
    let env = TestEnvironment::new();

    env.create_file("docs/with-frontmatter.md", r#"---
title: My Document Title
author: Test Author
date: 2024-01-15
tags:
  - documentation
  - tutorial
---

# Content

This document has rich frontmatter metadata.
"#);

    env.create_file("docs/no-frontmatter.md", r#"# Plain Document

This document has no frontmatter.
"#);

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Search and check that title is extracted
    let _results = client.search("Document Title", 5, None);
    // Title should be searchable

    let all_docs = client.multi_get("*.md", None);
    assert_eq!(all_docs.len(), 2);

    // Check that frontmatter doc has title
    for (doc, _) in &all_docs {
        if doc.relative_path.contains("with-frontmatter") {
            assert!(doc.title.is_some(), "Should extract title from frontmatter");
            assert_eq!(doc.title.as_ref().unwrap(), "My Document Title");
        }
    }
}

/// Scenario 9: Concurrent-like access patterns.
///
/// Simulates multiple sequential operations that might occur
/// in rapid succession, similar to how an LLM might make
/// multiple tool calls in quick succession.
#[test]
fn scenario_rapid_operations() {
    let env = TestEnvironment::new();

    // Create many documents
    for i in 0..20 {
        env.create_file(
            &format!("docs/doc{:02}.md", i),
            &format!("# Document {}\n\nContent for document number {}.", i, i),
        );
    }

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Rapid status checks
    for _ in 0..5 {
        let status = client.status();
        assert_eq!(status.documents, 20);
    }

    // Rapid searches
    let mut all_results = Vec::new();
    for i in 0..10 {
        let results = client.search(&format!("Document {}", i), 5, None);
        all_results.push(results);
    }

    // All searches should have succeeded
    for (i, results) in all_results.iter().enumerate() {
        assert!(!results.is_empty(), "Search {} should have results", i);
    }

    // Rapid document retrievals
    let first_search = client.search("Document", 10, None);
    for result in &first_search {
        let doc = client.get(result.id);
        assert!(doc.is_some(), "Should retrieve document {}", result.id);
    }
}

/// Scenario 10: Realistic LLM conversation simulation.
///
/// Simulates a complete conversation where an LLM is helping
/// a user find information in their knowledge base.
#[test]
fn scenario_llm_conversation() {
    let env = TestEnvironment::new();

    // User's knowledge base
    env.create_file("kb/projects/project-alpha.md", r#"---
title: Project Alpha
status: active
---

# Project Alpha

## Overview

Project Alpha is our main product initiative.

## Team

- Alice (Lead)
- Bob (Backend)
- Carol (Frontend)

## Timeline

- Q1: Design phase
- Q2: Implementation
- Q3: Testing
- Q4: Launch
"#);

    env.create_file("kb/projects/project-beta.md", r#"---
title: Project Beta
status: planning
---

# Project Beta

## Overview

Project Beta is a research initiative.

## Goals

- Explore new technologies
- Build prototypes
- Evaluate feasibility
"#);

    env.create_file("kb/people/alice.md", r#"---
title: Alice Johnson
role: Tech Lead
---

# Alice Johnson

## Role

Senior Tech Lead on Project Alpha.

## Contact

alice@company.com
"#);

    env.create_file("kb/meetings/2024-01-15.md", r#"---
title: Weekly Standup - Jan 15
date: 2024-01-15
---

# Weekly Standup

## Attendees

- Alice
- Bob

## Discussion

- Project Alpha is on track
- Bob completing backend API
- Need to sync with Carol on frontend
"#);

    env.add_and_index_collection("kb", "kb", "**/*.md");

    let client = McpClient::new(&env);

    // User: "What projects are we working on?"
    // LLM first checks status to understand the KB
    let status = client.status();
    assert!(status.documents > 0, "LLM sees documents are available");

    // LLM searches for projects
    let projects = client.search("project", 10, None);
    assert!(projects.len() >= 2, "LLM finds project documents");

    // LLM might retrieve specific project details
    let alpha_search = client.search("Project Alpha timeline", 1, None);
    if !alpha_search.is_empty() {
        let (_doc, content) = client.get(alpha_search[0].id)
            .expect("LLM retrieves project document");
        assert!(content.contains("Q1") || content.contains("Timeline"),
            "LLM can read project timeline");
    }

    // User: "Who is Alice?"
    let alice_search = client.search("Alice", 5, None);
    assert!(!alice_search.is_empty(), "LLM finds information about Alice");

    // LLM might want to get all people documents
    let people = client.multi_get("people/*.md", None);
    assert!(!people.is_empty(), "LLM can batch-retrieve people docs");

    // User: "What happened in the last meeting?"
    let _meeting_search = client.search("standup meeting", 5, None);
    // Should find the meeting notes

    // User asks about something not in KB
    let _unknown = client.search("Project Gamma budget", 5, None);
    // LLM should handle empty results gracefully
    // (would respond "I don't have information about Project Gamma")
}

// =============================================================================
// Additional utility tests
// =============================================================================

/// Test that search scoring makes sense.
#[test]
fn test_search_scoring() {
    let env = TestEnvironment::new();

    // Create documents with varying relevance to "rust programming"
    env.create_file("docs/rust-guide.md",
        "# Complete Rust Programming Guide\n\nLearn Rust programming from scratch. Rust is great.");
    env.create_file("docs/python.md",
        "# Python Guide\n\nLearn Python. Also mentions rust once.");
    env.create_file("docs/unrelated.md",
        "# Cooking Recipes\n\nHow to make pasta.");

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    let results = client.search("rust programming", 10, None);

    // Rust guide should be the top result
    assert!(!results.is_empty());
    assert!(results[0].path.contains("rust-guide"),
        "Most relevant document should be first");

    // Cooking should not appear (or be last if it somehow matches)
    let has_cooking = results.iter().any(|r| r.path.contains("cooking") || r.path.contains("unrelated"));
    if has_cooking {
        let cooking_pos = results.iter().position(|r| r.path.contains("unrelated"));
        assert!(cooking_pos == Some(results.len() - 1) || cooking_pos.is_none(),
            "Unrelated document should be last or absent");
    }
}

/// Test document deduplication via content hash.
#[test]
fn test_content_deduplication() {
    let env = TestEnvironment::new();

    // Create two documents with identical content
    let content = "# Duplicated Content\n\nThis content appears in multiple places.";
    env.create_file("docs/original.md", content);
    env.create_file("docs/copy.md", content);

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    let status = client.status();
    assert_eq!(status.documents, 2, "Should have 2 document entries");

    // Both should be retrievable
    let docs = client.multi_get("*.md", None);
    assert_eq!(docs.len(), 2);

    // Content should be the same (deduplicated by hash internally)
    assert_eq!(docs[0].1, docs[1].1);
}

// =============================================================================
// Data Consistency Tests
// =============================================================================

/// Test that all indexed documents are searchable and retrievable with exact content.
#[test]
fn test_write_index_search_retrieve_consistency() {
    let env = TestEnvironment::new();

    // Create documents with unique, identifiable content
    let documents = vec![
        ("docs/alpha.md", "# Alpha Document\n\nUnique content for alpha: xyz123"),
        ("docs/beta.md", "# Beta Document\n\nUnique content for beta: abc456"),
        ("docs/gamma.md", "# Gamma Document\n\nUnique content for gamma: def789"),
    ];

    for (path, content) in &documents {
        env.create_file(path, content);
    }

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Verify document count
    let status = client.status();
    assert_eq!(status.documents, 3, "All documents should be indexed");

    // Verify each document is searchable by its unique identifier
    for (path, original_content) in &documents {
        let filename = std::path::Path::new(path)
            .file_stem()
            .unwrap()
            .to_str()
            .unwrap();

        // Search for the unique identifier
        let unique_id = match filename {
            "alpha" => "xyz123",
            "beta" => "abc456",
            "gamma" => "def789",
            _ => panic!("Unknown file"),
        };

        let results = client.search(unique_id, 10, None);
        assert!(
            !results.is_empty(),
            "Document {} should be searchable by unique ID {}",
            path,
            unique_id
        );

        // Verify the correct document is found (not just any document)
        let found = results.iter().any(|r| r.path.contains(filename));
        assert!(
            found,
            "Search for {} should find {} in results: {:?}",
            unique_id,
            filename,
            results.iter().map(|r| &r.path).collect::<Vec<_>>()
        );

        // Retrieve and verify exact content match
        let doc_result = results.iter().find(|r| r.path.contains(filename)).unwrap();
        let (doc, retrieved_content) = client.get(doc_result.id).expect("Should retrieve document");

        assert_eq!(
            &retrieved_content, *original_content,
            "Retrieved content should exactly match original for {}",
            path
        );

        // Verify path is preserved
        assert!(
            doc.relative_path.contains(filename),
            "Document path should contain filename"
        );
    }
}

/// Test that frontmatter metadata is correctly extracted and preserved.
#[test]
fn test_frontmatter_metadata_consistency() {
    let env = TestEnvironment::new();

    let content = r#"---
title: Precise Test Title
author: Test Author
tags:
  - tag1
  - tag2
---

# Body Content

This is the body.
"#;

    env.create_file("docs/metadata.md", content);
    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Retrieve and verify metadata
    let docs = client.multi_get("*.md", None);
    assert_eq!(docs.len(), 1);

    let (doc, retrieved_content) = &docs[0];

    // Verify title extraction
    assert_eq!(
        doc.title.as_deref(),
        Some("Precise Test Title"),
        "Title should be extracted from frontmatter"
    );

    // Verify content is preserved exactly
    assert_eq!(
        retrieved_content, content,
        "Full content including frontmatter should be preserved"
    );
}

/// Test that document updates preserve consistency.
#[test]
fn test_update_preserves_consistency() {
    let env = TestEnvironment::new();

    // Initial content
    let v1_content = "# Version 1\n\nOriginal content with marker: ORIGINAL_V1";
    env.create_file("docs/versioned.md", v1_content);
    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Verify V1 is searchable
    let v1_results = client.search("ORIGINAL_V1", 10, None);
    assert!(!v1_results.is_empty(), "V1 should be searchable");

    // Get document ID for later comparison
    let v1_doc_id = v1_results[0].id;
    let (_, v1_retrieved) = client.get(v1_doc_id).expect("Should retrieve V1");
    assert_eq!(v1_retrieved, v1_content);

    // Update content (need mtime change)
    std::thread::sleep(std::time::Duration::from_secs(1));
    let v2_content = "# Version 2\n\nUpdated content with marker: UPDATED_V2";
    env.create_file("docs/versioned.md", v2_content);

    // Trigger reindex
    let update_result = client.update();
    assert_eq!(update_result.updated, 1, "Should detect 1 update");

    // Verify V2 is now searchable
    let v2_results = client.search("UPDATED_V2", 10, None);
    assert!(!v2_results.is_empty(), "V2 should be searchable after update");

    // Verify V1 marker is no longer searchable
    let old_results = client.search("ORIGINAL_V1", 10, None);
    assert!(
        old_results.is_empty(),
        "V1 content should not be searchable after update"
    );

    // Retrieve and verify new content
    let v2_doc = &v2_results[0];
    let (_, v2_retrieved) = client.get(v2_doc.id).expect("Should retrieve V2");
    assert_eq!(
        v2_retrieved, v2_content,
        "Retrieved content should be V2 after update"
    );
}

/// Test that deleted documents are properly removed from search.
#[test]
fn test_deletion_removes_from_search() {
    let env = TestEnvironment::new();

    // Create two documents
    env.create_file("docs/keep.md", "# Keep\n\nThis document stays: KEEPER_DOC");
    env.create_file("docs/delete.md", "# Delete\n\nThis document goes: DELETED_DOC");
    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Verify both are searchable
    let keep_results = client.search("KEEPER_DOC", 10, None);
    let delete_results = client.search("DELETED_DOC", 10, None);
    assert!(!keep_results.is_empty(), "Keep doc should be searchable");
    assert!(!delete_results.is_empty(), "Delete doc should be searchable initially");

    // Delete one document
    fs::remove_file(env.root.join("docs/delete.md")).unwrap();
    let update_result = client.update();
    assert_eq!(update_result.removed, 1, "Should detect 1 removal");

    // Verify deleted doc is no longer searchable
    let after_delete = client.search("DELETED_DOC", 10, None);
    assert!(
        after_delete.is_empty(),
        "Deleted document should not appear in search"
    );

    // Verify kept doc is still searchable
    let still_there = client.search("KEEPER_DOC", 10, None);
    assert!(
        !still_there.is_empty(),
        "Kept document should still be searchable"
    );
}

/// Test search relevance: more relevant documents should rank higher.
#[test]
fn test_search_relevance_ranking() {
    let env = TestEnvironment::new();

    // Create documents with varying relevance to "rust programming"
    env.create_file(
        "docs/rust-focused.md",
        r#"# Rust Programming Guide

Rust programming is powerful. Rust programming language offers memory safety.
Programming in Rust is enjoyable. More about Rust programming here.
"#,
    );

    env.create_file(
        "docs/mentions-rust.md",
        r#"# General Programming

Various programming languages exist. Rust is one of them.
Python is also popular for programming.
"#,
    );

    env.create_file(
        "docs/no-rust.md",
        r#"# Python Tutorial

Python programming guide. Learn Python today.
JavaScript is also covered.
"#,
    );

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    let results = client.search("rust programming", 10, None);

    // Should find at least the first two documents
    assert!(results.len() >= 2, "Should find documents mentioning rust or programming");

    // The rust-focused document should be first (highest relevance)
    assert!(
        results[0].path.contains("rust-focused"),
        "Most relevant document should rank first. Got: {:?}",
        results.iter().map(|r| (&r.path, r.score)).collect::<Vec<_>>()
    );

    // If the python-only doc appears, it should be last
    if let Some(pos) = results.iter().position(|r| r.path.contains("no-rust")) {
        assert_eq!(
            pos,
            results.len() - 1,
            "Least relevant document should be last"
        );
    }
}

/// Test that all documents in a collection are retrievable.
#[test]
fn test_all_documents_retrievable() {
    let env = TestEnvironment::new();

    // Create 10 documents
    let doc_count = 10;
    let mut expected_contents: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for i in 0..doc_count {
        let filename = format!("doc{:02}.md", i);
        let content = format!("# Document {}\n\nContent for document number {}.", i, i);
        env.create_file(&format!("docs/{}", filename), &content);
        expected_contents.insert(filename, content);
    }

    env.add_and_index_collection("docs", "docs", "**/*.md");

    let client = McpClient::new(&env);

    // Verify count
    let status = client.status();
    assert_eq!(status.documents, doc_count, "All documents should be indexed");

    // Retrieve all documents and verify content
    let all_docs = client.multi_get("*.md", None);
    assert_eq!(all_docs.len(), doc_count, "All documents should be retrievable");

    // Verify each document's content matches
    for (doc, content) in &all_docs {
        let filename = std::path::Path::new(&doc.relative_path)
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();

        let expected = expected_contents
            .get(filename)
            .unwrap_or_else(|| panic!("Unexpected file: {}", filename));

        assert_eq!(
            content, expected,
            "Content mismatch for {}",
            filename
        );
    }
}
