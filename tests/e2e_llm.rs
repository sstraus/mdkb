//! Integration tests for LLM features.
//!
//! These tests require downloading the embedding model (~100MB) and are slow.
//! They are ignored by default and must be run explicitly:
//!
//! ```bash
//! cargo test --test e2e_llm -- --ignored
//! ```
//!
//! The model is cached so subsequent runs are faster.

use std::fs;
use std::sync::Arc;

use mdkb::cli::handlers::{
    Context, handle_collection_add, handle_embed, handle_hybrid_search, handle_init, handle_update,
    handle_vsearch,
};
use tempfile::TempDir;

/// Set up a test environment with indexed documents.
fn setup_test_env() -> (TempDir, Context) {
    let temp = TempDir::new().expect("failed to create temp dir");

    // Create test documents
    let docs_dir = temp.path().join("docs");
    fs::create_dir_all(&docs_dir).unwrap();

    fs::write(
        docs_dir.join("rust-basics.md"),
        r#"---
title: Rust Programming Basics
tags: [rust, programming, tutorial]
---

# Rust Programming Basics

Rust is a systems programming language focused on safety, speed, and concurrency.

## Memory Safety

Rust guarantees memory safety without garbage collection through its ownership system.
The borrow checker ensures references are always valid.

## Error Handling

Rust uses Result and Option types for explicit error handling.
The ? operator makes error propagation concise.
"#,
    )
    .unwrap();

    fs::write(
        docs_dir.join("python-basics.md"),
        r#"---
title: Python Programming Basics
tags: [python, programming, tutorial]
---

# Python Programming Basics

Python is a high-level, interpreted programming language known for its readability.

## Dynamic Typing

Python uses dynamic typing, which means variable types are determined at runtime.
This provides flexibility but requires careful testing.

## Error Handling

Python uses try/except blocks for exception handling.
Common exceptions include ValueError, TypeError, and KeyError.
"#,
    )
    .unwrap();

    fs::write(
        docs_dir.join("web-security.md"),
        r#"---
title: Web Application Security
tags: [security, web, best-practices]
---

# Web Application Security

Security is critical for any web application.

## Authentication

Always use secure password hashing (bcrypt, argon2).
Implement multi-factor authentication when possible.

## Authorization

Use role-based access control (RBAC) for permissions.
Validate user permissions on every request.

## Common Vulnerabilities

- SQL Injection: Use parameterized queries
- XSS: Sanitize user input, use Content Security Policy
- CSRF: Use anti-CSRF tokens
"#,
    )
    .unwrap();

    // Initialize mdkb
    handle_init(temp.path()).expect("init failed");

    // Open context and add collection
    let ctx = Context::open(temp.path()).expect("failed to open context");
    handle_collection_add(&ctx, "docs", "docs", "**/*.md").expect("failed to add collection");
    handle_update(&ctx, temp.path()).expect("failed to update");

    (temp, ctx)
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_embedding_service_loads() {
    let service = mdkb::llm::get_cached_service().expect("failed to load service");

    // Verify we can generate an embedding
    let embedding = service
        .embed_query("test text")
        .expect("failed to generate embedding");

    // AllMiniLML6V2 produces 384-dimensional embeddings
    assert_eq!(embedding.len(), 384);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_embedding_service_caching() {
    let svc1 = mdkb::llm::get_cached_service().expect("failed to load service");
    let svc2 = mdkb::llm::get_cached_service().expect("failed to get cached service");

    assert!(
        Arc::ptr_eq(&svc1, &svc2),
        "get_cached_service should return same Arc instance"
    );
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_query_vs_document_embeddings() {
    let service = mdkb::llm::get_cached_service().expect("failed to load service");

    let doc_embeddings = service
        .embed_documents(&["Rust programming language"])
        .unwrap();
    let query_embedding = service.embed_query("What is Rust?").unwrap();

    assert_eq!(doc_embeddings[0].len(), 384);
    assert_eq!(query_embedding.len(), 384);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_semantic_similarity() {
    let service = mdkb::llm::get_cached_service().expect("failed to load service");

    // These should be semantically similar
    let rust1 = service.embed_query("Rust programming language").unwrap();
    let rust2 = service.embed_query("Rust is a systems language").unwrap();

    // This should be less similar
    let python = service.embed_query("Python scripting language").unwrap();

    // This should be very different
    let cooking = service
        .embed_query("How to bake chocolate chip cookies")
        .unwrap();

    let sim_rust_rust = mdkb::llm::cosine_similarity(&rust1, &rust2);
    let sim_rust_python = mdkb::llm::cosine_similarity(&rust1, &python);
    let sim_rust_cooking = mdkb::llm::cosine_similarity(&rust1, &cooking);

    // Rust descriptions should be most similar to each other
    assert!(
        sim_rust_rust > sim_rust_python,
        "rust-rust ({}) should be more similar than rust-python ({})",
        sim_rust_rust,
        sim_rust_python
    );

    // Programming languages should be more similar than programming vs cooking
    assert!(
        sim_rust_python > sim_rust_cooking,
        "rust-python ({}) should be more similar than rust-cooking ({})",
        sim_rust_python,
        sim_rust_cooking
    );
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_handle_embed_generates_embeddings() {
    let (temp, ctx) = setup_test_env();

    // Generate embeddings for all documents
    let result = handle_embed(&ctx, None).expect("embed failed");

    assert_eq!(result.generated, 3, "should generate embeddings for 3 docs");
    assert_eq!(result.skipped, 0, "no docs should be skipped on first run");
    assert!(result.errors.is_empty(), "should have no errors");

    // Running again should skip all
    let result2 = handle_embed(&ctx, None).expect("second embed failed");
    assert_eq!(result2.generated, 0, "should generate 0 on second run");
    assert_eq!(result2.skipped, 3, "should skip all 3 on second run");

    drop(temp);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_vsearch_finds_semantically_related() {
    let (temp, ctx) = setup_test_env();

    // Generate embeddings first
    handle_embed(&ctx, None).expect("embed failed");

    // Search for memory safety - should find Rust doc
    let results = handle_vsearch(&ctx, "memory safety without garbage collection", 10, None)
        .expect("vsearch failed");

    assert!(!results.is_empty(), "vsearch should return results");

    // Rust basics should be top result for memory safety query
    let top_result = &results[0];
    assert!(
        top_result.path.contains("rust"),
        "top result for 'memory safety' should be rust doc, got: {}",
        top_result.path
    );

    drop(temp);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_vsearch_with_collection_filter() {
    let (temp, ctx) = setup_test_env();

    // Add a second collection
    let notes_dir = temp.path().join("notes");
    fs::create_dir_all(&notes_dir).unwrap();
    fs::write(
        notes_dir.join("meeting.md"),
        "# Meeting Notes\n\nDiscussed Rust memory safety features.",
    )
    .unwrap();

    handle_collection_add(&ctx, "notes", "notes", "**/*.md").unwrap();
    handle_update(&ctx, temp.path()).unwrap();
    handle_embed(&ctx, None).expect("embed failed");

    // Search with collection filter
    let results = handle_vsearch(&ctx, "memory safety", 10, Some("docs")).expect("vsearch failed");

    // All results should be from docs collection
    for result in &results {
        assert!(
            result.path.starts_with("docs/") || !result.path.contains('/'),
            "result should be from docs collection: {}",
            result.path
        );
    }

    drop(temp);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_hybrid_search_combines_bm25_and_vector() {
    let (temp, ctx) = setup_test_env();

    // Generate embeddings
    handle_embed(&ctx, None).expect("embed failed");

    // Hybrid search should find results
    let results = handle_hybrid_search(&ctx, "error handling in programming", 10, None, false)
        .expect("hybrid search failed");

    assert!(!results.is_empty(), "hybrid search should return results");

    // Both Rust and Python docs discuss error handling
    let paths: Vec<_> = results.iter().map(|r| r.path.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.contains("rust")),
        "should find rust doc"
    );
    assert!(
        paths.iter().any(|p| p.contains("python")),
        "should find python doc"
    );

    drop(temp);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_hybrid_search_with_exact_term() {
    let (temp, ctx) = setup_test_env();

    // Generate embeddings
    handle_embed(&ctx, None).expect("embed failed");

    // Search for exact term that only appears in one doc
    let results = handle_hybrid_search(&ctx, "CSRF anti-CSRF tokens", 10, None, false)
        .expect("hybrid search failed");

    assert!(!results.is_empty(), "should find results");

    // Security doc should be top result (has exact match + semantic relevance)
    let top_result = &results[0];
    assert!(
        top_result.path.contains("security"),
        "top result for 'CSRF' should be security doc, got: {}",
        top_result.path
    );

    drop(temp);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_vsearch_respects_limit() {
    let (temp, ctx) = setup_test_env();

    handle_embed(&ctx, None).expect("embed failed");

    let results = handle_vsearch(&ctx, "programming", 2, None).expect("vsearch failed");

    assert!(
        results.len() <= 2,
        "should respect limit, got {} results",
        results.len()
    );

    drop(temp);
}

#[test]
#[ignore = "requires model download (~100MB), run with: cargo test --test e2e_llm -- --ignored"]
fn test_vsearch_empty_index() {
    let temp = TempDir::new().expect("failed to create temp dir");
    handle_init(temp.path()).expect("init failed");

    let ctx = Context::open(temp.path()).expect("failed to open context");

    // Search on empty index should return empty results, not error
    let results = handle_vsearch(&ctx, "anything", 10, None).expect("vsearch should not error");

    assert!(results.is_empty(), "empty index should return no results");

    drop(temp);
}
