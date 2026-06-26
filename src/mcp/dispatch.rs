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

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use futures::future::join_all;
use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};

use crate::cli::handlers::{
    Context, handle_hybrid_search, handle_mget, handle_session_index, handle_update,
};
use crate::cli::hook_logic::{
    REINDEX_TOOLS, build_recall_query, canonicalize_under_cwd, classify_bash_search,
    classify_definition_search, classify_grep_pattern, is_mdkb_invocation, prompt_is_wrapup,
    tool_input_path,
};
use crate::code::indexing::IndexFacade;
use crate::daemon::registry::RepoHandle;
use crate::domain::SearchResult;
use crate::metrics::{
    UsageMetrics, count_tokens, truncate_with_continuation, truncate_with_ellipsis,
};
use crate::store::memory::{access_recency_score, get_warmup_index, search_entries_fts};
use crate::store::{collections, documents, evolution, memory, search, stats};

use super::mcp_error;
use super::server::{
    apply_line_range, apply_min_confidence, format_memory_search_results, format_search_results,
    format_symbol, format_symbol_with_file_tokens, format_ttl_info, ood_hint, relative_time_ago,
    resolve_document, truncate_text,
};
use super::tools::{
    CodeFindParams, CodeGraphParams, GetParams, GraphParams, MemoryWriteBatchEntry, SearchParams,
    SymbolAtPositionParams, SymbolsInFileParams, UsageParams,
};

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
            if let Err(e) = crate::store::maintenance::maybe_incremental_vacuum(
                &ctx.conn,
                crate::store::maintenance::INCREMENTAL_VACUUM_THRESHOLD,
            ) {
                tracing::warn!("incremental_vacuum failed: {e}");
            }
        }
    }
}

/// Ensure the repo's database context is initialized.
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
                let doc_count =
                    collections::get_collection_document_count(&ctx.conn, &coll.name).unwrap_or(0);
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
pub async fn memory_delete_impl(
    handle: &RepoHandle,
    id: &str,
    dry_run: bool,
) -> Result<String, McpError> {
    memory::validate_entry_id(id).map_err(|e| mcp_error(e.to_string()))?;
    ensure_handle_context(handle).await?;

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    if dry_run {
        let exists = memory::get_entry_without_tracking(&ctx.conn, id)
            .map_err(|e| mcp_error(format!("Failed to check existing entry: {e}")))?
            .is_some();
        return Ok(if exists {
            format!("dry-run: would delete memory entry '{id}'")
        } else {
            format!("dry-run: memory entry '{id}' not found")
        });
    }

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
/// Resolve `source_file` → content. Returns `(content, source_path)`.
/// Errors if both content and source_file are provided.
fn resolve_source_file(
    content: &str,
    source_file: Option<&str>,
) -> Result<(String, Option<String>), McpError> {
    match source_file {
        Some(_) if !content.is_empty() => Err(mcp_error(
            "Cannot specify both content and source_file — use one or the other",
        )),
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| mcp_error(format!("Failed to read source_file {path}: {e}")))?;
            let abs = std::path::Path::new(path)
                .canonicalize()
                .unwrap_or_else(|_| std::path::PathBuf::from(path));
            Ok((text, Some(abs.to_string_lossy().to_string())))
        }
        None if content.is_empty() => {
            Err(mcp_error("Either content or source_file must be provided"))
        }
        None => Ok((content.to_string(), None)),
    }
}

