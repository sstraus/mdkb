//! Transport-agnostic MCP tool dispatch.
//!
//! Each tool body lives here as a free function taking a `&RepoHandle` plus
//! a `DispatchContext`. `dispatch_call` routes a JSON-RPC-style call by
//! method name and returns a JSON `Value`. The rmcp `#[tool]` wrappers in
//! `server.rs` call these impls and wrap the result into a `CallToolResult`;
//! the hook socket in `daemon::ipc_server` calls `dispatch_call` directly.
//!
//! Story 3 of `plans/daemon-ipc-socket.md` — initial slice wires `status`
//! only. Remaining 10 tools land in follow-up commits under #007-16f7.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};

use crate::cli::handlers::{Context, handle_hybrid_search, handle_mget};
use crate::code::indexing::IndexFacade;
use crate::daemon::registry::RepoHandle;
use crate::domain::SearchResult;
use crate::metrics::{UsageMetrics, count_tokens, truncate_with_continuation, truncate_with_ellipsis};
use crate::store::{collections, documents, evolution, memory, search, stats};

use super::server::{
    apply_line_range, apply_min_confidence, format_memory_search_results, format_search_results,
    format_symbol, format_symbol_with_file_tokens, format_ttl_info, ood_hint, relative_time_ago,
    resolve_document, truncate_text,
};
use super::tools::{GetParams, MemoryWriteBatchEntry, SearchParams};

/// Daemon-global state shared across all dispatched tool calls.
#[derive(Clone)]
pub struct DispatchContext {
    pub metrics: Arc<UsageMetrics>,
    pub session_id: Arc<AtomicI64>,
    pub persistent_call_count: Arc<AtomicU64>,
    pub optimize_interval_calls: u64,
}

impl std::fmt::Debug for DispatchContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchContext")
            .field("session_id", &self.session_id.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl DispatchContext {
    /// Record a tool call against per-repo stats. No-op when session not yet
    /// established (session_id == 0). Uses `handle.ctx` so it works in both
    /// standalone and global daemon mode.
    pub async fn record_persistent_call(
        &self,
        handle: &RepoHandle,
        tool_name: &str,
        tokens: usize,
        results: usize,
        truncated: bool,
    ) {
        let session_id = self.session_id.load(Ordering::Relaxed);
        if session_id == 0 {
            return;
        }

        let ctx_guard = handle.ctx.lock().await;
        let Some(ctx) = ctx_guard.as_ref() else {
            return;
        };

        if let Err(e) =
            stats::record_call(&ctx.conn, session_id, tool_name, tokens, results, truncated)
        {
            tracing::warn!("Failed to record call stats: {e}");
        }

        let call_count = self.persistent_call_count.fetch_add(1, Ordering::Relaxed) + 1;
        if crate::store::maintenance::should_optimize(call_count, self.optimize_interval_calls) {
            if let Err(e) = crate::store::maintenance::run_optimize(&ctx.conn) {
                tracing::warn!("PRAGMA optimize failed: {e}");
            }
        }
    }
}

pub fn mcp_error(message: impl Into<Cow<'static, str>>) -> McpError {
    McpError {
        code: ErrorCode::INTERNAL_ERROR,
        message: message.into(),
        data: None,
    }
}

/// Ensure the repo's database context is initialized. Mirrors
/// `McpServer::ensure_handle_context` — kept in sync until that caller is
/// retired in a later slice.
pub async fn ensure_handle_context(handle: &RepoHandle) -> Result<(), McpError> {
    let mut ctx_guard = handle.ctx.lock().await;
    if ctx_guard.is_none() {
        if handle.doc_reindex_active.load(Ordering::Relaxed) {
            return Err(mcp_error("Repo initializing, retry shortly"));
        }
        let ctx = match Context::open(&handle.root) {
            Ok(ctx) => ctx,
            Err(e) if e.is_not_found() => {
                tracing::info!("Auto-initializing mdkb at {}", handle.root.display());
                Context::init(&handle.root)
                    .map_err(|e| mcp_error(format!("Failed to auto-initialize mdkb: {e}")))?
            }
            Err(e) => return Err(mcp_error(format!("Failed to open database: {e}"))),
        };
        *ctx_guard = Some(ctx);
    }
    Ok(())
}

/// Acquire (and lazily initialize) the code index on a repo handle.
pub async fn acquire_handle_code_index(
    handle: &RepoHandle,
) -> Result<tokio::sync::MutexGuard<'_, Option<IndexFacade>>, McpError> {
    if handle.code_reindex_active.load(Ordering::Relaxed) {
        return Ok(handle.code_index.lock().await);
    }
    let mut idx_guard = handle.code_index.lock().await;
    if idx_guard.is_none() {
        let index_path = handle.root.join(".mdkb/code.sqlite");
        let mut facade = IndexFacade::open_or_create(&index_path)
            .map_err(|e| mcp_error(format!("Failed to open code index: {e}")))?;
        let pipeline_config = crate::code::indexing::pipeline::PipelineConfig {
            ignore_patterns: handle.code_ignore_patterns.clone(),
            respect_gitignore: handle.config.code.indexing.respect_gitignore,
            ..Default::default()
        };
        facade = facade.with_config(pipeline_config);
        *idx_guard = Some(facade);
    }
    Ok(idx_guard)
}

// ── Tool impls ──────────────────────────────────────────────────────────────

/// `status` — returns the human-readable index status string. Callers wrap
/// this in the transport-appropriate envelope (CallToolResult or JSON-RPC).
pub async fn status_impl(handle: &RepoHandle) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let mut output = {
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;

        let index_status = search::get_status(&ctx.conn)
            .map_err(|e| mcp_error(format!("Failed to get status: {e}")))?;

        let mut output = format!(
            "## Index Status\n\nDocuments: {}\nStale: {}\nDB Size: {} bytes\n",
            index_status.documents, index_status.stale_documents, index_status.db_size_bytes
        );

        let coll_list = collections::list_collections(&ctx.conn)
            .map_err(|e| mcp_error(format!("Failed to list collections: {e}")))?;

        output.push_str(&format!("\n## Collections ({})\n\n", coll_list.len()));
        if coll_list.is_empty() {
            output.push_str("No collections configured. Markdown files are indexed via collections (use CLI: `mdkb collection add <name> <path>`).\n");
        } else {
            for coll in &coll_list {
                let doc_count = collections::get_collection_document_count(&ctx.conn, &coll.name)
                    .unwrap_or(0);
                let source_tag = if coll.source == "convention" {
                    "[convention]"
                } else {
                    "[manual]"
                };
                output.push_str(&format!(
                    "- {} {} ({}): {} docs, pattern: {}\n",
                    coll.name, source_tag, coll.path, doc_count, coll.pattern
                ));
            }
        }

        output
    };

    if let Ok(idx_guard) = acquire_handle_code_index(handle).await {
        if let Some(facade) = idx_guard.as_ref() {
            let symbols = facade.symbol_count();
            let files = facade.file_count();
            let relationships = facade.relationship_count();
            output.push_str(&format!(
                "\n## Code Index\n\nSymbols: {}\nFiles: {}\nRelationships: {}\n",
                symbols, files, relationships
            ));
            if symbols == 0 {
                output.push_str("\nNo symbols indexed yet. Run `update` to index source code.\n");
            }
        }
    }

    Ok(output)
}

