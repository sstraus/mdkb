//! MCP server implementation.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::handler::server::tool::{Parameters, ToolRouter};
use rmcp::model::{
    CallToolResult, Content, ErrorCode, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{ErrorData as McpError, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::cli::handlers::{Context, handle_hybrid_search, handle_mget, handle_update};
use crate::config::McpConfig;
use crate::domain::SearchResult;
use crate::metrics::{count_tokens, truncate_with_continuation, truncate_with_ellipsis, UsageMetrics};
use crate::store::{collections, documents, memory, search, stats};
use crate::watcher::{FileWatcher, WatcherConfig};

use super::tools::{
    GetParams, MemoryGetParams, MemoryIndexParams, MemorySearchParams, MemoryWriteParams,
    MultiGetParams, SearchParams,
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
        }
    }

    /// Get the usage metrics.
    pub fn metrics(&self) -> &UsageMetrics {
        &self.metrics
    }

    /// Apply token limit to output, truncating if necessary.
    fn apply_token_limit(&self, output: String) -> String {
        let max = self.config.max_response_tokens;
        if max == 0 {
            return output;
        }

        let tokens = count_tokens(&output);
        if tokens <= max {
            return output;
        }

        if self.config.truncate_with_ellipsis {
            truncate_with_ellipsis(&output, max)
        } else {
            crate::metrics::tokens::truncate_to_tokens(&output, max).0
        }
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
    async fn mdkb_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let ctx_guard = self.ctx.lock().await;
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;

        let results = handle_hybrid_search(
            ctx,
            &params.query,
            params.limit,
            params.collection.as_deref(),
        )
        .map_err(|e| mcp_error(format!("Search failed: {}", e)))?;

        let output = format_search_results(&results);
        let tokens = count_tokens(&output);
        let result_count = results.len();
        self.metrics.record_search(tokens, result_count);
        self.record_persistent_call("search", tokens, result_count, false).await;
        tracing::debug!("mdkb_search: {} tokens, {} results", tokens, result_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve a document by ID or path.
    #[tool(description = "Retrieve a document by ID or path, with optional line range")]
    async fn mdkb_get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        let max_tokens = self.config.max_response_tokens;
        let (output, truncated) = if max_tokens > 0 {
            let result = truncate_with_continuation(&output, max_tokens, doc.id);
            (result.content, result.truncated)
        } else {
            (output, false)
        };

        let tokens = count_tokens(&output);
        self.metrics.record_get(tokens);
        self.record_persistent_call("get", tokens, 1, truncated).await;
        tracing::debug!("mdkb_get: {} tokens, truncated={}", tokens, truncated);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// List all collections.
    #[tool(description = "List all indexed collections with their paths and document counts")]
    async fn mdkb_list_collections(&self) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        self.record_persistent_call("list_collections", tokens, coll_list.len(), false).await;
        tracing::debug!("mdkb_list_collections: {} tokens, {} collections", tokens, coll_list.len());

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Get index status.
    #[tool(description = "Get the current index status (collections, documents, etc.)")]
    async fn mdkb_status(&self) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        self.metrics.record_status(tokens);
        self.record_persistent_call("status", tokens, 1, false).await;
        tracing::debug!("mdkb_status: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Trigger reindex of all collections.
    #[tool(description = "Trigger a differential reindex of all collections")]
    async fn mdkb_update(&self) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        self.metrics.record_update(tokens);
        self.record_persistent_call("update", tokens, 1, false).await;
        tracing::debug!("mdkb_update: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve multiple documents by pattern.
    #[tool(description = "Retrieve multiple documents matching a glob pattern")]
    async fn mdkb_multi_get(
        &self,
        Parameters(params): Parameters<MultiGetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        let doc_limit = self.config.max_document_tokens;

        for (doc, content) in &results {
            let title = doc.title.as_deref().unwrap_or("(untitled)");

            // Apply per-document token limit if configured
            let truncated_content = if doc_limit > 0 {
                let content_tokens = count_tokens(content);
                if content_tokens > doc_limit {
                    if self.config.truncate_with_ellipsis {
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

        // Apply overall response limit
        let original_len = output.len();
        let output = self.apply_token_limit(output);
        let truncated = output.len() < original_len;
        let tokens = count_tokens(&output);
        let result_count = results.len();
        self.metrics.record_multi_get(tokens, result_count);
        self.record_persistent_call("multi_get", tokens, result_count, truncated).await;
        tracing::debug!("mdkb_multi_get: {} tokens, {} docs, truncated={}", tokens, result_count, truncated);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Get usage metrics for all tools.
    #[tool(description = "Get token usage metrics for all MCP tools (session and historical)")]
    async fn mdkb_metrics(&self) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let summary = self.metrics.summary();
        let mut output = format!(
            "=== Current Session ===\n\
             Total calls: {}\n\
             Total tokens: {}\n\
             Avg tokens/call: {:.1}\n\n\
             Tool breakdown:\n\
             - search:    {} calls, {} tokens ({:.1} avg)\n\
             - get:       {} calls, {} tokens ({:.1} avg)\n\
             - multi_get: {} calls, {} tokens ({:.1} avg)\n\
             - status:    {} calls, {} tokens ({:.1} avg)\n\
             - update:    {} calls, {} tokens ({:.1} avg)",
            summary.total_calls,
            summary.total_tokens,
            summary.avg_tokens_per_call,
            summary.search.call_count,
            summary.search.total_tokens,
            summary.search.avg_tokens_per_call,
            summary.get.call_count,
            summary.get.total_tokens,
            summary.get.avg_tokens_per_call,
            summary.multi_get.call_count,
            summary.multi_get.total_tokens,
            summary.multi_get.avg_tokens_per_call,
            summary.status.call_count,
            summary.status.total_tokens,
            summary.status.avg_tokens_per_call,
            summary.update.call_count,
            summary.update.total_tokens,
            summary.update.avg_tokens_per_call,
        );

        // Add historical stats from SQLite
        let ctx_guard = self.ctx.lock().await;
        if let Some(ctx) = ctx_guard.as_ref() {
            if let Ok(aggregate) = stats::get_aggregate_stats(&ctx.conn) {
                output.push_str(&format!(
                    "\n\n=== All-Time Stats ===\n\
                     Total sessions: {}\n\
                     Total calls: {}\n\
                     Total tokens: {}\n\
                     Total truncations: {}\n\
                     Avg tokens/call: {:.1}",
                    aggregate.total_sessions,
                    aggregate.total_calls,
                    aggregate.total_tokens,
                    aggregate.total_truncations,
                    aggregate.avg_tokens_per_call,
                ));
            }
        }

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Get memory warmup index (compact list for AI session start).
    #[tool(description = "Get memory index for session warmup (~50 entries, compact format). Call this at session start to load context.")]
    async fn mdkb_memory_index(
        &self,
        Parameters(params): Parameters<MemoryIndexParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        self.record_persistent_call("memory_index", tokens, index.len(), false).await;
        tracing::debug!("mdkb_memory_index: {} tokens, {} entries", tokens, index.len());

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve a memory entry by ID.
    #[tool(description = "Retrieve full content of a memory entry by ID")]
    async fn mdkb_memory_get(
        &self,
        Parameters(params): Parameters<MemoryGetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        self.record_persistent_call("memory_get", tokens, 1, false).await;
        tracing::debug!("mdkb_memory_get: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Write or update a memory entry.
    #[tool(description = "Create or update a memory entry. Use for persisting important knowledge across sessions.")]
    async fn mdkb_memory_write(
        &self,
        Parameters(params): Parameters<MemoryWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
            };
            memory::add_entry(&ctx.conn, &entry)
                .map_err(|e| mcp_error(format!("Failed to create memory entry: {}", e)))?;
            format!("Created memory entry: {}", params.id)
        };

        let tokens = count_tokens(&output);
        self.record_persistent_call("memory_write", tokens, 1, false).await;
        tracing::debug!("mdkb_memory_write: {}", output);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Search memory entries.
    #[tool(description = "Search memory entries by keyword")]
    async fn mdkb_memory_search(
        &self,
        Parameters(params): Parameters<MemorySearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

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
        self.record_persistent_call("memory_search", tokens, result_count, false).await;
        tracing::debug!("mdkb_memory_search: {} tokens, {} results", tokens, result_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
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
            instructions: None,
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

    let server = McpServer::with_config(root.clone(), mcp_config);
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
}