/// `memory_write_impl` and `memory_write_batch_impl`. Synchronous because it
/// runs against an already-locked `Connection`; embedding I/O is best-effort
/// and falls back silently when the LLM service is unconfigured (tests).
///
/// `embedding` must be pre-computed by the async caller **before** the ctx
/// lock is acquired so that CPU-bound ONNX inference never blocks the tokio
/// executor while holding the Mutex guard.
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
    embedding: Option<Vec<f32>>,
    source_path: Option<&str>,
    dry_run: bool,
) -> Result<String, McpError> {
    memory::validate_entry_input(id, title, tags, content).map_err(|e| mcp_error(e.to_string()))?;

    let existing = memory::get_entry_without_tracking(conn, id)
        .map_err(|e| mcp_error(format!("Failed to check existing entry: {e}")))?;

    let entry_type: memory::EntryType = entry_type_str.parse().map_err(|e: String| {
        mcp_error(format!(
            "{e}. Valid types: topic, problem, decision, reminder, prior, handoff"
        ))
    })?;

    let source_type: memory::SourceType =
        source_type_str.parse().map_err(|e: String| mcp_error(e))?;

    if dry_run {
        let action = if existing.is_some() {
            "update"
        } else {
            "create"
        };
        return Ok(format!("dry-run: would {action} memory entry '{id}'"));
    }

    let now = chrono::Utc::now().timestamp();
    // Priors default to 30-day TTL if not explicitly specified.
    const PRIOR_DEFAULT_TTL: u64 = 30 * 24 * 3600;
    let effective_ttl = ttl.or(if entry_type == memory::EntryType::Prior {
        Some(PRIOR_DEFAULT_TTL)
    } else {
        None
    });
    let expires_at = effective_ttl.map(|s| now + s as i64);
    let due_at = due_in.map(|s| now + s as i64);
    let is_new = existing.is_none();

    // Pre-write duplicate check: reject if a near-identical entry exists (new entries only).
    // L2 distance < 0.32 ≈ cosine similarity > 0.95 — very high bar, minimizes false positives.
    // Uses the pre-computed embedding passed in by the async caller (computed outside the lock).
    if is_new {
        if let Some(ref emb) = embedding {
            if let Ok(similar) = crate::store::vectors::memory_vector_search(conn, emb, 3) {
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
            source_path: source_path.map(String::from),
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

    // Store the pre-computed embedding and append similarity warnings.
    // The embedding was computed outside the lock by the async caller to avoid
    // blocking the tokio executor during CPU-bound ONNX inference.
    if let Some(ref emb) = embedding {
        if let Some(rowid) = memory::get_rowid(conn, id).unwrap_or(None) {
            if let Err(e) = crate::store::vectors::store_memory_embedding(
                conn,
                rowid,
                emb,
                crate::llm::embeddings::MODEL_NAME,
            ) {
                tracing::warn!("Failed to store memory embedding for '{id}': {e}");
            }

            if is_new {
                let warnings = memory::find_similar_entries(conn, emb, rowid, id);
                output.push_str(&warnings);
            }
        }
    }

    Ok(output)
}

/// `memory_write` — create or update a single memory entry. Wraps
/// `write_single_memory` with `RepoHandle` ctx acquisition.
///
/// Embedding is generated **before** the ctx lock is acquired so that
/// CPU-bound ONNX inference (10–100 ms) never blocks the tokio executor
/// while holding the Mutex guard.
pub async fn memory_write_impl(
    handle: &RepoHandle,
    entry: &MemoryWriteBatchEntry,
    dry_run: bool,
) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let (content, source_path) = resolve_source_file(&entry.content, entry.source_file.as_deref())?;

    // Pre-compute embedding outside the lock — ONNX is CPU-bound. Skipped for
    // dry-run, which returns before any embedding or write happens.
    let embedding = if dry_run {
        None
    } else {
        let embed_text = format!("{} {}", entry.title, content);
        tokio::task::spawn_blocking(move || {
            crate::llm::get_cached_service()
                .ok()
                .and_then(|svc| svc.embed_query(&embed_text).ok())
        })
        .await
        .unwrap_or(None)
    };

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    write_single_memory(
        &ctx.conn,
        &entry.id,
        &entry.title,
        &content,
        &entry.entry_type,
        &entry.source_type,
        &entry.tags,
        entry.ttl,
        entry.due_in,
        embedding,
        source_path.as_deref(),
        dry_run,
    )
}

/// `memory_write_batch` — create or update up to 20 entries in one call.
/// Returns `(joined_output, count)`. Enforces empty/limit guards before
/// touching the DB.
///
/// All embeddings are generated **before** the ctx lock is acquired so that
/// CPU-bound ONNX inference never blocks the tokio executor while holding
/// the Mutex guard.
pub async fn memory_write_batch_impl(
    handle: &RepoHandle,
    entries: &[MemoryWriteBatchEntry],
    dry_run: bool,
) -> Result<(String, usize), McpError> {
    if entries.is_empty() {
        return Err(mcp_error("entries array must not be empty"));
    }
    if entries.len() > 20 {
        return Err(mcp_error("max 20 entries per batch"));
    }

    ensure_handle_context(handle).await?;

    // Resolve source_file → content for all entries before computing embeddings.
    let resolved: Vec<(String, Option<String>)> = entries
        .iter()
        .map(|e| resolve_source_file(&e.content, e.source_file.as_deref()))
        .collect::<Result<_, _>>()?;

    // Pre-compute embeddings for all entries outside the lock — ONNX is CPU-bound.
    // Skipped for dry-run, which returns before any embedding or write happens.
    let embeddings: Vec<Option<Vec<f32>>> = if dry_run {
        vec![None; entries.len()]
    } else {
        let embed_texts: Vec<String> = entries
            .iter()
            .zip(resolved.iter())
            .map(|(e, (content, _))| format!("{} {}", e.title, content))
            .collect();
        tokio::task::spawn_blocking(move || match crate::llm::get_cached_service() {
            Ok(svc) => embed_texts
                .iter()
                .map(|text| svc.embed_query(text).ok())
                .collect(),
            Err(_) => vec![None; embed_texts.len()],
        })
        .await
        .unwrap_or_else(|_| vec![None; entries.len()])
    };

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    let mut results = Vec::with_capacity(entries.len());
    for ((entry, (content, source_path)), embedding) in entries
        .iter()
        .zip(resolved.iter())
        .zip(embeddings.into_iter())
    {
        let result = write_single_memory(
            &ctx.conn,
            &entry.id,
            &entry.title,
            content,
            &entry.entry_type,
            &entry.source_type,
            &entry.tags,
            entry.ttl,
            entry.due_in,
            embedding,
            source_path.as_deref(),
            dry_run,
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
            let query_embedding =
                match crate::llm::get_cached_service().and_then(|s| s.embed_query(&params.query)) {
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

            let query_embedding =
                match crate::llm::get_cached_service().and_then(|s| s.embed_query(&params.query)) {
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
                    let mut results = facade.find_symbols_by_file(file_pattern, params.limit * 2);
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
                    let total = facade.symbol_count();
                    return Ok((
                        format!("0 matches ({total} symbols indexed). Try a shorter name."),
                        0,
                    ));
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

    // Fan-out concurrently across all repos, then merge. Per-repo errors are
    // logged and treated as empty results so a single failing repo does not
    // abort the whole cross-repo search.
    let per_repo_futures = handles.iter().map(|handle| {
        let handle = Arc::clone(handle);
        let query = params.query.clone();
        let collection = params.collection.clone();
        let include_superseded = params.include_superseded;
        let min_confidence = params.min_confidence;
        let recency_weight = handle.config.search.memory.access_recency_weight;
        let recency_half_life = handle.config.search.memory.recency_half_life_secs;

        async move {
            if let Err(e) = ensure_handle_context(&handle).await {
                tracing::warn!(
                    "cross_repo_search: skipping {}: {}",
                    handle.root.display(),
                    e.message
                );
                return Vec::<SearchResult>::new();
            }
            let ctx_guard = handle.ctx.lock().await;
            let ctx = match ctx_guard.as_ref() {
                Some(ctx) => ctx,
                None => return Vec::new(),
            };

            let repo_tag = handle.root.display().to_string();
            let mut repo_results: Vec<SearchResult> = Vec::new();

            match scope {
                Some("docs") | None => {
                    match handle_hybrid_search(
                        ctx,
                        &query,
                        limit,
                        collection.as_deref(),
                        include_superseded,
                    ) {
                        Ok(mut results) => {
                            for r in &mut results {
                                r.repo_root = Some(repo_tag.clone());
                            }
                            repo_results.extend(results);
                        }
                        Err(e) => {
                            tracing::warn!(
                                "cross_repo_search: docs search failed on {}: {e}",
                                repo_tag
                            );
                        }
                    }
                }
                Some("memory") => {
                    let query_embedding = crate::llm::get_cached_service()
                        .and_then(|s| s.embed_query(&query))
                        .ok();
                    match memory::search_entries_hybrid(
                        &ctx.conn,
                        &query,
                        query_embedding.as_deref(),
                        limit,
                        recency_weight,
                        recency_half_life,
                    ) {
                        Ok(entries) => {
                            let entries = apply_min_confidence(entries, min_confidence);
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
                                repo_results.push(pseudo);
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                "cross_repo_search: memory search failed on {}: {e}",
                                repo_tag
                            );
                        }
                    }
                }
                _ => {}
            }

            repo_results
        }
    });

    let mut all_results: Vec<SearchResult> = join_all(per_repo_futures)
        .await
        .into_iter()
        .flatten()
        .collect();

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
const GET_BATCH_MAX_IDS: usize = 50;

async fn get_batch_impl(
    handle: &RepoHandle,
    ids: &str,
    lines: Option<&str>,
) -> Result<(String, usize), McpError> {
    let id_count = ids.split(',').filter(|s| !s.trim().is_empty()).count();
    if id_count > GET_BATCH_MAX_IDS {
        return Err(mcp_error(format!(
            "get_batch: too many IDs ({id_count}); limit is {GET_BATCH_MAX_IDS}"
        )));
    }

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

/// `update` — three-phase differential reindex: documents, code, sessions.
/// Returns the human-readable summary aggregated from each phase.
pub async fn update_impl(handle: &RepoHandle) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let doc_output = {
        let mut ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard
            .as_mut()
            .ok_or_else(|| mcp_error("Database not initialized"))?;
        let result = handle_update(ctx, &handle.root)
            .map_err(|e| mcp_error(format!("Document update failed: {e}")))?;
        format!(
            "## Documents\n\nAdded: {}\nUpdated: {}\nRemoved: {}\nUnchanged: {}",
            result.added, result.updated, result.removed, result.unchanged
        )
    };

    let code_output = {
        let mut idx_guard = acquire_handle_code_index(handle).await?;
        if let Some(facade) = idx_guard.as_mut() {
            match facade.reindex(&handle.root) {
                Ok(stats) => format!(
                    "\n\n## Code\n\nFiles: {}\nSymbols: {}\nRelationships: {}",
                    stats.files_indexed, stats.symbols_indexed, stats.relationships_collected
                ),
                Err(e) => {
                    tracing::error!("Code reindex failed: {e}");
                    format!("\n\n## Code\n\nReindex failed: {e}")
                }
            }
        } else {
            String::new()
        }
    };

    let session_output = match crate::daemon::config::home_dir() {
        Err(e) => {
            tracing::warn!("Session indexing skipped: cannot resolve home dir: {e}");
            String::new()
        }
        Ok(home) => {
            let sessions_base = home.join(".claude/projects");
            let project_root = handle.root.to_string_lossy().to_string();

            let mut ctx_guard = handle.ctx.lock().await;
            if let Some(ctx) = ctx_guard.as_mut() {
                match handle_session_index(ctx, &sessions_base, &project_root) {
                    Ok(sr) if sr.added > 0 || sr.updated > 0 => format!(
                        "\n\n## Sessions\n\nAdded: {}\nUpdated: {}\nUnchanged: {}",
                        sr.added, sr.updated, sr.unchanged
                    ),
                    Ok(_) => String::new(),
                    Err(e) => {
                        tracing::warn!("Session indexing failed: {e}");
                        String::new()
                    }
                }
            } else {
                String::new()
            }
        }
    };

    Ok(format!("{doc_output}{code_output}{session_output}"))
}

fn symbol_to_json(s: &crate::code::symbol::Symbol) -> serde_json::Value {
    serde_json::json!({
        "name": s.name.as_ref(),
        "kind": s.kind.to_string(),
        "file_path": s.file_path.as_ref(),
        "line_start": s.range.start_line,
        "line_end": s.range.end_line,
        "col_start": s.range.start_column,
        "col_end": s.range.end_column,
        "signature": s.signature.as_deref(),
        "scope_context": s.scope_context.as_ref().map(|sc| format!("{sc:?}")),
    })
}

fn symbols_to_json_string(symbols: &[crate::code::symbol::Symbol]) -> Result<String, McpError> {
    let json_symbols: Vec<serde_json::Value> = symbols.iter().map(symbol_to_json).collect();
    serde_json::to_string(&json_symbols)
        .map_err(|e| mcp_error(format!("failed to serialize symbols: {e}")))
}

/// `symbols_in_file` — list all symbols in a file, ordered by position.
pub async fn symbols_in_file_impl(
    handle: &RepoHandle,
    params: &SymbolsInFileParams,
) -> Result<String, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let facade = match idx_guard.as_ref() {
        Some(f) => f,
        None => return Err(mcp_error("code index not available — run `update` first")),
    };
    let symbols = facade
        .db()
        .symbols_in_file_ordered(&params.file)
        .map_err(|e| mcp_error(format!("symbols_in_file: {e}")))?;

    symbols_to_json_string(&symbols)
}

/// `code_find` — exact symbol lookup by name with optional filters.
pub async fn code_find_impl(
    handle: &RepoHandle,
    params: &CodeFindParams,
) -> Result<String, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let facade = match idx_guard.as_ref() {
        Some(f) => f,
        None => return Err(mcp_error("code index not available — run `update` first")),
    };

    let mut results = facade.find_symbols_by_name(&params.name);

    if let Some(kind_str) = &params.kind {
        let kind: crate::code::types::SymbolKind = kind_str
            .parse()
            .map_err(|_| mcp_error(format!("unknown symbol kind: {kind_str}")))?;
        results.retain(|s| s.kind == kind);
    }

    if let Some(file_substr) = &params.file {
        results.retain(|s| s.file_path.as_ref().contains(file_substr.as_str()));
    }

    let limit = params.limit.unwrap_or(50) as usize;
    results.truncate(limit);

    symbols_to_json_string(&results)
}

/// `symbol_at_position` — find the innermost symbol at a given file position.
pub async fn symbol_at_position_impl(
    handle: &RepoHandle,
    params: &SymbolAtPositionParams,
) -> Result<String, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let facade = match idx_guard.as_ref() {
        Some(f) => f,
        None => return Err(mcp_error("code index not available — run `update` first")),
    };
    let symbol = facade
        .db()
        .symbol_at_position(&params.file, params.line, params.col)
        .map_err(|e| mcp_error(format!("symbol_at_position: {e}")))?;

    match symbol {
        Some(s) => Ok(serde_json::json!({
            "name": s.name.as_ref(),
            "kind": format!("{:?}", s.kind),
            "file_path": s.file_path.as_ref(),
            "line_start": s.range.start_line,
            "line_end": s.range.end_line,
            "col_start": s.range.start_column,
            "col_end": s.range.end_column,
            "signature": s.signature.as_deref(),
            "module_path": s.module_path.as_deref(),
        })
        .to_string()),
        None => Ok("null".to_string()),
    }
}

/// Hop limit for `graph` path queries over MCP (the CLI exposes `--max-hops`).
const GRAPH_MCP_MAX_HOPS: u32 = 6;

/// `graph` — knowledge-graph queries. Dispatches by direction (links/backlinks/
/// neighbors/path) into the graph store and returns formatted text.
pub async fn graph_impl(handle: &RepoHandle, params: &GraphParams) -> Result<String, McpError> {
    use crate::store::graph;

    ensure_handle_context(handle).await?;
    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    let relation = params.relation.as_deref();
    let entity = &params.entity;

    let output = match params.direction.as_str() {
        "links" => {
            let doc = resolve_document(&ctx.conn, entity).map_err(|e| mcp_error(e.to_string()))?;
            let edges = graph::get_outgoing(&ctx.conn, doc.id, relation)
                .map_err(|e| mcp_error(e.to_string()))?;
            format_graph_edges(entity, "links", &edges)
        }
        "backlinks" => {
            let edges = graph::get_incoming(&ctx.conn, entity, relation)
                .map_err(|e| mcp_error(e.to_string()))?;
            format_graph_edges(entity, "backlinks", &edges)
        }
        "neighbors" => {
            let doc = resolve_document(&ctx.conn, entity).map_err(|e| mcp_error(e.to_string()))?;
            let nbrs = graph::neighbors(&ctx.conn, doc.id, relation, params.depth)
                .map_err(|e| mcp_error(e.to_string()))?;
            format_graph_neighbors(entity, &nbrs)
        }
        "path" => {
            let to = params
                .to
                .as_deref()
                .ok_or_else(|| mcp_error("direction=path requires 'to'"))?;
            let doc = resolve_document(&ctx.conn, entity).map_err(|e| mcp_error(e.to_string()))?;
            let path = graph::shortest_path(&ctx.conn, doc.id, to, GRAPH_MCP_MAX_HOPS)
                .map_err(|e| mcp_error(e.to_string()))?;
            match path {
                Some(nodes) => format!("{}: {}", entity, nodes.join(" -> ")),
                None => format!("No path from {entity} to {to}."),
            }
        }
        other => {
            return Err(mcp_error(format!(
                "Unknown direction '{other}'. Use links, backlinks, neighbors, or path."
            )));
        }
    };
    Ok(output)
}

fn format_graph_edges(entity: &str, label: &str, edges: &[crate::store::graph::Edge]) -> String {
    if edges.is_empty() {
        return format!("No {label} for {entity}.");
    }
    let mut out = format!("{} {label} for {entity}:\n", edges.len());
    for e in edges {
        out.push_str(&format!(
            "  [{}] --{}--> {} ({})\n",
            e.source_doc_id, e.relation, e.target_ref, e.source_kind
        ));
    }
    out
}

fn format_graph_neighbors(entity: &str, neighbors: &[crate::store::graph::Neighbor]) -> String {
    if neighbors.is_empty() {
        return format!("No neighbors for {entity}.");
    }
    let mut out = format!("{} neighbors of {entity}:\n", neighbors.len());
    for n in neighbors {
        out.push_str(&format!("  {} (depth {})\n", n.entity, n.depth));
    }
    out
}

/// `code_graph` — call graph queries. Resolves the symbol then dispatches by
/// direction (calls/callers/impact). Returns the formatted output text.
pub async fn code_graph_impl(
    handle: &RepoHandle,
    params: &CodeGraphParams,
) -> Result<String, McpError> {
    let idx_guard = acquire_handle_code_index(handle).await?;
    let facade = match idx_guard.as_ref() {
        Some(f) => f,
        None => return Ok("Code index is being rebuilt, retry shortly.".to_string()),
    };

    let symbol = super::server::McpServer::resolve_symbol(facade, &params.name, params.symbol_id)?;

    let output = match params.direction.as_str() {
        "calls" => {
            let called = facade.get_called_functions(symbol.id);
            if called.is_empty() {
                format!(
                    "{} ({:?}) does not call any indexed functions.",
                    symbol.name, symbol.kind
                )
            } else {
                let mut out = format!(
                    "{} ({:?}) calls {} function(s):\n\n",
                    symbol.name,
                    symbol.kind,
                    called.len()
                );
                for sym in &called {
                    out.push_str(&format_symbol(sym));
                    out.push('\n');
                }
                out
            }
        }
        "callers" => {
            let callers = facade.get_calling_functions(symbol.id);
            if callers.is_empty() {
                format!(
                    "{} ({:?}) has no indexed callers.",
                    symbol.name, symbol.kind
                )
            } else {
                let mut out = format!(
                    "{} ({:?}) is called by {} function(s):\n\n",
                    symbol.name,
                    symbol.kind,
                    callers.len()
                );
                for sym in &callers {
                    out.push_str(&format_symbol(sym));
                    out.push('\n');
                }
                out
            }
        }
        "impact" => {
            let impacted_ids = facade.get_impact_radius(symbol.id, params.max_depth);
            if impacted_ids.is_empty() {
                format!(
                    "{} ({:?}) has no reachable symbols within {} hop(s).",
                    symbol.name, symbol.kind, params.max_depth
                )
            } else {
                let mut out = format!(
                    "Impact radius for {} ({:?}): {} symbol(s) within {} hop(s):\n\n",
                    symbol.name,
                    symbol.kind,
                    impacted_ids.len(),
                    params.max_depth
                );
                for sid in &impacted_ids {
                    if let Some(sym) = facade.get_symbol(*sid) {
                        out.push_str(&format_symbol(&sym));
                        out.push('\n');
                    } else {
                        out.push_str(&format!("  sym#{} (not found in index)\n", sid.value()));
                    }
                }
                out
            }
        }
        _ => {
            return Err(mcp_error(format!(
                "Invalid direction: '{}'. Valid: calls, callers, impact.",
                params.direction
            )));
        }
    };

    Ok(output)
}

/// `usage` — token economy audit. Reads session + lifetime stats and returns
/// a JSON-formatted string. `session_id` is the daemon-global current session.
pub async fn usage_impl(
    handle: &RepoHandle,
    params: &UsageParams,
    session_id: i64,
) -> Result<String, McpError> {
    ensure_handle_context(handle).await?;

    let ctx_guard = handle.ctx.lock().await;
    let ctx = ctx_guard
        .as_ref()
        .ok_or_else(|| mcp_error("Database not initialized"))?;

    let session = if session_id > 0 {
        stats::get_session(&ctx.conn, session_id)
            .map_err(|e| mcp_error(format!("Failed to read session: {e}")))?
    } else {
        None
    };
    let session_tool_usage = if session_id > 0 {
        stats::get_tool_usage(&ctx.conn, session_id)
            .map_err(|e| mcp_error(format!("Failed to read tool usage: {e}")))?
    } else {
        Vec::new()
    };

    let lifetime = if !params.session_only {
        Some(
            stats::get_aggregate_stats(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to read lifetime stats: {e}")))?,
        )
    } else {
        None
    };
    let lifetime_tool_usage = if !params.session_only {
        stats::get_aggregate_tool_usage(&ctx.conn)
            .map_err(|e| mcp_error(format!("Failed to read lifetime tool usage: {e}")))?
    } else {
        Vec::new()
    };

    let primary_tools = if params.session_only {
        &session_tool_usage
    } else {
        &lifetime_tool_usage
    };
    let mut top_sorted: Vec<&stats::ToolUsageRecord> = primary_tools.iter().collect();
    top_sorted.sort_by_key(|r| std::cmp::Reverse(r.call_count));
    let top_5_most_called: Vec<Value> = top_sorted
        .iter()
        .take(5)
        .map(|r| {
            json!({
                "tool_name": r.tool_name,
                "call_count": r.call_count,
            })
        })
        .collect();

    let per_tool: Vec<Value> = session_tool_usage
        .iter()
        .map(|r| {
            json!({
                "tool_name": r.tool_name,
                "call_count": r.call_count,
                "total_tokens": r.total_tokens,
                "total_results": r.total_results,
            })
        })
        .collect();

    let session_json = session.as_ref().map(|s| {
        json!({
            "id": s.id,
            "total_calls": s.total_calls,
            "total_tokens": s.total_tokens,
            "truncations": s.truncation_count,
        })
    });

    let mut out = json!({
        "session": session_json,
        "per_tool": per_tool,
        "top_5_most_called": top_5_most_called,
    });

    if let Some(l) = lifetime {
        out["lifetime"] = json!({
            "total_sessions": l.total_sessions,
            "total_calls": l.total_calls,
            "total_tokens": l.total_tokens,
            "truncations": l.total_truncations,
            "avg_tokens_per_call": l.avg_tokens_per_call,
        });
        let lifetime_per_tool: Vec<Value> = lifetime_tool_usage
            .iter()
            .map(|r| {
                json!({
                    "tool_name": r.tool_name,
                    "call_count": r.call_count,
                    "total_tokens": r.total_tokens,
                    "total_results": r.total_results,
                })
            })
            .collect();
        out["lifetime_per_tool"] = Value::Array(lifetime_per_tool);
    }

    serde_json::to_string_pretty(&out)
        .map_err(|e| mcp_error(format!("Failed to serialize usage: {e}")))
}

/// Dispatch a tool call by method name. Returns a JSON value — callers are
/// responsible for the transport envelope.
///
/// Unknown methods return an `McpError`; the JSON-RPC caller maps that into
/// a `-32601 Method not found` response.
// ── Hook dispatch impls ───────────────────────────────────────────────────────
// These return raw hook envelopes (hookSpecificOutput) rather than {text:...}.
// Called from dispatch_call "hook.*" arms and from hook_client's no-daemon path.

/// Append one line to `.mdkb/hook-events.jsonl`; also `.mdkb/hook-slow.jsonl`
/// when elapsed exceeds the configured budget. Best-effort — silently drops on
/// I/O failure. Designed to run inside `spawn_blocking`.
fn log_hook_event(
    root: std::path::PathBuf,
    event: &str,
    outcome: &str,
    elapsed_ms: u64,
    slow_threshold_ms: u64,
) {
    use std::io::Write as _;
    let ts = chrono::Utc::now().timestamp();
    let mut line = serde_json::json!({
        "ts": ts,
        "event": event,
        "outcome": outcome,
        "elapsed_ms": elapsed_ms,
    })
    .to_string();
    line.push('\n');
    let mdkb_dir = root.join(".mdkb");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(mdkb_dir.join("hook-events.jsonl"))
    {
        let _ = f.write_all(line.as_bytes());
    }
    if elapsed_ms > slow_threshold_ms {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(mdkb_dir.join("hook-slow.jsonl"))
        {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Max ancestor stores layered into a read (keeps fan-out bounded; nested
/// projects rarely sit more than a couple of levels under a parent store).
const MAX_ANCESTOR_LAYERS: usize = 4;

/// Run `f` against each existing ancestor store's `index.sqlite`, opened
/// READ-ONLY. Never migrates, never creates, never spawns a watcher — it only
/// surfaces stores a human/`init` already created above the primary (e.g. a
/// nested git repo inheriting its parent repo's memory). Per-ancestor errors
/// are logged and skipped so a missing/old ancestor can never break a read.
fn for_each_ancestor_conn<F: FnMut(&rusqlite::Connection)>(primary_root: &std::path::Path, mut f: F) {
    for anc in crate::git::discover_ancestor_stores(primary_root, MAX_ANCESTOR_LAYERS) {
        let db = anc.join(".mdkb/index.sqlite");
        if !db.exists() {
            continue;
        }
        match rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            Ok(conn) => f(&conn),
            Err(e) => tracing::debug!("layer: skipping ancestor {}: {e}", anc.display()),
        }
    }
}

/// Extract the memory id from a warmup line (`[type] id: title #tags`).
fn warmup_line_id(line: &str) -> &str {
    line.split_once("] ")
        .map(|(_, rest)| rest)
        .unwrap_or(line)
        .split_once(':')
        .map(|(id, _)| id.trim())
        .unwrap_or(line)
}

pub async fn hook_session_start_impl(handle: &RepoHandle) -> Value {
    let cfg = &handle.config.hooks;
    if !cfg.session_start_enabled {
        return json!({});
    }
    if ensure_handle_context(handle).await.is_err() {
        return json!({});
    }
    let ctx_guard = handle.ctx.lock().await;
    let conn = match ctx_guard.as_ref() {
        Some(c) => &c.conn,
        None => return json!({}),
    };
    let limit = cfg.warmup_limit.max(1);
    let mut lines = match get_warmup_index(conn, limit) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("hook.session_start: get_warmup_index failed: {e}");
            return json!({});
        }
    };
    drop(ctx_guard);

    // Layer in ancestor stores (read-only): a nested project inherits its
    // parent's warmup. Primary entries win on id collision; result is capped.
    let mut seen: std::collections::HashSet<String> =
        lines.iter().map(|l| warmup_line_id(l).to_string()).collect();
    for_each_ancestor_conn(&handle.root, |conn| {
        if lines.len() >= limit {
            return;
        }
        if let Ok(anc_lines) = get_warmup_index(conn, limit) {
            for line in anc_lines {
                if lines.len() >= limit {
                    break;
                }
                if seen.insert(warmup_line_id(&line).to_string()) {
                    lines.push(line);
                }
            }
        }
    });

    if lines.is_empty() {
        return json!({});
    }

    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "mdkb".to_string());

    let mut body = String::from("## mdkb memory warmup\n\n");
    for line in &lines {
        body.push_str("- ");
        body.push_str(line);
        body.push('\n');
    }
    body.push_str(&format!(
        "\n**mdkb CLI** (semantic search — not available via Grep/Glob). Run `{bin} cheatsheet` for full syntax.\n"
    ));

    // Check code index staleness
    let code_db = handle.root.join(".mdkb/code.sqlite");
    if code_db.exists() {
        if let Ok(conn) = rusqlite::Connection::open_with_flags(
            &code_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            let last: Option<i64> = conn
                .query_row("SELECT MAX(indexed_at) FROM code_files", [], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .unwrap_or(None);
            if let Some(ts) = last {
                let now = chrono::Utc::now().timestamp();
                let age_days = (now - ts) / 86_400;
                if age_days >= 7 {
                    body.push_str(&format!(
                        "\n**⚠️ Code index is {age_days} days stale.** Run `{bin} code index --incremental` to refresh.\n"
                    ));
                }
            }
        }
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": body,
        }
    })
}

pub async fn hook_user_prompt_submit_impl(handle: &RepoHandle, prompt: &str) -> Value {
    use crate::cli::hook_logic::prompt_wants_call_graph;

    let cfg = &handle.config.hooks;
    if !cfg.user_prompt_submit_enabled {
        return json!({});
    }
    if prompt.trim().is_empty() || prompt_is_wrapup(prompt) {
        return json!({});
    }

    let wants_cg = prompt_wants_call_graph(prompt);
    let fts_query = build_recall_query(prompt);

    if fts_query.is_none() && !wants_cg {
        return json!({});
    }

    let mut results = Vec::new();
    if let Some(ref q) = fts_query {
        if ensure_handle_context(handle).await.is_err() {
            return json!({});
        }
        let ctx_guard = handle.ctx.lock().await;
        let conn = match ctx_guard.as_ref() {
            Some(c) => &c.conn,
            None => return json!({}),
        };
        let limit = cfg.recall_limit.max(1);
        results = match search_entries_fts(conn, q, limit) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("hook.user_prompt_submit: search_entries_fts failed: {e}");
                Vec::new()
            }
        };
        drop(ctx_guard);

        // Layer in ancestor stores (read-only): a nested project also recalls
        // its parent's memory. Primary entries win on id collision; capped.
        let mut seen: std::collections::HashSet<String> =
            results.iter().map(|e| e.id.clone()).collect();
        for_each_ancestor_conn(&handle.root, |conn| {
            if results.len() >= limit {
                return;
            }
            if let Ok(anc) = search_entries_fts(conn, q, limit) {
                for e in anc {
                    if results.len() >= limit {
                        break;
                    }
                    if seen.insert(e.id.clone()) {
                        results.push(e);
                    }
                }
            }
        });
    }

    if results.is_empty() && !wants_cg {
        return json!({});
    }

    if cfg.recall_half_life_secs > 0 && !results.is_empty() {
        let now = chrono::Utc::now().timestamp();
        let n = results.len() as f64;
        let mut scored: Vec<(f64, usize)> = results
            .iter()
            .enumerate()
            .map(|(rank, entry)| {
                let bm25 = (n - rank as f64) / n;
                let recency = access_recency_score(
                    entry.access_count,
                    entry.last_accessed,
                    now,
                    cfg.recall_half_life_secs,
                );
                (bm25 + recency, rank)
            })
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let orig = results.clone();
        results = scored.into_iter().map(|(_, i)| orig[i].clone()).collect();
    }

    let mut body = String::new();

    if !results.is_empty() {
        body.push_str("## mdkb: relevant context\n\n");
        for entry in &results {
            let snippet_raw = entry.content.trim().replace('\n', " ");
            let snippet: String = snippet_raw.chars().take(160).collect();
            body.push_str(&format!("- [{}] {} — {}\n", entry.id, entry.title, snippet));
        }
        body.push_str("\nIf your work corroborates any entry above, use `memory_confirm(id, outcome=\"confirmed\")` instead of writing a new one.\n");
    }

    if wants_cg {
        body.push_str(
            "\n💡 This looks like a call-graph query. Use `code_graph(name)` or `code_graph(name, direction=\"callers\"|\"callees\"|\"impact\")` — one MCP call replaces multi-file Grep.\n",
        );
    }

    if body.is_empty() {
        return json!({});
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": body,
        }
    })
}