/// `memory_delete` — delete a memory entry by id. Returns the human-readable
/// result string; callers wrap it for the transport.
pub async fn memory_delete_impl(handle: &RepoHandle, id: &str) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    let deleted = memory::delete_entry(&ctx.conn, id)
        .map_err(|e| mcp_error(format!("Failed to delete memory entry: {e}")))?;

    Ok(if deleted {
        format!("Deleted memory entry '{id}'.")
    } else {
        format!("Memory entry '{id}' not found.")
    })
}

/// `memory_confirm` — record a Bayesian confirmation signal against an entry.
/// `outcome` must be "confirmed" (+1) or "refuted" (-1, floor 0).
pub async fn memory_confirm_impl(
    handle: &RepoHandle,
    id: &str,
    outcome: &str,
) -> Result<String, McpError> {
    let delta: i32 = match outcome {
        "confirmed" => 1,
        "refuted" => -1,
        other => {
            return Err(mcp_error(format!(
                "Invalid outcome '{other}'. Expected \"confirmed\" or \"refuted\"."
            )));
        }
    };

    ensure_handle_context(handle).await?;

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    memory::confirm_entry(&ctx.conn, id, delta)
        .map_err(|e| mcp_error(format!("Failed to confirm memory entry: {e}")))
}

/// Core logic for writing a single memory entry. Used by both
/// `memory_write_impl` and `memory_write_batch_impl`. Synchronous because it
/// runs against an already-locked `Connection`; embedding I/O is best-effort
/// and falls back silently when the LLM service is unconfigured (tests).
fn write_single_memory(
    conn: &rusqlite::Connection,
    id: &str,
    title: &str,
    content: &str,
    entry_type_str: &str,
    source_type_str: &str,
    tags: &[String],
    ttl: Option<u64>,
    due_in: Option<u64>,
) -> Result<String, McpError> {
    memory::validate_entry_input(id, title, tags, content).map_err(|e| mcp_error(e.to_string()))?;

    let existing = memory::get_entry_without_tracking(conn, id)
        .map_err(|e| mcp_error(format!("Failed to check existing entry: {e}")))?;

    let entry_type: memory::EntryType = entry_type_str.parse().map_err(|e: String| {
        mcp_error(format!(
            "{e}. Valid types: topic, problem, decision, reminder"
        ))
    })?;

    let source_type: memory::SourceType =
        source_type_str.parse().map_err(|e: String| mcp_error(e))?;

    let now = chrono::Utc::now().timestamp();
    let expires_at = ttl.map(|s| now + s as i64);
    let due_at = due_in.map(|s| now + s as i64);
    let is_new = existing.is_none();

    // Pre-write duplicate check: reject if a near-identical entry exists (new entries only).
    // L2 distance < 0.32 ≈ cosine similarity > 0.95 — very high bar, minimizes false positives.
    if is_new {
        if let Ok(service) = crate::llm::get_cached_service() {
            let embed_text = format!("{title} {content}");
            if let Ok(embedding) = service.embed_query(&embed_text) {
                if let Ok(similar) =
                    crate::store::vectors::memory_vector_search(conn, &embedding, 3)
                {
                    for (rowid, distance) in &similar {
                        if *distance < 0.32 {
                            if let Ok(Some(dup)) = memory::get_entry_by_rowid(conn, *rowid) {
                                let similarity = 1.0 - (*distance as f64 * *distance as f64 / 2.0);
                                return Err(mcp_error(format!(
                                    "Near-duplicate entry exists: \"{}\" (id: {}, similarity: {:.0}%). \
                                     Update that entry instead, or use a more distinct title/content.",
                                    dup.title,
                                    dup.id,
                                    similarity * 100.0
                                )));
                            }
                        }
                    }
                }
            }
        }
    }

    let mut output = if let Some(mut existing_entry) = existing {
        if let Err(e) = memory::save_revision(
            conn,
            id,
            &existing_entry.content,
            content,
            existing_entry.source_type,
        ) {
            tracing::warn!("Failed to save revision for {id}: {e}");
        }

        existing_entry.title = title.to_string();
        existing_entry.content = content.to_string();
        existing_entry.entry_type = entry_type;
        existing_entry.tags = tags.to_vec();
        existing_entry.expires_at = expires_at;
        if due_in.is_some() {
            existing_entry.due_at = due_at;
        }
        memory::update_entry(conn, &existing_entry)
            .map_err(|e| mcp_error(format!("Failed to update memory entry: {e}")))?;

        let rev_info = memory::get_revision_summary(conn, id)
            .map(|s| {
                if s.count > 0 {
                    format!(" ({} revisions)", s.count)
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        format!("Updated memory entry: {id}{rev_info}")
    } else {
        let entry = memory::MemoryEntry {
            id: id.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            entry_type,
            tags: tags.to_vec(),
            status: memory::EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type,
            expires_at,
            due_at,
        };
        memory::add_entry(conn, &entry)
            .map_err(|e| mcp_error(format!("Failed to create memory entry: {e}")))?;
        format!("Created memory entry: {id}")
    };

    // Generate embedding for hybrid search + duplicate detection
    if let Ok(service) = crate::llm::get_cached_service() {
        let embed_text = format!("{title} {content}");
        match service.embed_query(&embed_text) {
            Ok(embedding) => {
                if let Some(rowid) = memory::get_rowid(conn, id).unwrap_or(None) {
                    if let Err(e) = crate::store::vectors::store_memory_embedding(
                        conn,
                        rowid,
                        &embedding,
                        crate::llm::embeddings::MODEL_NAME,
                    ) {
                        tracing::warn!("Failed to store memory embedding for '{id}': {e}");
                    }

                    if is_new {
                        let warnings = memory::find_similar_entries(conn, &embedding, rowid, id);
                        output.push_str(&warnings);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to embed memory entry '{id}': {e}");
            }
        }
    }

    Ok(output)
}

/// `memory_write` — create or update a single memory entry. Wraps
/// `write_single_memory` with `RepoHandle` ctx acquisition.
pub async fn memory_write_impl(
    handle: &RepoHandle,
    entry: &MemoryWriteBatchEntry,
) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    write_single_memory(
        &ctx.conn,
        &entry.id,
        &entry.title,
        &entry.content,
        &entry.entry_type,
        &entry.source_type,
        &entry.tags,
        entry.ttl,
        entry.due_in,
    )
}

/// `memory_write_batch` — create or update up to 20 entries in one call.
/// Returns `(joined_output, count)`. Enforces empty/limit guards before
/// touching the DB.
pub async fn memory_write_batch_impl(
    handle: &RepoHandle,
    entries: &[MemoryWriteBatchEntry],
) -> Result<(String, usize), McpError> {
    if entries.is_empty() {
        return Err(mcp_error("entries array must not be empty"));
    }
    if entries.len() > 20 {
        return Err(mcp_error("max 20 entries per batch"));
    }

    ensure_handle_context(handle).await?;

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    let mut results = Vec::with_capacity(entries.len());
    for entry in entries {
        let result = write_single_memory(
            &ctx.conn,
            &entry.id,
            &entry.title,
            &entry.content,
            &entry.entry_type,
            &entry.source_type,
            &entry.tags,
            entry.ttl,
            entry.due_in,
        )?;
        results.push(result);
    }

    let count = results.len();
    Ok((results.join("\n"), count))
}

/// `memory_list` — list active memory entries sorted by `sort` ("recent" |
/// "popular" | "newest"). Returns `(rendered_text, entry_count)` so callers
/// can record search metrics.
pub async fn memory_list_impl(
    handle: &RepoHandle,
    limit: usize,
    sort: &str,
) -> Result<(String, usize), McpError> {
    let sort_order: memory::MemorySortOrder = sort.parse().map_err(mcp_error)?;

    ensure_handle_context(handle).await?;

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    let entries = memory::list_entries_sorted(
        &ctx.conn,
        limit,
        sort_order,
        Some(memory::EntryStatus::Active),
    )
    .map_err(|e| mcp_error(format!("Failed to list memory entries: {e}")))?;

    if entries.is_empty() {
        return Ok(("No memory entries.".to_string(), 0));
    }

    let mut out = format!("Found {} memory entries:\n\n", entries.len());
    for e in &entries {
        let tags = e
            .tags
            .iter()
            .map(|t| format!("#{t}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ttl_info = format_ttl_info(e.expires_at);
        out.push_str(&format!(
            "- [{}] {} ({}, {}{}): {} {}\n",
            e.entry_type,
            e.id,
            e.title,
            relative_time_ago(e.updated_at),
            ttl_info,
            truncate_text(&e.content, 80),
            tags,
        ));
    }
    let count = entries.len();
    Ok((out, count))
}

/// `search` — single-repo hybrid search. Routes by `params.scope`:
/// "docs", "memory", "code", "symbols", or `None` (docs+memory). Returns
/// `(rendered_text, result_count)`. Cross-repo (`root="*"`) lives in
/// `cross_repo_search_impl`.
pub async fn search_impl(
    handle: &RepoHandle,
    params: &SearchParams,
) -> Result<(String, usize), McpError> {
    ensure_handle_context(handle).await?;

    let scope = params.scope.as_deref();
    let limit = params.limit.min(100);

    match scope {
        Some("docs") => {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;
            let results = handle_hybrid_search(
                ctx,
                &params.query,
                limit,
                params.collection.as_deref(),
                params.include_superseded,
            )
            .map_err(|e| mcp_error(format!("Search failed: {e}")))?;

            let top_score = results.first().map(|r| r.score);
            let mut output = format_search_results(&results, limit);
            if let Some(hint) = ood_hint(results.len(), top_score) {
                output.push_str(hint);
            }
            Ok((output, results.len()))
        }
        Some("memory") => {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;
            let query_embedding = match crate::llm::get_cached_service()
                .and_then(|s| s.embed_query(&params.query))
            {
                Ok(emb) => Some(emb),
                Err(e) => {
                    tracing::warn!("Memory search falling back to BM25-only: {e}");
                    None
                }
            };
            let entries = memory::search_entries_hybrid(
                &ctx.conn,
                &params.query,
                query_embedding.as_deref(),
                limit,
                handle.config.search.memory.access_recency_weight,
                handle.config.search.memory.recency_half_life_secs,
            )
            .map_err(|e| mcp_error(format!("Memory search failed: {e}")))?;
            let entries = apply_min_confidence(entries, params.min_confidence);

            let mut output = format_memory_search_results(&entries);
            if let Some(hint) = ood_hint(entries.len(), None) {
                output.push_str(hint);
            }
            Ok((output, entries.len()))
        }
        None => {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;
            let doc_results = handle_hybrid_search(
                ctx,
                &params.query,
                limit,
                params.collection.as_deref(),
                params.include_superseded,
            )
            .map_err(|e| mcp_error(format!("Search failed: {e}")))?;

            let query_embedding = match crate::llm::get_cached_service()
                .and_then(|s| s.embed_query(&params.query))
            {
                Ok(emb) => Some(emb),
                Err(e) => {
                    tracing::warn!("Memory search falling back to BM25-only: {e}");
                    None
                }
            };
            let mem_entries = memory::search_entries_hybrid(
                &ctx.conn,
                &params.query,
                query_embedding.as_deref(),
                limit,
                handle.config.search.memory.access_recency_weight,
                handle.config.search.memory.recency_half_life_secs,
            )
            .map_err(|e| mcp_error(format!("Memory search failed: {e}")))?;
            let mem_entries = apply_min_confidence(mem_entries, params.min_confidence);

            let total = doc_results.len() + mem_entries.len();
            let top_score = doc_results.first().map(|r| r.score);

            let mut output = if total == 0 {
                String::new()
            } else {
                let mut s = String::new();
                if !doc_results.is_empty() {
                    s.push_str(&format_search_results(&doc_results, limit));
                }
                if !mem_entries.is_empty() {
                    if !doc_results.is_empty() {
                        s.push_str("\n## Memory\n\n");
                    }
                    s.push_str(&format_memory_search_results(&mem_entries));
                }
                s
            };
            if let Some(hint) = ood_hint(total, top_score) {
                output.push_str(hint);
            }
            Ok((output, total))
        }
        Some("code") | Some("symbols") => {
            let mut idx_guard = acquire_handle_code_index(handle).await?;
            let facade = match idx_guard.as_mut() {
                Some(f) => f,
                None => {
                    return Ok(("Code index is being rebuilt, retry shortly.".to_string(), 0));
                }
            };

            if scope == Some("code") {
                if !handle.config.code.semantic_search.enabled {
                    return Err(mcp_error(
                        "Semantic code search is disabled. Enable it in mdkb.toml: [code.semantic_search] enabled = true, then re-index.",
                    ));
                }
                let code_limit = params.limit.min(5);
                let mut results = facade
                    .semantic_search(&params.query, code_limit, params.threshold)
                    .map_err(|e| {
                        tracing::error!("Semantic code search failed: {e}");
                        mcp_error(
                            "Semantic code search failed. The embedding model may not be installed — run `mdkb code index` to initialize.",
                        )
                    })?;

                if let Some(ref kind_str) = params.kind {
                    if let Ok(kind) = kind_str.parse::<crate::code::types::SymbolKind>() {
                        results.retain(|(s, _)| s.kind == kind);
                    } else {
                        return Err(mcp_error(format!(
                            "Unknown symbol kind: '{kind_str}'. Valid kinds: function, method, struct, enum, trait, interface, class, module, variable, constant, field, parameter, type_alias, macro"
                        )));
                    }
                }

                if results.is_empty() {
                    return Ok(("No semantic matches found.".to_string(), 0));
                }
                let mut out = format!("Found {} semantic match(es):\n\n", results.len());
                for (sym, score) in &results {
                    out.push_str(&format_symbol(sym));
                    out.push_str(&format!("    Similarity: {score:.3}\n"));
                    out.push('\n');
                }
                let count = results.len();
                Ok((out, count))
            } else {
                let mut symbols = if let Some(ref file_pattern) = params.file {
                    let mut results =
                        facade.find_symbols_by_file(file_pattern, params.limit * 2);
                    if !params.query.is_empty() && params.query != "*" {
                        let q = params.query.to_lowercase();
                        results.retain(|s| s.name.to_lowercase().contains(&q));
                    }
                    results
                } else {
                    facade.search_symbols(&params.query, params.limit)
                };

                if let Some(ref kind_str) = params.kind {
                    if let Ok(kind) = kind_str.parse::<crate::code::types::SymbolKind>() {
                        symbols.retain(|s| s.kind == kind);
                    } else {
                        return Err(mcp_error(format!(
                            "Unknown symbol kind: '{kind_str}'. Valid kinds: function, method, struct, enum, trait, interface, class, module, variable, constant, field, parameter, type_alias, macro"
                        )));
                    }
                }

                if symbols.is_empty() {
                    return Ok(("No symbols found.".to_string(), 0));
                }
                let rel_paths: Vec<String> = symbols
                    .iter()
                    .map(|s| s.file_path.to_string())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let token_map = facade.get_file_token_estimates(&rel_paths);
                let mut out = format!("Found {} symbol(s):\n\n", symbols.len());
                for sym in &symbols {
                    out.push_str(&format_symbol_with_file_tokens(
                        sym,
                        token_map.get(sym.file_path.as_ref()).copied(),
                    ));
                    out.push('\n');
                }
                let count = symbols.len();
                Ok((out, count))
            }
        }
        Some(invalid) => Err(mcp_error(format!(
            "Invalid scope: '{invalid}'. Valid: docs, memory, code, symbols."
        ))),
    }
}

/// `search` (cross-repo) — fan out across the registered handles, merge with
/// score-descending sort, and truncate to `params.limit`. Memory results from
/// each repo are formatted as a single pseudo-result for compatibility with
/// `SearchResult`-based aggregation.
///
/// Code/symbols scopes are rejected — those indexes are per-repo only.
pub async fn cross_repo_search_impl(
    handles: &[Arc<RepoHandle>],
    params: &SearchParams,
) -> Result<(String, usize), McpError> {
    if handles.is_empty() {
        return Err(mcp_error(
            "No repos registered. Waiting for MCP roots from client.",
        ));
    }

    let scope = params.scope.as_deref();
    let limit = params.limit.min(100);

    if matches!(scope, Some("code") | Some("symbols")) {
        return Err(mcp_error(
            "Cross-repo search is not supported for code/symbols scope. Specify a root.",
        ));
    }

    let mut all_results: Vec<SearchResult> = Vec::new();

    for handle in handles {
        ensure_handle_context(handle).await?;
        let ctx_guard = handle.ctx.lock().await;
        let ctx = match ctx_guard.as_ref() {
            Some(ctx) => ctx,
            None => continue,
        };

        let repo_tag = handle.root.display().to_string();

        match scope {
            Some("docs") | None => {
                let mut results = handle_hybrid_search(
                    ctx,
                    &params.query,
                    limit,
                    params.collection.as_deref(),
                    params.include_superseded,
                )
                .map_err(|e| mcp_error(format!("Search failed on {repo_tag}: {e}")))?;

                for r in &mut results {
                    r.repo_root = Some(repo_tag.clone());
                }
                all_results.extend(results);
            }
            Some("memory") => {
                let query_embedding = crate::llm::get_cached_service()
                    .and_then(|s| s.embed_query(&params.query))
                    .ok();
                let entries = memory::search_entries_hybrid(
                    &ctx.conn,
                    &params.query,
                    query_embedding.as_deref(),
                    limit,
                    handle.config.search.memory.access_recency_weight,
                    handle.config.search.memory.recency_half_life_secs,
                )
                .map_err(|e| mcp_error(format!("Memory search failed on {repo_tag}: {e}")))?;
                let entries = apply_min_confidence(entries, params.min_confidence);

                if !entries.is_empty() {
                    let text = format_memory_search_results(&entries);
                    let mut pseudo = SearchResult {
                        id: 0,
                        collection: "memory".to_string(),
                        path: String::new(),
                        title: None,
                        score: 1.0,
                        snippets: vec![text],
                        status: None,
                        superseded_by: None,
                        repo_root: Some(repo_tag),
                    };
                    if let Some(e) = entries.first() {
                        pseudo.path = e.id.clone();
                        pseudo.title = Some(e.title.clone());
                    }
                    all_results.push(pseudo);
                }
            }
            _ => {}
        }
    }

    all_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    all_results.truncate(limit);

    let output = if all_results.is_empty() {
        "No results across repos. Try broader terms.".to_string()
    } else {
        format_search_results(&all_results, limit)
    };

    let count = all_results.len();
    Ok((output, count))
}

/// Render a single document's content with optional line range and evolution
/// metadata. Mirrors `McpServer::get_document_content`. Truncation uses
/// `handle.config.mcp.max_response_tokens` for parity.
fn render_document_content(
    handle: &RepoHandle,
    ctx: &Context,
    doc: &crate::domain::Document,
    lines: Option<&str>,
) -> Result<String, McpError> {
    let content = documents::get_content(&ctx.conn, &doc.hash)
        .map_err(|e| mcp_error(format!("Failed to get content: {e}")))?
        .ok_or_else(|| mcp_error("Content missing for document. Try `update` to reindex."))?;

    let mut output = if let Some(range) = lines {
        apply_line_range(&content, range)?
    } else {
        content
    };

    if let Ok(Some((status, reason))) = evolution::get_document_status(&ctx.conn, doc.id) {
        let status_str = format!("{status:?}");
        if status_str != "Current" {
            output.push_str(&format!("\n\n---\n**Status:** {status_str}"));
            if let Some(r) = reason {
                output.push_str(&format!(" ({r})"));
            }
            if let Ok(descendants) = evolution::get_superseded_by(&ctx.conn, doc.id) {
                for evo in &descendants {
                    if let Ok(Some(source)) = documents::get_document(&ctx.conn, evo.source_doc_id)
                    {
                        output.push_str(&format!(
                            "\n**Superseded by:** {} ({})",
                            source.relative_path, evo.relationship
                        ));
                    }
                }
            }
        }
    }

    let max_tokens = handle.config.mcp.max_response_tokens;
    let output = if max_tokens > 0 {
        truncate_with_continuation(&output, max_tokens, doc.id).content
    } else {
        output
    };
    Ok(output)
}

/// `get` — comma-separated batch retrieval. Returns aggregated text and the
/// number of items found. Errors when no items resolved.
async fn get_batch_impl(
    handle: &RepoHandle,
    ids: &str,
    lines: Option<&str>,
) -> Result<(String, usize), McpError> {
    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    let mut output = String::new();
    let mut found = 0usize;

    for raw_id in ids.split(',') {
        let id = raw_id.trim();
        if id.is_empty() {
            continue;
        }

        if let Ok(numeric_id) = id.parse::<i64>() {
            if let Ok(Some(doc)) = documents::get_document(&ctx.conn, numeric_id) {
                match render_document_content(handle, ctx, &doc, lines) {
                    Ok(content) => {
                        let title = doc.title.as_deref().unwrap_or("(untitled)");
                        output.push_str(&format!(
                            "=== [{}] {} - {} ===\n{}\n\n",
                            doc.id, doc.relative_path, title, content
                        ));
                        found += 1;
                        continue;
                    }
                    Err(e) => {
                        output.push_str(&format!(
                            "=== [{}] {} ===\nContent error: {}\n\n",
                            doc.id, doc.relative_path, e
                        ));
                        found += 1;
                        continue;
                    }
                }
            }
        }

        if let Ok(doc) = resolve_document(&ctx.conn, id) {
            match render_document_content(handle, ctx, &doc, lines) {
                Ok(content) => {
                    let title = doc.title.as_deref().unwrap_or("(untitled)");
                    output.push_str(&format!(
                        "=== [{}] {} - {} ===\n{}\n\n",
                        doc.id, doc.relative_path, title, content
                    ));
                    found += 1;
                    continue;
                }
                Err(e) => {
                    output.push_str(&format!(
                        "=== [{}] {} ===\nContent error: {}\n\n",
                        doc.id, doc.relative_path, e
                    ));
                    found += 1;
                    continue;
                }
            }
        }

        if let Ok(Some(entry)) = memory::get_entry(&ctx.conn, id) {
            let ttl = format_ttl_info(entry.expires_at);
            output.push_str(&format!(
                "=== [MEM] {} - {}{} ===\n{}\n\n",
                entry.id, entry.title, ttl, entry.content
            ));
            found += 1;
            continue;
        }

        output.push_str(&format!("=== {id} ===\nNot found\n\n"));
    }

    if found == 0 {
        return Err(mcp_error("None of the requested items were found."));
    }
    Ok((output, found))
}

/// `get` — glob retrieval. Returns aggregated text, number of docs matched,
/// and a `truncated` flag indicating whether `max_response_tokens` clipped
/// the output.
async fn get_glob_impl(
    handle: &RepoHandle,
    pattern: &str,
) -> Result<(String, usize, bool), McpError> {
    let doc_limit = handle.config.mcp.max_document_tokens;
    let truncate_ellipsis = handle.config.mcp.truncate_with_ellipsis;
    let max_response_tokens = handle.config.mcp.max_response_tokens;

    let (output, result_count) = {
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;

        let results = handle_mget(ctx, pattern, None)
            .map_err(|e| mcp_error(format!("Glob retrieval failed: {e}")))?;

        if results.is_empty() {
            return Ok(("No documents matched pattern.".to_string(), 0, false));
        }

        let mut output = format!("Found {} documents:\n\n", results.len());
        for (doc, content) in &results {
            let title = doc.title.as_deref().unwrap_or("(untitled)");

            let truncated_content = if doc_limit > 0 {
                let content_tokens = count_tokens(content);
                if content_tokens > doc_limit {
                    if truncate_ellipsis {
                        truncate_with_ellipsis(content, doc_limit)
                    } else {
                        crate::metrics::tokens::truncate_to_tokens(content, doc_limit).0
                    }
                } else {
                    content.clone()
                }
            } else {
                content.clone()
            };

            output.push_str(&format!(
                "=== [{}] {} - {} ===\n{}\n\n",
                doc.id, doc.relative_path, title, truncated_content
            ));
        }
        let result_count = results.len();
        (output, result_count)
    };

    let original_len = output.len();
    let output = if max_response_tokens > 0 {
        crate::metrics::tokens::truncate_to_tokens(&output, max_response_tokens).0
    } else {
        output
    };
    let truncated = output.len() < original_len;
    Ok((output, result_count, truncated))
}

/// `get` — full implementation. Returns `(text, count, truncated)` so callers
/// can record metrics. Single-doc and memory-slug paths report `count=1` and
/// `truncated=false`. Batch and glob paths report their own counts and (for
/// glob) actual truncation status.
pub async fn get_impl(
    handle: &RepoHandle,
    params: &GetParams,
) -> Result<(String, usize, bool), McpError> {
    ensure_handle_context(handle).await?;

    let id = &params.id;

    if id.contains(',') {
        let (text, count) = get_batch_impl(handle, id, params.lines.as_deref()).await?;
        return Ok((text, count, false));
    }

    if id.contains('*') || id.contains('?') {
        return get_glob_impl(handle, id).await;
    }

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    if let Ok(numeric_id) = id.parse::<i64>() {
        if let Some(doc) = documents::get_document(&ctx.conn, numeric_id)
            .map_err(|e| mcp_error(format!("Failed to get document: {e}")))?
        {
            let output = render_document_content(handle, ctx, &doc, params.lines.as_deref())?;
            return Ok((output, 1, false));
        }
    }

    if id.contains('/') || id.contains('.') {
        if let Ok(doc) = resolve_document(&ctx.conn, id) {
            let output = render_document_content(handle, ctx, &doc, params.lines.as_deref())?;
            return Ok((output, 1, false));
        }
    }

    if let Ok(Some(entry)) = memory::get_entry(&ctx.conn, id) {
        if params.format.as_deref() == Some("history") {
            let revisions = memory::get_revisions(&ctx.conn, &entry.id).unwrap_or_default();
            let output = if revisions.is_empty() {
                format!("No revision history for '{}'", entry.id)
            } else {
                let mut parts = vec![format!(
                    "# Revision history for '{}' ({} revision{})\n",
                    entry.id,
                    revisions.len(),
                    if revisions.len() == 1 { "" } else { "s" }
                )];
                for (i, rev) in revisions.iter().enumerate() {
                    let date = chrono::DateTime::from_timestamp(rev.created_at, 0)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                        .unwrap_or_else(|| "?".to_string());
                    parts.push(format!(
                        "## Revision {} ({})\n```diff\n{}\n```",
                        i + 1,
                        date,
                        rev.diff
                    ));
                }
                parts.join("\n\n")
            };
            return Ok((output, 1, false));
        }

        let is_summary = params.format.as_deref() == Some("summary");
        let body = if is_summary {
            entry
                .content
                .split("\n\n")
                .next()
                .unwrap_or(&entry.content)
                .to_string()
        } else {
            entry.content.clone()
        };
        let conf = entry.confidence();
        let last_conf = entry
            .last_confirmed_at
            .map(|ts| {
                let days = (chrono::Utc::now().timestamp() - ts) as f64 / 86400.0;
                if days < 1.0 {
                    "today".to_string()
                } else {
                    format!("{}d ago", days as u64)
                }
            })
            .unwrap_or_else(|| "never".to_string());
        let conf_line = format!(
            "Confidence: {:.2} ({}↑, confirmed {}, source: {})",
            conf, entry.confirmations, last_conf, entry.source_type
        );

        let rev_line = memory::get_revision_summary(&ctx.conn, &entry.id)
            .map(|s| {
                if s.count == 0 {
                    return String::new();
                }
                let dates: Vec<String> = s
                    .dates
                    .iter()
                    .map(|&ts| {
                        chrono::DateTime::from_timestamp(ts, 0)
                            .map(|dt| dt.format("%Y-%m-%d").to_string())
                            .unwrap_or_else(|| "?".to_string())
                    })
                    .collect();
                format!(
                    "\nHistory: {} revision{} ({})",
                    s.count,
                    if s.count == 1 { "" } else { "s" },
                    dates.join(", ")
                )
            })
            .unwrap_or_default();

        let now = chrono::Utc::now().timestamp();
        let expired_marker = match entry.expires_at {
            Some(ts) if ts <= now => " [EXPIRED]",
            _ => "",
        };
        let ttl_line = match entry.expires_at {
            Some(ts) => {
                let dt = chrono::DateTime::from_timestamp(ts, 0)
                    .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
                    .unwrap_or_else(|| ts.to_string());
                format!("\nExpires: {dt}")
            }
            None => String::new(),
        };
        let output = format!(
            "# {}{} ({})\n\nType: {} | Status: {} | Tags: {}\nAccessed: {} times | {}{}{}\n\n{}",
            entry.title,
            expired_marker,
            entry.id,
            entry.entry_type,
            entry.status,
            if entry.tags.is_empty() {
                "none".to_string()
            } else {
                entry.tags.join(", ")
            },
            entry.access_count,
            conf_line,
            rev_line,
            ttl_line,
            body
        );
        return Ok((output, 1, false));
    }

    if let Ok(doc) = resolve_document(&ctx.conn, id) {
        let output = render_document_content(handle, ctx, &doc, params.lines.as_deref())?;
        return Ok((output, 1, false));
    }

    Err(mcp_error(format!("Not found: '{}'.", params.id)))
}

/// Dispatch a tool call by method name. Returns a JSON value — callers are
/// responsible for the transport envelope.
///
/// Unknown methods return an `McpError`; the JSON-RPC caller maps that into
/// a `-32601 Method not found` response.
pub async fn dispatch_call(
    tool_name: &str,
    params: Value,
    handle: Arc<RepoHandle>,
    dctx: &DispatchContext,
) -> Result<Value, McpError> {
    match tool_name {
        "status" => {
            let text = status_impl(&handle).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_status(tokens);
            dctx.record_persistent_call(&handle, "status", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_delete" => {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| mcp_error("memory_delete: missing 'id'"))?;
            let text = memory_delete_impl(&handle, id).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_delete", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_confirm" => {
            let id = params
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| mcp_error("memory_confirm: missing 'id'"))?;
            let outcome = params
                .get("outcome")
                .and_then(Value::as_str)
                .ok_or_else(|| mcp_error("memory_confirm: missing 'outcome'"))?;
            let text = memory_confirm_impl(&handle, id, outcome).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_confirm", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_write" => {
            let entry: MemoryWriteBatchEntry = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("memory_write: invalid params: {e}")))?;
            let text = memory_write_impl(&handle, &entry).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_write", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_write_batch" => {
            let entries_value = params
                .get("entries")
                .cloned()
                .ok_or_else(|| mcp_error("memory_write_batch: missing 'entries'"))?;
            let entries: Vec<MemoryWriteBatchEntry> = serde_json::from_value(entries_value)
                .map_err(|e| mcp_error(format!("memory_write_batch: invalid 'entries': {e}")))?;
            let (text, count) = memory_write_batch_impl(&handle, &entries).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_write_batch", tokens, count, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count }))
        }
        "search" => {
            let sp: SearchParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("search: invalid params: {e}")))?;
            if sp.root.as_deref() == Some("*") {
                return Err(mcp_error(
                    "Cross-repo search via dispatch_call is not supported (no registry handle).",
                ));
            }
            let (text, count) = search_impl(&handle, &sp).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_search(tokens, count);
            dctx.record_persistent_call(&handle, "search", tokens, count, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count }))
        }
        "get" => {
            let gp: GetParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("get: invalid params: {e}")))?;
            let (text, count, truncated) = get_impl(&handle, &gp).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_get(tokens);
            dctx.record_persistent_call(&handle, "get", tokens, count, truncated)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count, "truncated": truncated }))
        }
        "memory_list" => {
            let limit = params
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(20);
            let sort = params
                .get("sort")
                .and_then(Value::as_str)
                .unwrap_or("recent");
            let (text, count) = memory_list_impl(&handle, limit, sort).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_search(tokens, count);
            dctx.record_persistent_call(&handle, "memory_list", tokens, count, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens, "count": count }))
        }
        other => Err(McpError {
            code: ErrorCode::METHOD_NOT_FOUND,
            message: format!("unknown tool: {other}").into(),
            data: None,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;
    use tokio::sync::Mutex;

    fn make_handle(tmp: &TempDir) -> Arc<RepoHandle> {
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        Arc::new(RepoHandle::from_shared(
            root,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Config::default(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ))
    }

    fn make_dctx() -> DispatchContext {
        DispatchContext {
            metrics: Arc::new(UsageMetrics::new()),
            session_id: Arc::new(AtomicI64::new(0)),
            persistent_call_count: Arc::new(AtomicU64::new(0)),
            optimize_interval_calls: 200,
        }
    }

    #[tokio::test]
    async fn status_impl_returns_body_for_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let body = status_impl(&handle).await.expect("status impl");

        assert!(body.contains("## Index Status"), "body: {body}");
        assert!(body.contains("Documents:"), "body: {body}");
        assert!(body.contains("## Collections"), "body: {body}");
    }

    #[tokio::test]
    async fn dispatch_call_routes_status_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call("status", Value::Null, handle, &dctx)
            .await
            .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("## Index Status"), "result: {result}");
        assert!(
            result.get("tokens").and_then(Value::as_u64).unwrap_or(0) > 0,
            "tokens missing: {result}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_unknown_tool_is_method_not_found() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("no_such_tool", Value::Null, handle, &dctx)
            .await
            .expect_err("must error");

        assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
    }

    async fn seed_memory_entry(handle: &RepoHandle, id: &str) {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let now = chrono::Utc::now().timestamp();
        let entry = crate::store::memory::MemoryEntry {
            id: id.to_string(),
            title: format!("Title for {id}"),
            content: "Some content about the topic.".to_string(),
            entry_type: crate::store::memory::EntryType::Topic,
            tags: vec!["alpha".to_string()],
            status: crate::store::memory::EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: crate::store::memory::SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        crate::store::memory::add_entry(&ctx.conn, &entry).expect("seed entry");
    }

    #[tokio::test]
    async fn memory_delete_impl_reports_not_found_for_missing_id() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let out = memory_delete_impl(&handle, "does-not-exist")
            .await
            .expect("delete impl");

        assert!(out.contains("not found"), "output: {out}");
    }

    #[tokio::test]
    async fn memory_delete_impl_removes_seeded_entry() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "to-delete").await;

        let out = memory_delete_impl(&handle, "to-delete")
            .await
            .expect("delete impl");

        assert!(out.contains("Deleted memory entry"), "output: {out}");

        // Second call must report not found now.
        let again = memory_delete_impl(&handle, "to-delete")
            .await
            .expect("delete impl second");
        assert!(again.contains("not found"), "second: {again}");
    }

    #[tokio::test]
    async fn memory_confirm_impl_rejects_invalid_outcome() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "confirm-me").await;

        let err = memory_confirm_impl(&handle, "confirm-me", "maybe")
            .await
            .expect_err("must reject");
        assert!(
            err.message.contains("Invalid outcome"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn memory_confirm_impl_accepts_confirmed() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "confirm-me").await;

        let out = memory_confirm_impl(&handle, "confirm-me", "confirmed")
            .await
            .expect("confirm impl");
        assert!(!out.is_empty(), "output should be non-empty");
    }

    #[tokio::test]
    async fn memory_list_impl_returns_placeholder_when_empty() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let (text, count) = memory_list_impl(&handle, 20, "recent")
            .await
            .expect("list impl");

        assert_eq!(count, 0);
        assert!(text.contains("No memory entries"), "text: {text}");
    }

    #[tokio::test]
    async fn memory_list_impl_lists_seeded_entries() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "entry-a").await;
        seed_memory_entry(&handle, "entry-b").await;

        let (text, count) = memory_list_impl(&handle, 10, "recent")
            .await
            .expect("list impl");

        assert_eq!(count, 2, "text: {text}");
        assert!(text.contains("entry-a"), "text: {text}");
        assert!(text.contains("entry-b"), "text: {text}");
    }

    #[tokio::test]
    async fn memory_list_impl_rejects_invalid_sort() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let err = memory_list_impl(&handle, 10, "bogus")
            .await
            .expect_err("must reject");
        assert!(!err.message.is_empty());
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_delete() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "from-dispatch").await;

        let result = dispatch_call(
            "memory_delete",
            json!({ "id": "from-dispatch" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("Deleted memory entry"), "result: {result}");
    }

    #[tokio::test]
    async fn dispatch_call_memory_delete_missing_id_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("memory_delete", Value::Null, handle, &dctx)
            .await
            .expect_err("must error");
        assert!(err.message.contains("missing 'id'"), "msg: {}", err.message);
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_confirm() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "confirm-via-dispatch").await;

        let result = dispatch_call(
            "memory_confirm",
            json!({ "id": "confirm-via-dispatch", "outcome": "confirmed" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        assert!(result.get("text").is_some(), "result: {result}");
    }

    fn entry_input(id: &str) -> MemoryWriteBatchEntry {
        MemoryWriteBatchEntry {
            id: id.to_string(),
            title: format!("Title {id}"),
            content: format!("Content for {id}"),
            entry_type: "topic".to_string(),
            tags: vec!["t".to_string()],
            source_type: "user_statement".to_string(),
            ttl: None,
            due_in: None,
        }
    }

    #[tokio::test]
    async fn memory_write_impl_creates_then_updates() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let created = memory_write_impl(&handle, &entry_input("w-1"))
            .await
            .expect("write impl");
        assert!(created.starts_with("Created memory entry: w-1"), "out: {created}");

        let mut second = entry_input("w-1");
        second.content = "Updated content body".to_string();
        let updated = memory_write_impl(&handle, &second)
            .await
            .expect("write impl update");
        assert!(updated.starts_with("Updated memory entry: w-1"), "out: {updated}");
    }

    #[tokio::test]
    async fn memory_write_batch_impl_rejects_empty() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let err = memory_write_batch_impl(&handle, &[])
            .await
            .expect_err("must reject");
        assert!(err.message.contains("must not be empty"), "msg: {}", err.message);
    }

    #[tokio::test]
    async fn memory_write_batch_impl_rejects_over_limit() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let entries: Vec<_> = (0..21).map(|i| entry_input(&format!("b-{i}"))).collect();

        let err = memory_write_batch_impl(&handle, &entries)
            .await
            .expect_err("must reject");
        assert!(err.message.contains("max 20 entries"), "msg: {}", err.message);
    }

    #[tokio::test]
    async fn memory_write_batch_impl_writes_all_entries() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let mut a = entry_input("b-a");
        a.title = "Authentication flow notes".to_string();
        a.content = "OAuth2 PKCE token rotation strategy".to_string();
        let mut b = entry_input("b-b");
        b.title = "Database migration runbook".to_string();
        b.content = "Postgres logical replication failover steps".to_string();
        let mut c = entry_input("b-c");
        c.title = "Frontend build config".to_string();
        c.content = "Vite chunk splitting and tree shaking knobs".to_string();

        let (text, count) = memory_write_batch_impl(&handle, &[a, b, c])
            .await
            .expect("batch impl");

        assert_eq!(count, 3);
        for id in ["b-a", "b-b", "b-c"] {
            assert!(text.contains(id), "missing {id} in: {text}");
        }
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_write() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call(
            "memory_write",
            json!({
                "id": "via-dispatch",
                "title": "Disp Title",
                "content": "Disp content body",
                "entry_type": "topic",
                "tags": [],
                "source_type": "user_statement"
            }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("Created memory entry: via-dispatch"), "result: {result}");
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_write_batch() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call(
            "memory_write_batch",
            json!({
                "entries": [
                    {
                        "id": "bd-1",
                        "title": "T1",
                        "content": "Body 1 content",
                        "entry_type": "topic",
                        "tags": [],
                        "source_type": "user_statement"
                    },
                    {
                        "id": "bd-2",
                        "title": "T2",
                        "content": "Body 2 content",
                        "entry_type": "topic",
                        "tags": [],
                        "source_type": "user_statement"
                    }
                ]
            }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        assert_eq!(result.get("count").and_then(Value::as_u64).unwrap_or(0), 2);
    }

    #[tokio::test]
    async fn dispatch_call_memory_write_batch_missing_entries_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("memory_write_batch", json!({}), handle, &dctx)
            .await
            .expect_err("must error");
        assert!(err.message.contains("'entries'"), "msg: {}", err.message);
    }

    #[tokio::test]
    async fn dispatch_call_routes_memory_list() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "listed").await;

        let result = dispatch_call("memory_list", json!({}), handle, &dctx)
            .await
            .expect("dispatch");

        assert_eq!(
            result.get("count").and_then(Value::as_u64).unwrap_or(0),
            1,
            "result: {result}"
        );
        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("listed"), "text: {text}");
    }

    fn search_params(query: &str, scope: Option<&str>) -> SearchParams {
        SearchParams {
            query: query.to_string(),
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: scope.map(str::to_string),
            kind: None,
            threshold: 0.5,
            file: None,
            min_confidence: None,
        }
    }

    #[tokio::test]
    async fn search_impl_rejects_invalid_scope() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("anything", Some("bogus"));

        let err = search_impl(&handle, &params).await.expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Invalid scope"), "msg: {msg}");
        assert!(msg.contains("bogus"), "msg: {msg}");
    }

    #[tokio::test]
    async fn search_impl_docs_scope_returns_empty_for_no_docs() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("nothing matches", Some("docs"));

        let (text, count) = search_impl(&handle, &params).await.expect("docs scope");
        assert_eq!(count, 0, "expected zero, text: {text}");
    }

    #[tokio::test]
    async fn search_impl_memory_scope_returns_empty_for_no_entries() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("anything", Some("memory"));

        let (_text, count) = search_impl(&handle, &params).await.expect("memory scope");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn search_impl_symbols_scope_returns_empty_for_no_index() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = search_params("anything", Some("symbols"));

        let (text, count) = search_impl(&handle, &params)
            .await
            .expect("symbols scope");
        assert_eq!(count, 0, "text: {text}");
        assert!(text.contains("No symbols found"), "text: {text}");
    }

    #[tokio::test]
    async fn cross_repo_search_impl_rejects_empty_handles() {
        let params = search_params("anything", None);
        let err = cross_repo_search_impl(&[], &params)
            .await
            .expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("No repos registered"), "msg: {msg}");
    }

    #[tokio::test]
    async fn dispatch_call_search_rejects_cross_repo() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call(
            "search",
            json!({ "query": "x", "root": "*" }),
            handle,
            &dctx,
        )
        .await
        .expect_err("should error");
        let msg = err.to_string();
        assert!(
            msg.contains("Cross-repo search via dispatch_call is not supported"),
            "msg: {msg}"
        );
    }

    fn get_params(id: &str) -> GetParams {
        GetParams {
            id: id.to_string(),
            root: None,
            lines: None,
            format: None,
        }
    }

    #[tokio::test]
    async fn get_impl_unknown_id_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = get_params("does-not-exist");

        let err = get_impl(&handle, &params).await.expect_err("should error");
        assert!(err.to_string().contains("Not found"), "msg: {err}");
    }

    #[tokio::test]
    async fn get_impl_glob_returns_no_match_for_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = get_params("**/*.md");

        let (text, count, truncated) =
            get_impl(&handle, &params).await.expect("glob get");
        assert_eq!(count, 0, "text: {text}");
        assert!(!truncated);
        assert!(text.contains("No documents matched pattern"), "text: {text}");
    }

    #[tokio::test]
    async fn get_impl_batch_errors_when_no_items_resolve() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = get_params("missing-1, missing-2");

        let err = get_impl(&handle, &params).await.expect_err("should error");
        assert!(
            err.to_string()
                .contains("None of the requested items were found"),
            "msg: {err}"
        );
    }

    #[tokio::test]
    async fn get_impl_returns_seeded_memory_entry() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "seeded-get").await;

        let params = get_params("seeded-get");
        let (text, count, truncated) = get_impl(&handle, &params).await.expect("memory get");
        assert_eq!(count, 1);
        assert!(!truncated);
        assert!(text.contains("seeded-get"), "text: {text}");
        assert!(text.contains("Type:"), "text: {text}");
    }

    #[tokio::test]
    async fn dispatch_call_routes_get_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_memory_entry(&handle, "dispatched-get").await;

        let result = dispatch_call(
            "get",
            json!({ "id": "dispatched-get" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        assert_eq!(
            result.get("count").and_then(Value::as_u64).unwrap_or(0),
            1,
            "result: {result}"
        );
        assert_eq!(
            result.get("truncated").and_then(Value::as_bool),
            Some(false),
            "result: {result}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_get_missing_id_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("get", json!({}), handle, &dctx)
            .await
            .expect_err("should error");
        assert!(
            err.to_string().contains("get: invalid params"),
            "msg: {err}"
        );
    }

    #[tokio::test]
    async fn dispatch_call_routes_search_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call(
            "search",
            json!({ "query": "anything", "scope": "docs" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch");

        assert_eq!(
            result.get("count").and_then(Value::as_u64).unwrap_or(99),
            0,
            "result: {result}"
        );
        assert!(result.get("tokens").is_some(), "result: {result}");
    }
}
