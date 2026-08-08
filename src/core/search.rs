//! Reading documents: hybrid search and batch retrieval.
//!
//! The read path every adapter shares. The MCP layer calls straight into
//! `hybrid_search_fts` because it owns its own FTS escaping and embedding — it
//! must not run ONNX inference while holding the context mutex — so this cannot
//! live behind a command-line entry point.

use std::collections::HashMap;

use crate::core::Context;
use crate::domain::{Document, SearchQuery, SearchResult};
use crate::error::{Error, Result};
use crate::store::{collections, documents, hybrid, search, vectors};
use globset::Glob;

/// Hybrid doc search over a pre-built FTS5 expression and a pre-computed query
/// embedding. Mirrors `memory::search_entries_hybrid_fts`: the caller owns both
/// escaping (so a full-sentence prompt can be OR-expanded) and embedding (so
/// inference never runs while a caller holds the context mutex).
///
/// `query_embedding: None` degrades to BM25-only.
pub fn hybrid_search_fts(
    ctx: &Context,
    fts_query: &str,
    query_embedding: Option<&[f32]>,
    limit: usize,
    collection: Option<&str>,
    include_superseded: bool,
) -> Result<Vec<SearchResult>> {
    // Get BM25 results
    let bm25_query = SearchQuery {
        text: String::new(), // ignored: `fts_query` is already escaped
        limit: limit * 2,    // Get more for fusion
        collection: collection.map(String::from),
        tags: vec![],
        include_superseded,
    };
    let bm25_results = search::search_fts(&ctx.conn, fts_query, &bm25_query)?;

    let vector_results = match query_embedding {
        Some(embedding) => vectors::chunk_vector_search(&ctx.conn, embedding, limit * 2)?,
        None => Vec::new(),
    };

    // Fuse results using RRF
    let config = hybrid::HybridConfig::default();
    let mut fused = hybrid::rrf_fusion(&bm25_results, &vector_results, &config);

    // Normalize scores
    hybrid::normalize_scores(&mut fused);

    if fused.is_empty() {
        return Ok(Vec::new());
    }

    // Batch retrieve all documents in a single query (includes status)
    let doc_ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
    let docs = documents::get_documents_batch(&ctx.conn, &doc_ids)?;

    // Build BM25 lookup for superseded_by metadata
    let bm25_map: HashMap<i64, &SearchResult> = bm25_results.iter().map(|r| (r.id, r)).collect();

    // Build a map for quick lookup
    let doc_map: std::collections::HashMap<i64, _> = docs.into_iter().map(|d| (d.id, d)).collect();

    // Convert to SearchResult format, preserving RRF order
    let mut results = Vec::new();
    for (doc_id, score) in fused {
        if let Some(doc) = doc_map.get(&doc_id) {
            // Filter by collection if specified; exclude claude_sessions from default search
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
            } else if doc.collection == crate::domain::COLLECTION_CLAUDE_SESSIONS {
                continue;
            }

            // Filter superseded/retracted documents (vector search doesn't filter by status)
            if !include_superseded {
                if matches!(doc.status.as_deref(), Some("superseded" | "retracted")) {
                    continue;
                }
            }

            // Populate superseded_by from BM25 results if available
            let bm25 = bm25_map.get(&doc_id);
            results.push(SearchResult {
                id: doc.id,
                collection: doc.collection.clone(),
                path: doc.relative_path.clone(),
                title: doc.title.clone(),
                score,
                snippets: vec![],
                status: doc.status.clone(),
                superseded_by: bm25.and_then(|r| r.superseded_by.clone()),
                repo_root: None,
            });

            // Stop once we have enough results
            if results.len() >= limit {
                break;
            }
        }
    }

    Ok(results)
}
/// Handle `mdkb query` command - hybrid search with RRF fusion.
///
/// Uses batch document retrieval to avoid N+1 queries.
/// Uses cached model to avoid 2-5 second load time per request.
pub fn handle_hybrid_search(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
    include_superseded: bool,
) -> Result<Vec<SearchResult>> {
    let fts_query = search::escape_fts5_query(query_text);
    // Falls back to BM25-only if the embedding service is unavailable.
    let query_embedding =
        match crate::llm::get_cached_service().and_then(|s| s.embed_query(query_text)) {
            Ok(v) => Some(v),
            Err(e) => {
                tracing::debug!("Hybrid search falling back to BM25-only: {e}");
                None
            }
        };
    hybrid_search_fts(
        ctx,
        &fts_query,
        query_embedding.as_deref(),
        limit,
        collection,
        include_superseded,
    )
}
/// Handle `mdkb mget` command - batch retrieval by pattern.
///
/// Uses batch content retrieval to avoid N+1 queries.
pub fn handle_mget(
    ctx: &Context,
    pattern: &str,
    collection_filter: Option<&str>,
) -> Result<Vec<(Document, String)>> {
    let glob = Glob::new(pattern)
        .map_err(|e| Error::other(format!("Invalid glob pattern '{}': {}", pattern, e)))?
        .compile_matcher();

    // Get all documents, optionally filtered by collection
    let all_collections = collections::list_collections(&ctx.conn)?;
    let mut matching_docs = Vec::new();

    for coll in &all_collections {
        // Skip if collection filter doesn't match
        if let Some(filter) = collection_filter {
            if coll.name != filter {
                continue;
            }
        }

        let docs = documents::list_documents(&ctx.conn, &coll.name)?;

        for doc in docs {
            // Check if path matches pattern
            if glob.is_match(&doc.relative_path) {
                matching_docs.push(doc);
            }
        }
    }

    if matching_docs.is_empty() {
        return Ok(Vec::new());
    }

    // Batch retrieve all content in a single query (fixes N+1 query pattern)
    let hashes: Vec<&str> = matching_docs.iter().map(|d| d.hash.as_str()).collect();
    let content_map = documents::get_content_batch(&ctx.conn, &hashes)?;

    // Combine documents with their content
    let mut results = Vec::new();
    for doc in matching_docs {
        if let Some(content) = content_map.get(&doc.hash) {
            results.push((doc, content.clone()));
        }
    }

    Ok(results)
}