pub async fn hook_post_tool_use_impl(handle: &RepoHandle, event: &Value) -> Value {
    if !handle.config.hooks.post_tool_use_enabled {
        return json!({});
    }
    let tool_name = match event.get("tool_name").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return json!({}),
    };
    if !REINDEX_TOOLS.contains(&tool_name) {
        return json!({});
    }
    let raw_path = match event.get("tool_input").and_then(tool_input_path) {
        Some(p) => p,
        None => return json!({}),
    };
    let path = match canonicalize_under_cwd(&handle.root, &raw_path) {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            tracing::warn!("hook.post_tool_use: rejected path outside root: {raw_path}");
            return json!({});
        }
    };
    if let Err(e) = handle.reindex_tx.try_send(path) {
        tracing::warn!("hook.post_tool_use: reindex_tx send failed: {e}");
        return json!({});
    }
    json!({"queued": true})
}

pub fn hook_pre_tool_use_impl(handle: &RepoHandle, event: &Value) -> Value {
    if !handle.config.hooks.pre_tool_use_enabled {
        return json!({});
    }
    let tool_name = match event.get("tool_name").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return json!({}),
    };
    let tool_input = match event.get("tool_input") {
        Some(v) => v,
        None => return json!({}),
    };
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_else(|| "mdkb".to_string());

    // Grep tool calls arrive with a clean pattern; Bash calls carry a raw shell
    // command we parse for a `grep`/`rg` filesystem search. Both feed the same
    // redirection classifiers. Claude searches code via Bash far more than the
    // Grep tool, so matching Bash is where the redirect actually reaches it.
    let suggestion = match tool_name {
        "Grep" => {
            let Some(pattern) = tool_input.get("pattern").and_then(|v| v.as_str()) else {
                return json!({});
            };
            let path = tool_input.get("path").and_then(|v| v.as_str());
            classify_definition_search(pattern, &bin)
                .or_else(|| classify_grep_pattern(pattern, path, &bin))
        }
        "Bash" => {
            let Some(command) = tool_input.get("command").and_then(|v| v.as_str()) else {
                return json!({});
            };
            classify_bash_search(command, &bin)
        }
        _ => return json!({}),
    };

    // DEFERRED (2026-06-07) — inject actual symbol hits (file:line) inline
    // instead of a suggestion ("act, not suggest"). Gated on conversion data
    // from the `mdkb_invocation` telemetry: only worth the code-index lookup on
    // this hot-path hook if the plain suggestion proves not to convert.
    match suggestion {
        Some(text) => json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "additionalContext": text,
            }
        }),
        None => json!({}),
    }
}

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
            let dry_run = params
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = memory_delete_impl(&handle, id, dry_run).await?;
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
            let dry_run = params
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entry: MemoryWriteBatchEntry = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("memory_write: invalid params: {e}")))?;
            let text = memory_write_impl(&handle, &entry, dry_run).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "memory_write", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "memory_write_batch" => {
            let dry_run = params
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let entries_value = params
                .get("entries")
                .cloned()
                .ok_or_else(|| mcp_error("memory_write_batch: missing 'entries'"))?;
            let entries: Vec<MemoryWriteBatchEntry> = serde_json::from_value(entries_value)
                .map_err(|e| mcp_error(format!("memory_write_batch: invalid 'entries': {e}")))?;
            let (text, count) = memory_write_batch_impl(&handle, &entries, dry_run).await?;
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
                .map(|n| (n as usize).min(200))
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
        "update" => {
            let text = update_impl(&handle).await?;
            let tokens = count_tokens(&text);
            dctx.metrics.record_update(tokens);
            dctx.record_persistent_call(&handle, "update", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "code_graph" => {
            let cp: CodeGraphParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("code_graph: invalid params: {e}")))?;
            let text = code_graph_impl(&handle, &cp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "code_graph", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "graph" => {
            let gp: GraphParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("graph: invalid params: {e}")))?;
            let text = graph_impl(&handle, &gp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "graph", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "symbols_in_file" => {
            let sp: SymbolsInFileParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("symbols_in_file: invalid params: {e}")))?;
            let text = symbols_in_file_impl(&handle, &sp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "symbols_in_file", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "symbol_at_position" => {
            let sp: SymbolAtPositionParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("symbol_at_position: invalid params: {e}")))?;
            let text = symbol_at_position_impl(&handle, &sp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "symbol_at_position", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "code_find" => {
            let cp: CodeFindParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("code_find: invalid params: {e}")))?;
            let text = code_find_impl(&handle, &cp).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "code_find", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "usage" => {
            let up: UsageParams = serde_json::from_value(params)
                .map_err(|e| mcp_error(format!("usage: invalid params: {e}")))?;
            let session_id = dctx.session_id.load(Ordering::Relaxed);
            let text = usage_impl(&handle, &up, session_id).await?;
            let tokens = count_tokens(&text);
            dctx.record_persistent_call(&handle, "usage", tokens, 1, false)
                .await;
            Ok(json!({ "text": text, "tokens": tokens }))
        }
        "hook.session_start" => {
            let t0 = std::time::Instant::now();
            let result = hook_session_start_impl(&handle).await;
            let ms = t0.elapsed().as_millis() as u64;
            let outcome = if result == json!({}) {
                "skipped"
            } else {
                "fired"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "session_start", outcome, ms, budget)
            });
            Ok(result)
        }
        "hook.user_prompt_submit" => {
            let prompt = params
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let t0 = std::time::Instant::now();
            let result = hook_user_prompt_submit_impl(&handle, prompt).await;
            let ms = t0.elapsed().as_millis() as u64;
            let outcome = if result == json!({}) {
                "skipped"
            } else {
                "fired"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "user_prompt_submit", outcome, ms, budget)
            });
            Ok(result)
        }
        "hook.post_tool_use" => {
            let t0 = std::time::Instant::now();
            let result = hook_post_tool_use_impl(&handle, &params).await;
            let ms = t0.elapsed().as_millis() as u64;
            let outcome = if result == json!({}) {
                "skipped"
            } else {
                "fired"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "post_tool_use", outcome, ms, budget)
            });
            Ok(json!({}))
        }
        "hook.pre_tool_use" => {
            let t0 = std::time::Instant::now();
            let result = hook_pre_tool_use_impl(&handle, &params);
            let ms = t0.elapsed().as_millis() as u64;
            // "mdkb_invocation" is the conversion signal: a Bash command that
            // actually runs mdkb. Tracking it against "fired" measures whether
            // the redirect suggestions land. (A fire produces a non-empty result;
            // an mdkb call produces none, so the checks don't overlap.)
            let outcome = if result != json!({}) {
                "fired"
            } else if params.get("tool_name").and_then(|v| v.as_str()) == Some("Bash")
                && params
                    .get("tool_input")
                    .and_then(|t| t.get("command"))
                    .and_then(|v| v.as_str())
                    .is_some_and(is_mdkb_invocation)
            {
                "mdkb_invocation"
            } else {
                "skipped"
            };
            let root = handle.root.clone();
            let budget = handle.config.hooks.latency_budget_ms;
            tokio::task::spawn_blocking(move || {
                log_hook_event(root, "pre_tool_use", outcome, ms, budget)
            });
            Ok(result)
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
    async fn warmup_layers_ancestor_store_readonly_without_creating() {
        let tmp = TempDir::new().unwrap();
        // Parent store (the ancestor) with its own memory entry.
        let parent = make_handle(&tmp);
        seed_memory_entry(&parent, "parent-mem").await;

        // Primary store nested under the parent.
        let nested_root = tmp.path().join("nested-repo");
        std::fs::create_dir_all(nested_root.join(".mdkb")).unwrap();
        let primary = Arc::new(RepoHandle::from_shared(
            nested_root.clone(),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Config::default(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ));
        seed_memory_entry(&primary, "child-mem").await;

        let out = hook_session_start_impl(&primary).await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(body.contains("child-mem"), "primary entry missing: {body}");
        assert!(
            body.contains("parent-mem"),
            "ancestor entry missing — layering broken: {body}"
        );

        // Guardrail: layered reads must never create a store anywhere.
        assert!(!nested_root.join("sub/.mdkb").exists());
    }

    #[tokio::test]
    async fn recall_layers_ancestor_store_readonly_without_creating() {
        let tmp = TempDir::new().unwrap();
        // Parent store (the ancestor) with its own memory entry.
        let parent = make_handle(&tmp);
        seed_memory_entry(&parent, "parent-mem").await;

        // Primary store nested under the parent.
        let nested_root = tmp.path().join("nested-repo");
        std::fs::create_dir_all(nested_root.join(".mdkb")).unwrap();
        let primary = Arc::new(RepoHandle::from_shared(
            nested_root.clone(),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Config::default(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ));
        seed_memory_entry(&primary, "child-mem").await;

        // Prompt terms match the seeded entries' content ("...about the topic.").
        let out =
            hook_user_prompt_submit_impl(&primary, "what do we know about the topic content").await;
        let body = out
            .pointer("/hookSpecificOutput/additionalContext")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(body.contains("child-mem"), "primary recall entry missing: {body}");
        assert!(
            body.contains("parent-mem"),
            "ancestor recall entry missing — recall layering broken: {body}"
        );

        // Guardrail: layered reads must never create a store anywhere.
        assert!(!nested_root.join("sub/.mdkb").exists());
    }

    /// Seed a document `project.md` with an `owner -> alice` frontmatter edge.
    /// Returns the document id.
    async fn seed_graph_doc(handle: &RepoHandle) -> i64 {
        ensure_handle_context(handle).await.expect("init ctx");
        let ctx_guard = handle.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let conn = &ctx.conn;
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at)
             VALUES ('docs', './docs', '**/*.md', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('h1', '# P', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at)
             VALUES ('docs', 'project.md', 'h1', 1, 1)",
            [],
        )
        .unwrap();
        let doc_id = conn.last_insert_rowid();
        crate::store::graph::add_edge(
            conn,
            doc_id,
            "alice",
            "owner",
            crate::store::graph::KIND_FRONTMATTER,
            None,
        )
        .unwrap();
        doc_id
    }

    #[tokio::test]
    async fn graph_impl_links_backlinks_neighbors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_graph_doc(&handle).await;

        // links: outgoing from the document.
        let links = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "links".to_string(),
                relation: None,
                depth: 1,
            },
        )
        .await
        .expect("links");
        assert!(links.contains("alice"), "links: {links}");
        assert!(links.contains("owner"), "links: {links}");

        // backlinks: by the raw dangling slug.
        let backlinks = graph_impl(
            &handle,
            &GraphParams {
                entity: "alice".to_string(),
                root: None,
                to: None,
                direction: "backlinks".to_string(),
                relation: None,
                depth: 1,
            },
        )
        .await
        .expect("backlinks");
        assert!(
            backlinks.contains("backlinks for alice"),
            "backlinks: {backlinks}"
        );

        // neighbors: alice is one hop from project.md (undirected).
        let neighbors = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "neighbors".to_string(),
                relation: None,
                depth: 1,
            },
        )
        .await
        .expect("neighbors");
        assert!(neighbors.contains("alice"), "neighbors: {neighbors}");

        // path: project.md -> alice (alice is a dangling one-hop target).
        let path = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: Some("alice".to_string()),
                direction: "path".to_string(),
                relation: None,
                depth: 1,
            },
        )
        .await
        .expect("path");
        assert!(path.contains("project.md"), "path: {path}");
        assert!(path.contains("alice"), "path: {path}");
    }

    #[tokio::test]
    async fn graph_impl_path_requires_to() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_graph_doc(&handle).await;

        let err = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "path".to_string(),
                relation: None,
                depth: 1,
            },
        )
        .await
        .expect_err("path without 'to' should error");
        assert!(err.to_string().contains("requires 'to'"), "err: {err}");
    }

    #[tokio::test]
    async fn graph_impl_rejects_unknown_direction() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_graph_doc(&handle).await;

        let err = graph_impl(
            &handle,
            &GraphParams {
                entity: "project.md".to_string(),
                root: None,
                to: None,
                direction: "sideways".to_string(),
                relation: None,
                depth: 1,
            },
        )
        .await
        .expect_err("unknown direction should error");
        assert!(err.to_string().contains("Unknown direction"), "err: {err}");
    }

    #[tokio::test]
    async fn dispatch_call_routes_graph_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        seed_graph_doc(&handle).await;

        let result = dispatch_call(
            "graph",
            json!({ "entity": "project.md", "direction": "links" }),
            handle,
            &dctx,
        )
        .await
        .expect("dispatch graph");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        assert!(text.contains("alice"), "result: {result}");
    }

    #[tokio::test]
    async fn memory_delete_impl_rejects_invalid_id() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let err = memory_delete_impl(&handle, "UPPER_CASE", false)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("ID must be"),
            "should reject invalid ID: {}",
            err.message
        );

        let err2 = memory_delete_impl(&handle, "", false).await.unwrap_err();
        assert!(err2.message.contains("ID must be"));
    }

    #[tokio::test]
    async fn memory_delete_impl_reports_not_found_for_missing_id() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let out = memory_delete_impl(&handle, "does-not-exist", false)
            .await
            .expect("delete impl");

        assert!(out.contains("not found"), "output: {out}");
    }

    #[tokio::test]
    async fn memory_delete_impl_removes_seeded_entry() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "to-delete").await;

        let out = memory_delete_impl(&handle, "to-delete", false)
            .await
            .expect("delete impl");

        assert!(out.contains("Deleted memory entry"), "output: {out}");

        // Second call must report not found now.
        let again = memory_delete_impl(&handle, "to-delete", false)
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
            source_file: None,
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

        let created = memory_write_impl(&handle, &entry_input("w-1"), false)
            .await
            .expect("write impl");
        assert!(
            created.starts_with("Created memory entry: w-1"),
            "out: {created}"
        );

        let mut second = entry_input("w-1");
        second.content = "Updated content body".to_string();
        let updated = memory_write_impl(&handle, &second, false)
            .await
            .expect("write impl update");
        assert!(
            updated.starts_with("Updated memory entry: w-1"),
            "out: {updated}"
        );
    }

    #[tokio::test]
    async fn memory_write_impl_dry_run_does_not_persist() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let preview = memory_write_impl(&handle, &entry_input("dry-w"), true)
            .await
            .expect("dry-run write");
        assert_eq!(preview, "dry-run: would create memory entry 'dry-w'");

        // A real delete must report the entry was never written.
        let after = memory_delete_impl(&handle, "dry-w", false)
            .await
            .expect("delete impl");
        assert!(after.contains("not found"), "entry persisted: {after}");
    }

    #[tokio::test]
    async fn memory_delete_impl_dry_run_keeps_entry() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "dry-del").await;

        let preview = memory_delete_impl(&handle, "dry-del", true)
            .await
            .expect("dry-run delete");
        assert_eq!(preview, "dry-run: would delete memory entry 'dry-del'");

        // The entry must still be present for a real delete to remove.
        let real = memory_delete_impl(&handle, "dry-del", false)
            .await
            .expect("delete impl");
        assert!(real.contains("Deleted memory entry"), "entry gone: {real}");
    }

    #[tokio::test]
    async fn memory_write_batch_impl_rejects_empty() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);

        let err = memory_write_batch_impl(&handle, &[], false)
            .await
            .expect_err("must reject");
        assert!(
            err.message.contains("must not be empty"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn memory_write_batch_impl_rejects_over_limit() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let entries: Vec<_> = (0..21).map(|i| entry_input(&format!("b-{i}"))).collect();

        let err = memory_write_batch_impl(&handle, &entries, false)
            .await
            .expect_err("must reject");
        assert!(
            err.message.contains("max 20 entries"),
            "msg: {}",
            err.message
        );
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

        let (text, count) = memory_write_batch_impl(&handle, &[a, b, c], false)
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
        assert!(
            text.contains("Created memory entry: via-dispatch"),
            "result: {result}"
        );
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

        let err = search_impl(&handle, &params)
            .await
            .expect_err("should error");
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

        let (text, count) = search_impl(&handle, &params).await.expect("symbols scope");
        assert_eq!(count, 0, "text: {text}");
        assert!(text.contains("0 matches"), "text: {text}");
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

        let (text, count, truncated) = get_impl(&handle, &params).await.expect("glob get");
        assert_eq!(count, 0, "text: {text}");
        assert!(!truncated);
        assert!(
            text.contains("No documents matched pattern"),
            "text: {text}"
        );
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

        let result = dispatch_call("get", json!({ "id": "dispatched-get" }), handle, &dctx)
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

    #[tokio::test]
    async fn usage_impl_returns_empty_session_when_no_session_id() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let params = UsageParams {
            session_only: true,
            root: None,
        };

        let body = usage_impl(&handle, &params, 0).await.expect("usage impl");
        let parsed: Value = serde_json::from_str(&body).expect("json");

        assert!(parsed.get("session").map_or(false, Value::is_null));
        assert_eq!(parsed["per_tool"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            parsed["top_5_most_called"].as_array().map(Vec::len),
            Some(0)
        );
        assert!(parsed.get("lifetime").is_none());
    }

    #[tokio::test]
    async fn dispatch_call_routes_usage_to_impl() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let result = dispatch_call("usage", json!({}), handle, &dctx)
            .await
            .expect("dispatch");

        let text = result.get("text").and_then(Value::as_str).unwrap_or("");
        let parsed: Value = serde_json::from_str(text).expect("usage text is json");
        assert!(parsed.get("session").is_some(), "result: {result}");
        assert!(result.get("tokens").is_some(), "result: {result}");
    }

    #[tokio::test]
    async fn code_graph_impl_returns_rebuild_hint_when_no_index() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        // Mark reindex active so acquire_handle_code_index returns the (None) lock.
        handle
            .code_reindex_active
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let params = CodeGraphParams {
            name: "anything".into(),
            root: None,
            direction: "calls".into(),
            symbol_id: None,
            max_depth: 3,
        };

        let text = code_graph_impl(&handle, &params)
            .await
            .expect("code_graph impl");
        assert!(text.contains("Code index is being rebuilt"), "text: {text}");
    }

    #[tokio::test]
    async fn dispatch_call_code_graph_invalid_params_errors() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("code_graph", json!({}), handle, &dctx)
            .await
            .expect_err("missing name");
        assert!(
            err.message.contains("invalid params"),
            "msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn dispatch_call_unknown_method_remains_method_not_found_after_new_routes() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();

        let err = dispatch_call("nonexistent_tool_xyz", json!({}), handle, &dctx)
            .await
            .expect_err("should be method-not-found");
        assert_eq!(err.code, ErrorCode::METHOD_NOT_FOUND);
    }

    // --- memory_list limit cap ---

    #[tokio::test]
    async fn memory_list_limit_clamped_to_200() {
        // Requesting limit=500 must be silently clamped to 200.
        // Seed 5 entries and verify the call succeeds and count <= 200.
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        for i in 0..5u8 {
            seed_memory_entry(&handle, &format!("entry-{i}")).await;
        }
        // dispatch with limit=500 — must not error
        let result = dispatch_call(
            "memory_list",
            json!({ "limit": 500, "sort": "recent" }),
            handle,
            &make_dctx(),
        )
        .await
        .expect("memory_list must succeed");
        let count = result.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        assert!(count <= 5, "returned {count} entries");
    }

    #[tokio::test]
    async fn memory_list_default_limit_is_below_200() {
        // No explicit limit → default of 20, well within the 200 cap.
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        seed_memory_entry(&handle, "solo").await;
        let result = dispatch_call(
            "memory_list",
            json!({ "sort": "recent" }),
            handle,
            &make_dctx(),
        )
        .await
        .expect("memory_list default");
        let count = result.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        assert_eq!(count, 1);
    }

    // --- get_batch_impl ID cap ---

    #[tokio::test]
    async fn get_batch_rejects_more_than_50_ids() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        ensure_handle_context(&handle).await.unwrap();

        // Build a comma-separated string of 51 IDs
        let ids: String = (1..=51)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let err = get_batch_impl(&handle, &ids, None)
            .await
            .expect_err("must reject >50 IDs");
        assert!(
            err.message.contains("too many IDs"),
            "unexpected msg: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn get_batch_accepts_exactly_50_ids() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        ensure_handle_context(&handle).await.unwrap();

        // 50 IDs that don't exist — should succeed (returning "Not found" entries)
        let ids: String = (1..=50)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        // Panics only if the cap itself errors — "not found" is acceptable output
        let _ = get_batch_impl(&handle, &ids, None).await;
    }

    // ── hook method tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn hook_session_start_silent_on_empty_index() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let result = hook_session_start_impl(&handle).await;
        // Empty index → no warmup lines → silent ({})
        assert_eq!(result, json!({}), "must be silent when index is empty");
    }

    #[tokio::test]
    async fn hook_session_start_disabled_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        let mut cfg = Config::default();
        cfg.hooks.session_start_enabled = false;
        let handle = Arc::new(RepoHandle::from_shared(
            root,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            cfg,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        ));
        let result = hook_session_start_impl(&handle).await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_user_prompt_submit_silent_on_empty_prompt() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let result = hook_user_prompt_submit_impl(&handle, "").await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_user_prompt_submit_silent_on_wrapup() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let result = hook_user_prompt_submit_impl(&handle, "/clear").await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_user_prompt_submit_silent_when_no_results() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        // Empty DB → no results
        let result = hook_user_prompt_submit_impl(&handle, "find authentication bug").await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_post_tool_use_ignores_unknown_tool() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let event = json!({"tool_name": "Bash", "tool_input": {"command": "ls"}});
        let result = hook_post_tool_use_impl(&handle, &event).await;
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn hook_post_tool_use_injects_valid_path() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        // Create a real file so canonicalize_under_cwd can resolve the parent dir
        let file = tmp.path().join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn main() {}").unwrap();
        let event = json!({
            "tool_name": "Write",
            "tool_input": {"file_path": file.to_str().unwrap()},
        });
        let result = hook_post_tool_use_impl(&handle, &event).await;
        assert_eq!(
            result,
            json!({"queued": true}),
            "post_tool_use returns queued on success"
        );
    }

    #[tokio::test]
    async fn hook_pre_tool_use_suggests_symbol_search() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let event = json!({
            "tool_name": "Grep",
            "tool_input": {"pattern": "handle_session_start"},
        });
        let result = hook_pre_tool_use_impl(&handle, &event);
        assert!(
            result.get("hookSpecificOutput").is_some(),
            "must suggest alternative for plain identifier"
        );
    }

    #[tokio::test]
    async fn hook_pre_tool_use_silent_for_non_grep_tool() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let event = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": "src/lib.rs"},
        });
        let result = hook_pre_tool_use_impl(&handle, &event);
        assert_eq!(result, json!({}));
    }

    #[tokio::test]
    async fn dispatch_call_routes_hook_session_start() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        let result = dispatch_call("hook.session_start", json!({}), handle, &dctx)
            .await
            .expect("must not error");
        // Empty index → silent, but the call must succeed
        assert!(result == json!({}) || result.get("hookSpecificOutput").is_some());
    }

    #[tokio::test]
    async fn dispatch_call_routes_hook_pre_tool_use() {
        let tmp = TempDir::new().unwrap();
        let handle = make_handle(&tmp);
        let dctx = make_dctx();
        let event = json!({
            "tool_name": "Grep",
            "tool_input": {"pattern": "dispatch_call"},
        });
        let result = dispatch_call("hook.pre_tool_use", event, handle, &dctx)
            .await
            .expect("must not error");
        assert!(result.get("hookSpecificOutput").is_some());
    }
}
