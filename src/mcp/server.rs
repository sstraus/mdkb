//! MCP server implementation.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::handler::server::tool::{Parameters, ToolRouter};
use rmcp::model::{
    CallToolResult, Content, EmptyObject, ErrorCode, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{ErrorData as McpError, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::cli::handlers::{Context, handle_hybrid_search, handle_mget, handle_update};
use crate::config::McpConfig;
use crate::domain::SearchResult;
use crate::metrics::{count_tokens, truncate_with_continuation, truncate_with_ellipsis, UsageMetrics};
use crate::store::{collections, documents, evolution, memory, search, stats};
use crate::watcher::{FileWatcher, WatcherConfig};

use super::tools::{
    EvolutionDirection, EvolutionParams, GetParams, MemoryGetParams, MemoryIndexParams,
    MemorySearchParams, MemoryWriteParams, MetricsParams, MultiGetParams, SearchParams,
};

/// Create an MCP error from a message.
fn mcp_error(message: impl Into<Cow<'static, str>>) -> McpError {
    McpError {
        code: ErrorCode::INTERNAL_ERROR,
        message: message.into(),
        data: None,
    }
}

/// MCP server for mdkb.
#[derive(Clone)]
pub struct McpServer {
    /// Path to the mdkb root directory.
    root: PathBuf,
    /// Shared database context.
    ctx: Arc<Mutex<Option<Context>>>,
    /// Tool router.
    tool_router: ToolRouter<Self>,
    /// Usage metrics tracker (in-memory).
    metrics: Arc<UsageMetrics>,
    /// MCP configuration.
    config: McpConfig,
    /// Current session ID for persistent stats.
    session_id: Arc<AtomicI64>,
    /// Warmup instructions (loaded at startup, used in get_info).
    warmup_instructions: Option<String>,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

#[tool_router]
impl McpServer {
    /// Create a new MCP server with default config.
    pub fn new(root: PathBuf) -> Self {
        Self::with_config(root, McpConfig::default())
    }

    /// Create a new MCP server with custom config.
    pub fn with_config(root: PathBuf, config: McpConfig) -> Self {
        Self {
            root,
            ctx: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
            metrics: Arc::new(UsageMetrics::new()),
            config,
            session_id: Arc::new(AtomicI64::new(0)),
            warmup_instructions: None,
        }
    }

    /// Create a new MCP server with warmup instructions pre-loaded.
    pub fn with_warmup(root: PathBuf, config: McpConfig, warmup: Option<String>) -> Self {
        Self {
            root,
            ctx: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
            metrics: Arc::new(UsageMetrics::new()),
            config,
            session_id: Arc::new(AtomicI64::new(0)),
            warmup_instructions: warmup,
        }
    }

    /// Get the usage metrics.
    pub fn metrics(&self) -> &UsageMetrics {
        &self.metrics
    }

    /// Initialize the database connection and stats session.
    async fn ensure_context(&self) -> Result<(), McpError> {
        let mut ctx_guard = self.ctx.lock().await;
        if ctx_guard.is_none() {
            let ctx = Context::open(&self.root)
                .map_err(|e| mcp_error(format!("Failed to open database: {}", e)))?;

            // Initialize stats schema
            stats::init_stats_schema(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to init stats schema: {}", e)))?;

            // Create a new session if we don't have one
            if self.session_id.load(Ordering::Relaxed) == 0 {
                let session_id = stats::create_session(&ctx.conn)
                    .map_err(|e| mcp_error(format!("Failed to create session: {}", e)))?;
                self.session_id.store(session_id, Ordering::Relaxed);
                tracing::info!("Started stats session {}", session_id);
            }

            *ctx_guard = Some(ctx);
        }
        Ok(())
    }

    /// Record a tool call to persistent stats.
    async fn record_persistent_call(
        &self,
        tool_name: &str,
        tokens: usize,
        results: usize,
        truncated: bool,
    ) {
        let session_id = self.session_id.load(Ordering::Relaxed);
        if session_id == 0 {
            return; // No session yet
        }

        let ctx_guard = self.ctx.lock().await;
        if let Some(ctx) = ctx_guard.as_ref() {
            if let Err(e) = stats::record_call(&ctx.conn, session_id, tool_name, tokens, results, truncated) {
                tracing::warn!("Failed to record call stats: {}", e);
            }
        }
    }

    /// Search documents using hybrid search (BM25 + semantic with RRF fusion).
    #[tool(description = "Search markdown documents using hybrid search (combines keyword and semantic search)")]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens, result_count) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let results = handle_hybrid_search(
                ctx,
                &params.query,
                params.limit,
                params.collection.as_deref(),
                params.include_superseded,
            )
            .map_err(|e| mcp_error(format!("Search failed: {}", e)))?;

            let output = format_search_results(&results);
            let tokens = count_tokens(&output);
            let result_count = results.len();
            (output, tokens, result_count)
        }; // ctx_guard dropped here before record_persistent_call

        self.metrics.record_search(tokens, result_count);
        self.record_persistent_call("search", tokens, result_count, false).await;
        tracing::debug!("mdkb_search: {} tokens, {} results", tokens, result_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve a document by ID or path.
    #[tool(description = "Retrieve a document by ID or path, with optional line range")]
    async fn get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let max_tokens = self.config.max_response_tokens;
        let (output, tokens, truncated) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            // Try to parse as ID
            let doc = if let Ok(id) = params.id.parse::<i64>() {
                documents::get_document(&ctx.conn, id)
                    .map_err(|e| mcp_error(format!("Failed to get document: {}", e)))?
            } else {
                None
            };

            let doc = doc.ok_or_else(|| mcp_error(format!("Document not found: {}", params.id)))?;

            let content = documents::get_content(&ctx.conn, &doc.hash)
                .map_err(|e| mcp_error(format!("Failed to get content: {}", e)))?
                .ok_or_else(|| mcp_error("Content not found"))?;

            // Apply line range if specified
            let output = if let Some(range) = &params.lines {
                apply_line_range(&content, range)?
            } else {
                content
            };

            // Apply token limit with continuation guidance
            let (output, truncated) = if max_tokens > 0 {
                let result = truncate_with_continuation(&output, max_tokens, doc.id);
                (result.content, result.truncated)
            } else {
                (output, false)
            };

            let tokens = count_tokens(&output);
            (output, tokens, truncated)
        }; // ctx_guard dropped here

        self.metrics.record_get(tokens);
        self.record_persistent_call("get", tokens, 1, truncated).await;
        tracing::debug!("mdkb_get: {} tokens, truncated={}", tokens, truncated);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// List all collections.
    #[tool(description = "List all indexed collections with their paths and document counts")]
    async fn list_collections(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens, coll_count) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let coll_list = collections::list_collections(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to list collections: {}", e)))?;

            if coll_list.is_empty() {
                return Ok(CallToolResult::success(vec![Content::text(
                    "No collections found. Use 'mdkb collection add <name> <path>' to add one.",
                )]));
            }

            let mut output = String::from("Collections:\n");
            for coll in &coll_list {
                // Get document count for this collection
                let doc_count = collections::get_collection_document_count(&ctx.conn, &coll.name)
                    .unwrap_or(0);
                output.push_str(&format!(
                    "- {} ({}): {} documents\n  Pattern: {}\n",
                    coll.name,
                    coll.path,
                    doc_count,
                    coll.pattern
                ));
            }

            let tokens = count_tokens(&output);
            let coll_count = coll_list.len();
            (output, tokens, coll_count)
        }; // ctx_guard dropped here

        self.record_persistent_call("list_collections", tokens, coll_count, false).await;
        tracing::debug!("mdkb_list_collections: {} tokens, {} collections", tokens, coll_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Get index status.
    #[tool(description = "Get the current index status (collections, documents, etc.)")]
    async fn status(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let status = search::get_status(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to get status: {}", e)))?;

            let output = format!(
                "Collections: {}\nDocuments: {}\nStale: {}\nDB Size: {} bytes",
                status.collections, status.documents, status.stale_documents, status.db_size_bytes
            );

            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.metrics.record_status(tokens);
        self.record_persistent_call("status", tokens, 1, false).await;
        tracing::debug!("mdkb_status: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Trigger reindex of all collections.
    #[tool(description = "Trigger a differential reindex of all collections")]
    async fn update(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens) = {
            let mut ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_mut()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let result = handle_update(ctx, &self.root)
                .map_err(|e| mcp_error(format!("Update failed: {}", e)))?;

            let output = format!(
                "Added: {}\nUpdated: {}\nRemoved: {}\nUnchanged: {}",
                result.added, result.updated, result.removed, result.unchanged
            );

            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.metrics.record_update(tokens);
        self.record_persistent_call("update", tokens, 1, false).await;
        tracing::debug!("mdkb_update: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve multiple documents by pattern.
    #[tool(description = "Retrieve multiple documents matching a glob pattern")]
    async fn multi_get(
        &self,
        Parameters(params): Parameters<MultiGetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let doc_limit = self.config.max_document_tokens;
        let truncate_ellipsis = self.config.truncate_with_ellipsis;
        let max_response_tokens = self.config.max_response_tokens;

        let (output, result_count) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let results = handle_mget(ctx, &params.pattern, params.collection.as_deref())
                .map_err(|e| mcp_error(format!("Multi-get failed: {}", e)))?;

            if results.is_empty() {
                return Ok(CallToolResult::success(vec![Content::text(
                    "No documents found.",
                )]));
            }

            let mut output = format!("Found {} documents:\n\n", results.len());

            for (doc, content) in &results {
                let title = doc.title.as_deref().unwrap_or("(untitled)");

                // Apply per-document token limit if configured
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
        }; // ctx_guard dropped here

        // Apply overall response limit (no lock needed for this)
        let original_len = output.len();
        let output = if max_response_tokens > 0 {
            crate::metrics::tokens::truncate_to_tokens(&output, max_response_tokens).0
        } else {
            output
        };
        let truncated = output.len() < original_len;
        let tokens = count_tokens(&output);

        self.metrics.record_multi_get(tokens, result_count);
        self.record_persistent_call("multi_get", tokens, result_count, truncated).await;
        tracing::debug!("mdkb_multi_get: {} tokens, {} docs, truncated={}", tokens, result_count, truncated);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Get usage and query performance metrics.
    #[tool(description = "Get search performance metrics for self-evaluation. Includes token usage, query latency, zero-result rate, and quality scores. Use to understand query patterns, identify issues, and track improvements.")]
    async fn get_metrics(
        &self,
        Parameters(params): Parameters<MetricsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        // Current session token usage (doesn't need the ctx lock)
        let summary = self.metrics.summary();

        let output = {
            let ctx_guard = self.ctx.lock().await;

            let mut output = String::new();
            let mut warnings: Vec<String> = Vec::new();

            output.push_str(&format!(
                "=== Token Usage (Current Session) ===\n\
                 Total calls: {}\n\
                 Total tokens: {}\n\
                 Avg tokens/call: {:.1}\n",
                summary.total_calls,
                summary.total_tokens,
                summary.avg_tokens_per_call,
            ));

            if let Some(ctx) = ctx_guard.as_ref() {
                // Get query metrics for the period
                if let Ok(query_metrics) = stats::get_query_metrics(&ctx.conn, params.period_days) {
                    output.push_str(&format!(
                        "\n=== Query Metrics (last {} days) ===\n\
                         Total queries: {}\n\
                         Zero-result rate: {:.1}%{}\n\
                         Re-search rate: {:.1}%{}\n",
                        params.period_days,
                        query_metrics.total_queries,
                        query_metrics.zero_result_rate,
                        if query_metrics.zero_result_rate > 10.0 { " ⚠️" } else { " ✓" },
                        query_metrics.re_search_rate,
                        if query_metrics.re_search_rate > 15.0 { " ⚠️" } else { " ✓" },
                    ));

                    // Check for warnings
                    if query_metrics.zero_result_rate > 10.0 {
                        warnings.push(format!(
                            "Zero-result rate {:.1}% > 10% threshold - queries not finding results",
                            query_metrics.zero_result_rate
                        ));
                    }
                    if query_metrics.re_search_rate > 15.0 {
                        warnings.push(format!(
                            "Re-search rate {:.1}% > 15% threshold - initial results may be poor",
                            query_metrics.re_search_rate
                        ));
                    }

                    // Latency section
                    if params.include_latency {
                        output.push_str(&format!(
                            "\nLatency:\n\
                             - p50: {}ms{}\n\
                             - p95: {}ms{}\n\
                             - p99: {}ms{}\n",
                            query_metrics.latency_p50,
                            if query_metrics.latency_p50 > 100 { " ⚠️" } else { " ✓" },
                            query_metrics.latency_p95,
                            if query_metrics.latency_p95 > 300 { " ⚠️" } else { " ✓" },
                            query_metrics.latency_p99,
                            if query_metrics.latency_p99 > 500 { " ⚠️" } else { " ✓" },
                        ));

                        if query_metrics.latency_p99 > 500 {
                            warnings.push(format!(
                                "p99 latency {}ms > 500ms threshold - performance issue",
                                query_metrics.latency_p99
                            ));
                        }
                    }

                    // Quality section
                    if params.include_quality {
                        output.push_str(&format!(
                            "\nScore Distribution:\n\
                             - Excellent (>0.8): {:.1}%\n\
                             - Good (0.5-0.8): {:.1}%\n\
                             - Poor (<0.5): {:.1}%\n",
                            query_metrics.score_above_80,
                            query_metrics.score_50_to_80,
                            query_metrics.score_below_50,
                        ));

                        if query_metrics.score_below_50 > 20.0 {
                            warnings.push(format!(
                                "Poor-score rate {:.1}% > 20% threshold - content quality or relevance issue",
                                query_metrics.score_below_50
                            ));
                        }
                    }
                }

                // Add latency by search type if requested
                if params.include_latency {
                    if let Ok(latency_stats) = stats::get_query_latency_stats(&ctx.conn) {
                        if !latency_stats.is_empty() {
                            output.push_str("\nLatency by Search Type:");
                            for stat in &latency_stats {
                                output.push_str(&format!(
                                    "\n- {}: {:.1}ms avg, {}ms max ({} queries)",
                                    stat.search_type,
                                    stat.avg_latency_ms,
                                    stat.max_latency_ms,
                                    stat.count
                                ));
                            }
                            output.push('\n');
                        }
                    }
                }

                // All-time token stats
                if let Ok(aggregate) = stats::get_aggregate_stats(&ctx.conn) {
                    output.push_str(&format!(
                        "\n=== All-Time Token Stats ===\n\
                         Total sessions: {}\n\
                         Total calls: {}\n\
                         Total tokens: {}\n",
                        aggregate.total_sessions,
                        aggregate.total_calls,
                        aggregate.total_tokens,
                    ));
                }
            }

            // Add warnings section
            if !warnings.is_empty() {
                output.push_str("\n⚠️ Warnings:\n");
                for warning in &warnings {
                    output.push_str(&format!("  - {}\n", warning));
                }
            } else {
                output.push_str("\n✓ All metrics within acceptable ranges\n");
            }

            output
        }; // ctx_guard dropped here

        let tokens = count_tokens(&output);
        self.record_persistent_call("metrics", tokens, 1, false).await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Get memory warmup index (compact list for AI session start).
    #[tool(description = "Get memory index for session warmup (~50 entries, compact format). Call this at session start to load context.")]
    async fn memory_index(
        &self,
        Parameters(params): Parameters<MemoryIndexParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens, entry_count) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let index = memory::get_warmup_index(&ctx.conn, params.limit)
                .map_err(|e| mcp_error(format!("Failed to get memory index: {}", e)))?;

            let output = if index.is_empty() {
                "No memory entries found.".to_string()
            } else {
                format!("Memory index ({} entries):\n{}", index.len(), index.join("\n"))
            };

            let tokens = count_tokens(&output);
            let entry_count = index.len();
            (output, tokens, entry_count)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_index", tokens, entry_count, false).await;
        tracing::debug!("mdkb_memory_index: {} tokens, {} entries", tokens, entry_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve a memory entry by ID.
    #[tool(description = "Retrieve full content of a memory entry by ID")]
    async fn memory_get(
        &self,
        Parameters(params): Parameters<MemoryGetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let entry = memory::get_entry(&ctx.conn, &params.id)
                .map_err(|e| mcp_error(format!("Failed to get memory entry: {}", e)))?
                .ok_or_else(|| mcp_error(format!("Memory entry not found: {}", params.id)))?;

            let output = format!(
                "# {} ({})\n\nType: {} | Status: {} | Tags: {}\nAccessed: {} times\n\n{}",
                entry.title,
                entry.id,
                entry.entry_type,
                entry.status,
                if entry.tags.is_empty() {
                    "none".to_string()
                } else {
                    entry.tags.join(", ")
                },
                entry.access_count,
                entry.content
            );

            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_get", tokens, 1, false).await;
        tracing::debug!("mdkb_memory_get: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Write or update a memory entry.
    #[tool(description = "Create or update a memory entry. Use after: (1) solving a problem (type=problem, title=symptom), (2) making architectural decisions (type=decision, title=options), (3) learning patterns (type=topic, title=concept). Title max 50 chars, like a headline. ID should be slug format: 'auth-oauth2-pkce'.")]
    async fn memory_write(
        &self,
        Parameters(params): Parameters<MemoryWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            // Check if entry exists
            let existing = memory::get_entry(&ctx.conn, &params.id)
                .map_err(|e| mcp_error(format!("Failed to check existing entry: {}", e)))?;

            // Parse entry type
            let entry_type: memory::EntryType = params
                .entry_type
                .parse()
                .map_err(|e: String| mcp_error(e))?;

            let now = chrono::Utc::now().timestamp();

            let output = if let Some(mut existing_entry) = existing {
                // Update existing entry
                existing_entry.title = params.title.clone();
                existing_entry.content = params.content.clone();
                existing_entry.entry_type = entry_type;
                existing_entry.tags = params.tags.clone();
                memory::update_entry(&ctx.conn, &existing_entry)
                    .map_err(|e| mcp_error(format!("Failed to update memory entry: {}", e)))?;
                format!("Updated memory entry: {}", params.id)
            } else {
                // Create new entry
                let entry = memory::MemoryEntry {
                    id: params.id.clone(),
                    title: params.title.clone(),
                    content: params.content.clone(),
                    entry_type,
                    tags: params.tags.clone(),
                    status: memory::EntryStatus::Active,
                    created_at: now,
                    updated_at: now,
                    superseded_by: None,
                    access_count: 0,
                    last_accessed: None,
                    source_path: None,
                };
                memory::add_entry(&ctx.conn, &entry)
                    .map_err(|e| mcp_error(format!("Failed to create memory entry: {}", e)))?;
                format!("Created memory entry: {}", params.id)
            };

            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_write", tokens, 1, false).await;
        tracing::debug!("mdkb_memory_write: {}", output);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Search memory entries.
    #[tool(description = "Search memory entries by keyword")]
    async fn memory_search(
        &self,
        Parameters(params): Parameters<MemorySearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens, result_count) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let entries = memory::search_entries(&ctx.conn, &params.query, params.limit)
                .map_err(|e| mcp_error(format!("Failed to search memory: {}", e)))?;

            let output = if entries.is_empty() {
                "No matching memory entries found.".to_string()
            } else {
                let mut out = format!("Found {} memory entries:\n\n", entries.len());
                for entry in &entries {
                    out.push_str(&format!(
                        "- [{}] {} ({}): {}\n",
                        entry.id,
                        entry.title,
                        entry.entry_type,
                        truncate_text(&entry.content, 100)
                    ));
                }
                out
            };

            let tokens = count_tokens(&output);
            let result_count = entries.len();
            (output, tokens, result_count)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_search", tokens, result_count, false).await;
        tracing::debug!("mdkb_memory_search: {} tokens, {} results", tokens, result_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Query document evolution history.
    #[tool(description = "Trace document evolution - what supersedes it, what it supersedes. Use when checking if a document is current or finding latest version.")]
    async fn evolution(
        &self,
        Parameters(params): Parameters<EvolutionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            // Resolve document path to ID
            let doc = resolve_document(&ctx.conn, &params.path)
                .map_err(|e| mcp_error(format!("Document not found: {}", e)))?;

            let mut output = format!("Evolution for {}:\n\n", params.path);

            // Get ancestors (what this document supersedes/updates)
            let show_ancestors = matches!(
                params.direction,
                EvolutionDirection::Ancestors | EvolutionDirection::Both
            );

            // Get descendants (what supersedes/updates this document)
            let show_descendants = matches!(
                params.direction,
                EvolutionDirection::Descendants | EvolutionDirection::Both
            );

            if show_ancestors {
                output.push_str("Ancestors (what this supersedes):\n");
                let ancestors = evolution::get_evolution_chain(&ctx.conn, doc.id)
                    .map_err(|e| mcp_error(format!("Failed to get ancestors: {}", e)))?;

                if ancestors.is_empty() {
                    output.push_str("  (none - this may be an original document)\n");
                } else {
                    for evo in &ancestors {
                        // Get target document info
                        if let Ok(Some(target)) = documents::get_document(&ctx.conn, evo.target_doc_id) {
                            output.push_str(&format!(
                                "  └── {} ({}, {})\n",
                                target.relative_path,
                                evo.relationship,
                                format_timestamp(evo.created_at)
                            ));
                            if let Some(ref scope) = evo.scope {
                                output.push_str(&format!("      Scope: {}\n", scope));
                            }
                            if let Some(ref reason) = evo.reason {
                                output.push_str(&format!("      Reason: {}\n", reason));
                            }
                        }
                    }
                }
                output.push('\n');
            }

            if show_descendants {
                output.push_str("Descendants (what supersedes this):\n");
                let descendants = evolution::get_superseded_by(&ctx.conn, doc.id)
                    .map_err(|e| mcp_error(format!("Failed to get descendants: {}", e)))?;

                if descendants.is_empty() {
                    output.push_str("  (none - this is the current version)\n");
                } else {
                    for evo in &descendants {
                        // Get source document info (the one that supersedes this)
                        if let Ok(Some(source)) = documents::get_document(&ctx.conn, evo.source_doc_id) {
                            output.push_str(&format!(
                                "  └── {} ({}, {})\n",
                                source.relative_path,
                                evo.relationship,
                                format_timestamp(evo.created_at)
                            ));
                            if let Some(ref scope) = evo.scope {
                                output.push_str(&format!("      Scope: {}\n", scope));
                            }
                            if let Some(ref reason) = evo.reason {
                                output.push_str(&format!("      Reason: {}\n", reason));
                            }
                        }
                    }
                }
                output.push('\n');
            }

            // Get document status
            if let Ok(Some((status, reason))) = evolution::get_document_status(&ctx.conn, doc.id) {
                output.push_str(&format!("Status: {:?}\n", status));
                if let Some(r) = reason {
                    output.push_str(&format!("Status reason: {}\n", r));
                }
            }

            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.record_persistent_call("evolution", tokens, 1, false).await;
        tracing::debug!("mdkb_evolution: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}

/// Resolve a document by path or ID.
fn resolve_document(conn: &rusqlite::Connection, path_or_id: &str) -> crate::error::Result<crate::domain::Document> {
    // Try to parse as ID first
    if let Ok(id) = path_or_id.parse::<i64>() {
        if let Some(doc) = documents::get_document(conn, id)? {
            return Ok(doc);
        }
    }

    // Try as path in all collections
    let all_collections = collections::list_collections(conn)?;
    for coll in &all_collections {
        if let Some(doc) = documents::get_document_by_path(conn, &coll.name, path_or_id)? {
            return Ok(doc);
        }
    }

    Err(crate::error::Error::from(crate::error::ErrorKind::DocumentNotFound {
        id: path_or_id.to_string(),
    }))
}

/// Format a Unix timestamp as an ISO date string.
fn format_timestamp(timestamp: i64) -> String {
    use chrono::{DateTime, Utc};
    DateTime::<Utc>::from_timestamp(timestamp, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: Default::default(),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "mdkb".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: self.warmup_instructions.clone(),
        }
    }
}

/// Run the MCP server on stdio with file watching.
pub async fn run_server(root: PathBuf) -> crate::error::Result<()> {
    // Load config if available
    let config_path = root.join(".mdkb/config.toml");
    let mcp_config = if config_path.exists() {
        match crate::Config::load(&config_path) {
            Ok(config) => config.mcp,
            Err(e) => {
                tracing::warn!("Failed to load config, using defaults: {}", e);
                McpConfig::default()
            }
        }
    } else {
        McpConfig::default()
    };

    tracing::info!(
        "MCP config: max_response_tokens={}, max_document_tokens={}",
        mcp_config.max_response_tokens,
        mcp_config.max_document_tokens
    );

    // Load memory warmup before starting server
    let warmup_limit = 50; // TODO: get from memory config when added
    let warmup = load_memory_warmup(&root, warmup_limit);

    let server = McpServer::with_warmup(root.clone(), mcp_config, warmup);
    let (stdin, stdout) = rmcp::transport::io::stdio();

    tracing::info!("Starting mdkb MCP server...");

    // Start file watcher in background
    let watcher_root = root.clone();
    let watcher_ctx = server.ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = run_file_watcher(watcher_root, watcher_ctx).await {
            tracing::error!("File watcher error: {}", e);
        }
    });

    let service = server
        .serve((stdin, stdout))
        .await
        .map_err(|e| crate::error::Error::mcp(format!("Failed to start server: {e}")))?;

    service
        .waiting()
        .await
        .map_err(|e| crate::error::Error::mcp(format!("Server error: {e}")))?;

    Ok(())
}

/// Run the file watcher and trigger reindex on changes.
async fn run_file_watcher(
    root: PathBuf,
    ctx: Arc<Mutex<Option<Context>>>,
) -> crate::error::Result<()> {
    let config = WatcherConfig::default();
    let mut watcher = FileWatcher::new(config)?;

    // Open context to get collection paths
    let ctx_guard = ctx.lock().await;
    if ctx_guard.is_none() {
        // Context not yet initialized, will be initialized on first tool call
        drop(ctx_guard);
        return Ok(());
    }

    let collection_list = {
        let ctx_ref = ctx_guard.as_ref().unwrap();
        collections::list_collections(&ctx_ref.conn)?
    };
    drop(ctx_guard);

    // Watch all collection paths
    for coll in &collection_list {
        let path = root.join(&coll.path);
        if path.exists() {
            if let Err(e) = watcher.watch(&path) {
                tracing::warn!("Failed to watch {}: {}", path.display(), e);
            } else {
                tracing::info!("Watching collection '{}' at {}", coll.name, path.display());
            }
        }
    }

    // Process file changes
    while let Some(change) = watcher.recv().await {
        tracing::debug!("File change detected: {:?}", change.path);

        // Re-acquire context and trigger update
        let mut ctx_guard = ctx.lock().await;
        if let Some(ctx_ref) = ctx_guard.as_mut() {
            // Check if the changed file matches any collection pattern
            match handle_update(ctx_ref, &root) {
                Ok(result) => {
                    if result.added > 0 || result.updated > 0 || result.removed > 0 {
                        tracing::info!(
                            "Reindexed: {} added, {} updated, {} removed",
                            result.added,
                            result.updated,
                            result.removed
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("Reindex failed: {}", e);
                }
            }
        }
    }

    Ok(())
}

/// Build warmup instructions from memory index.
///
/// Creates a compact manifest of available memories for AI context injection.
/// Format optimized for token efficiency (~30 tokens per entry).
fn build_warmup_instructions(index: &[String]) -> String {
    if index.is_empty() {
        return String::new();
    }

    let mut instructions = String::from(
        "# mdkb Memory Index\n\n\
         You have access to persistent memory entries. Use these tools:\n\
         - `memory_get(id)`: Get full content of an entry\n\
         - `memory_write(id, title, content, type, tags)`: Save new knowledge\n\
         - `memory_search(query)`: Find entries beyond this index\n\n\
         ## When to Write Memories\n\
         - After solving a problem: type=problem, title=symptom\n\
         - After making architectural decisions: type=decision, title=options\n\
         - After learning important patterns: type=topic, title=concept\n\n\
         ## Available Memories\n\n",
    );

    for entry in index {
        instructions.push_str(entry);
        instructions.push('\n');
    }

    instructions.push_str("\nUse `memory_get(id)` to retrieve full content.\n");

    instructions
}

/// Load memory warmup from database.
///
/// Returns formatted instructions for MCP server, or None if no memories exist.
fn load_memory_warmup(root: &std::path::Path, limit: usize) -> Option<String> {
    // Try to open context
    let ctx = match Context::open(root) {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::debug!("No memory warmup: {}", e);
            return None;
        }
    };

    // Initialize schema if needed (for memory table)
    if let Err(e) = crate::store::schema::init_schema(&ctx.conn) {
        tracing::warn!("Failed to init schema for warmup: {}", e);
        return None;
    }

    // Get warmup index
    let index = match memory::get_warmup_index(&ctx.conn, limit) {
        Ok(idx) => idx,
        Err(e) => {
            tracing::debug!("No memory index: {}", e);
            return None;
        }
    };

    if index.is_empty() {
        return None;
    }

    let instructions = build_warmup_instructions(&index);
    let tokens = count_tokens(&instructions);
    tracing::info!(
        "Memory warmup: {} entries, ~{} tokens",
        index.len(),
        tokens
    );

    Some(instructions)
}

/// Truncate text to a maximum length with ellipsis.
fn truncate_text(text: &str, max_len: usize) -> String {
    let text = text.replace('\n', " ");
    if text.len() <= max_len {
        text
    } else {
        format!("{}...", &text[..max_len.saturating_sub(3)])
    }
}

/// Format search results for output.
fn format_search_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }

    let mut output = String::new();
    for r in results {
        let title = r.title.as_deref().unwrap_or("(untitled)");
        output.push_str(&format!(
            "[{}] {}:{} - {} (score: {:.2})\n",
            r.id, r.collection, r.path, title, r.score
        ));
        for snippet in &r.snippets {
            output.push_str(&format!("  {}\n", snippet));
        }
    }
    output
}

/// Apply line range to content.
fn apply_line_range(content: &str, range: &str) -> Result<String, McpError> {
    let parts: Vec<&str> = range.split(':').collect();
    if parts.len() != 2 {
        return Err(mcp_error(format!(
            "Invalid line range: '{}', expected 'start:end'",
            range
        )));
    }

    let start: usize = parts[0]
        .parse()
        .map_err(|_| mcp_error(format!("Invalid start line: '{}'", parts[0])))?;
    let end: usize = parts[1]
        .parse()
        .map_err(|_| mcp_error(format!("Invalid end line: '{}'", parts[1])))?;

    if start == 0 {
        return Err(mcp_error("Line numbers start at 1"));
    }

    let lines: Vec<&str> = content.lines().collect();
    let start_idx = start.saturating_sub(1);
    let end_idx = end.min(lines.len());

    if start_idx >= lines.len() {
        return Ok(String::new());
    }

    Ok(lines[start_idx..end_idx].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_search_results_empty() {
        let results: Vec<SearchResult> = vec![];
        let output = format_search_results(&results);
        assert_eq!(output, "No results found.");
    }

    #[test]
    fn test_format_search_results_with_results() {
        let results = vec![SearchResult {
            id: 1,
            collection: "docs".to_string(),
            path: "readme.md".to_string(),
            title: Some("README".to_string()),
            score: -5.5,
            snippets: vec!["...matching text...".to_string()],
            status: None,
            superseded_by: None,
        }];
        let output = format_search_results(&results);
        assert!(output.contains("[1]"));
        assert!(output.contains("docs"));
        assert!(output.contains("readme.md"));
        assert!(output.contains("README"));
    }

    #[test]
    fn test_apply_line_range_basic() {
        let content = "line 1\nline 2\nline 3\nline 4\nline 5";
        let result = apply_line_range(content, "2:4").unwrap();
        assert_eq!(result, "line 2\nline 3\nline 4");
    }

    #[test]
    fn test_apply_line_range_invalid() {
        let result = apply_line_range("content", "invalid");
        assert!(result.is_err());
    }

    #[test]
    fn test_apply_line_range_zero_start() {
        let result = apply_line_range("content", "0:5");
        assert!(result.is_err());
    }

    #[test]
    fn test_build_warmup_instructions_empty() {
        let index: Vec<String> = vec![];
        let result = build_warmup_instructions(&index);
        assert!(result.is_empty());
    }

    #[test]
    fn test_build_warmup_instructions_with_entries() {
        let index = vec![
            "auth-oauth2: OAuth2 PKCE implementation #auth #security".to_string(),
            "bug-null-email: Null email panic fix #bug #users".to_string(),
        ];
        let result = build_warmup_instructions(&index);

        // Check structure
        assert!(result.contains("# mdkb Memory Index"));
        assert!(result.contains("memory_get"));
        assert!(result.contains("memory_write"));
        assert!(result.contains("memory_search"));

        // Check guidance
        assert!(result.contains("When to Write Memories"));
        assert!(result.contains("type=problem"));
        assert!(result.contains("type=decision"));
        assert!(result.contains("type=topic"));

        // Check entries included
        assert!(result.contains("auth-oauth2: OAuth2 PKCE implementation #auth #security"));
        assert!(result.contains("bug-null-email: Null email panic fix #bug #users"));
    }

    #[test]
    fn test_build_warmup_instructions_token_budget() {
        // 50 entries should be around 1.5K tokens
        let mut index = Vec::new();
        for i in 0..50 {
            index.push(format!("entry-{i}: Some title for entry number {i} #tag1 #tag2"));
        }
        let result = build_warmup_instructions(&index);
        let tokens = count_tokens(&result);

        // Should be under 2K tokens for 50 entries
        assert!(tokens < 2000, "Warmup exceeds token budget: {} tokens", tokens);
    }

    /// Regression test for deadlock in MCP tool handlers.
    ///
    /// The MCP server was previously deadlocking because tool handlers held
    /// the ctx Mutex lock while calling record_persistent_call(), which also
    /// tried to acquire the same lock. tokio::Mutex is not reentrant, so this
    /// caused a deadlock.
    ///
    /// This test verifies that calling tools multiple times in succession
    /// completes within a reasonable timeout (doesn't deadlock).
    #[tokio::test]
    async fn test_mcp_tools_no_deadlock() {
        use std::time::Duration;
        use rmcp::model::EmptyObject;

        // Create temp directory and initialize mdkb
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

        // Create and initialize MCP server
        let server = McpServer::new(root);

        // Call status multiple times - this would deadlock before the fix
        // because status() calls ensure_context() and record_persistent_call(),
        // both of which acquire the ctx lock.
        let timeout_duration = Duration::from_secs(5);

        for i in 0..3 {
            let result = tokio::time::timeout(
                timeout_duration,
                server.status(Parameters(EmptyObject {})),
            )
            .await;

            match result {
                Ok(Ok(_)) => {} // Success
                Ok(Err(e)) => panic!("Tool call {} failed with error: {:?}", i, e),
                Err(_) => panic!(
                    "Tool call {} timed out after {:?} - likely deadlock!",
                    i, timeout_duration
                ),
            }
        }
    }

    /// Test that multiple different tool calls don't deadlock.
    #[tokio::test]
    async fn test_mcp_multiple_tools_no_deadlock() {
        use std::time::Duration;
        use rmcp::model::EmptyObject;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

        let server = McpServer::new(root);
        let timeout_duration = Duration::from_secs(5);

        // Call different tools in sequence
        let tools_to_test = vec![
            "status",
            "list_collections",
            "status",
        ];

        for (i, tool_name) in tools_to_test.iter().enumerate() {
            let result = match *tool_name {
                "status" => {
                    tokio::time::timeout(
                        timeout_duration,
                        server.status(Parameters(EmptyObject {})),
                    )
                    .await
                    .map(|r| r.map(|_| ()))
                }
                "list_collections" => {
                    tokio::time::timeout(
                        timeout_duration,
                        server.list_collections(Parameters(EmptyObject {})),
                    )
                    .await
                    .map(|r| r.map(|_| ()))
                }
                _ => unreachable!(),
            };

            match result {
                Ok(Ok(_)) => {} // Success
                Ok(Err(e)) => panic!("Tool {} ({}) failed: {:?}", tool_name, i, e),
                Err(_) => panic!(
                    "Tool {} ({}) timed out - likely deadlock!",
                    tool_name, i
                ),
            }
        }
    }
}
