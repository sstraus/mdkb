//! MCP server implementation.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, EmptyObject, ErrorCode, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::{ErrorData as McpError, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::cli::handlers::{Context, handle_hybrid_search, handle_mget, handle_update};
use crate::code::indexing::IndexFacade;
use crate::code::types::SymbolId;
use crate::config::McpConfig;
use crate::domain::SearchResult;
use crate::metrics::{count_tokens, truncate_with_continuation, truncate_with_ellipsis, UsageMetrics};
use crate::store::{collections, documents, evolution, memory, search, stats};
use crate::watcher::{FileWatcher, WatcherConfig};

use super::tools::{
    CodeGraphParams,
    GetParams, MemoryDeleteParams, MemoryWriteParams,
    SearchParams,
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
    /// Code intelligence index (Tantivy-backed).
    code_index: Arc<Mutex<Option<IndexFacade>>>,
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
            code_index: Arc::new(Mutex::new(None)),
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
            code_index: Arc::new(Mutex::new(None)),
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
    ///
    /// Auto-initializes `.mdkb/` if it doesn't exist, so the MCP server
    /// works out of the box without requiring a manual `mdkb init`.
    async fn ensure_context(&self) -> Result<(), McpError> {
        let mut ctx_guard = self.ctx.lock().await;
        if ctx_guard.is_none() {
            let ctx = match Context::open(&self.root) {
                Ok(ctx) => ctx,
                Err(e) if e.is_not_found() => {
                    tracing::info!("Auto-initializing mdkb at {}", self.root.display());
                    Context::init(&self.root)
                        .map_err(|e| mcp_error(format!("Failed to auto-initialize mdkb: {}", e)))?
                }
                Err(e) => return Err(mcp_error(format!("Failed to open database: {}", e))),
            };

            // Initialize stats schema
            stats::init_stats_schema(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to init stats schema: {}", e)))?;

            // Apply convention-based collection detection
            self.apply_conventions(&ctx)
                .map_err(|e| mcp_error(format!("Failed to apply conventions: {}", e)))?;

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

    /// Detect and register convention-based collections if enabled.
    fn apply_conventions(&self, ctx: &Context) -> std::result::Result<(), Box<dyn std::error::Error>> {
        let config = crate::config::Config::load_or_default(&ctx.config_path);
        if !config.conventions.enabled {
            return Ok(());
        }

        let existing = crate::store::collections::list_collections(&ctx.conn)?;
        let proposals = crate::domain::conventions::detect_conventions(&self.root, &existing);

        for proposal in &proposals {
            let coll = crate::domain::conventions::proposal_to_collection(proposal);
            crate::store::collections::add_collection(&ctx.conn, &coll)?;
            tracing::info!("Auto-detected collection: {} ({})", coll.name, coll.path);
        }

        Ok(())
    }

    /// Helper: retrieve document content with optional line range and evolution metadata.
    fn get_document_content(
        &self,
        ctx: &Context,
        doc: &crate::domain::Document,
        lines: &Option<String>,
    ) -> Result<String, McpError> {
        let content = documents::get_content(&ctx.conn, &doc.hash)
            .map_err(|e| mcp_error(format!("Failed to get content: {}", e)))?
            .ok_or_else(|| mcp_error("Content missing for document. Try `update` to reindex."))?;

        // Apply line range if specified
        let mut output = if let Some(range) = lines {
            apply_line_range(&content, range)?
        } else {
            content
        };

        // Append evolution metadata
        if let Ok(Some((status, reason))) = evolution::get_document_status(&ctx.conn, doc.id) {
            let status_str = format!("{:?}", status);
            if status_str != "Current" {
                output.push_str(&format!("\n\n---\n**Status:** {}", status_str));
                if let Some(r) = reason {
                    output.push_str(&format!(" ({})", r));
                }

                // Show what supersedes this document
                if let Ok(descendants) = evolution::get_superseded_by(&ctx.conn, doc.id) {
                    for evo in &descendants {
                        if let Ok(Some(source)) = documents::get_document(&ctx.conn, evo.source_doc_id) {
                            output.push_str(&format!(
                                "\n**Superseded by:** {} ({})",
                                source.relative_path, evo.relationship
                            ));
                        }
                    }
                }
            }
        }

        // Apply token limit with continuation guidance
        let max_tokens = self.config.max_response_tokens;
        let output = if max_tokens > 0 {
            truncate_with_continuation(&output, max_tokens, doc.id).content
        } else {
            output
        };

        Ok(output)
    }

    /// Helper: finish a document get by recording metrics.
    async fn finish_get(&self, output: String, tokens: usize) -> Result<CallToolResult, McpError> {
        self.metrics.record_get(tokens);
        self.record_persistent_call("get", tokens, 1, false).await;
        tracing::debug!("mdkb_get: {} tokens", tokens);
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve multiple documents by comma-separated IDs/paths/slugs.
    async fn get_batch(&self, ids: &str, lines: &Option<String>) -> Result<CallToolResult, McpError> {
        let ctx_guard = self.ctx.lock().await;
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;

        let mut output = String::new();
        let mut found = 0;

        for raw_id in ids.split(',') {
            let id = raw_id.trim();
            if id.is_empty() {
                continue;
            }

            // Try numeric ID
            if let Ok(numeric_id) = id.parse::<i64>() {
                if let Ok(Some(doc)) = documents::get_document(&ctx.conn, numeric_id) {
                    if let Ok(content) = self.get_document_content(ctx, &doc, lines) {
                        let title = doc.title.as_deref().unwrap_or("(untitled)");
                        output.push_str(&format!("=== [{}] {} - {} ===\n{}\n\n", doc.id, doc.relative_path, title, content));
                        found += 1;
                        continue;
                    }
                }
            }

            // Try path resolution
            if let Ok(doc) = resolve_document(&ctx.conn, id) {
                if let Ok(content) = self.get_document_content(ctx, &doc, lines) {
                    let title = doc.title.as_deref().unwrap_or("(untitled)");
                    output.push_str(&format!("=== [{}] {} - {} ===\n{}\n\n", doc.id, doc.relative_path, title, content));
                    found += 1;
                    continue;
                }
            }

            // Try memory slug
            if let Ok(Some(entry)) = memory::get_entry(&ctx.conn, id) {
                output.push_str(&format!(
                    "=== [MEM] {} - {} ===\n{}\n\n",
                    entry.id, entry.title, entry.content
                ));
                found += 1;
                continue;
            }

            output.push_str(&format!("=== {} ===\nNot found\n\n", id));
        }

        if found == 0 {
            return Err(mcp_error("None of the requested items were found. Use `search(query)` to find content."));
        }

        let tokens = count_tokens(&output);
        drop(ctx_guard);
        self.finish_get(output, tokens).await
    }

    /// Retrieve multiple documents matching a glob pattern.
    async fn get_glob(&self, pattern: &str) -> Result<CallToolResult, McpError> {
        let doc_limit = self.config.max_document_tokens;
        let truncate_ellipsis = self.config.truncate_with_ellipsis;
        let max_response_tokens = self.config.max_response_tokens;

        let (output, result_count) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let results = handle_mget(ctx, pattern, None)
                .map_err(|e| mcp_error(format!("Glob retrieval failed: {}", e)))?;

            if results.is_empty() {
                return Ok(CallToolResult::success(vec![Content::text(
                    "No documents matched pattern. Use `status` to see what's indexed, or `search(query)` to find by content.",
                )]));
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
        }; // ctx_guard dropped here

        let original_len = output.len();
        let output = if max_response_tokens > 0 {
            crate::metrics::tokens::truncate_to_tokens(&output, max_response_tokens).0
        } else {
            output
        };
        let truncated = output.len() < original_len;
        let tokens = count_tokens(&output);

        self.metrics.record_get(tokens);
        self.record_persistent_call("get", tokens, result_count, truncated).await;
        tracing::debug!("mdkb_get_glob: {} tokens, {} docs, truncated={}", tokens, result_count, truncated);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Initialize the code intelligence index (Tantivy-backed).
    ///
    /// Opens or creates the index at `.mdkb/code-index/`.
    async fn ensure_code_index(&self) -> Result<(), McpError> {
        let mut idx_guard = self.code_index.lock().await;
        if idx_guard.is_none() {
            let index_path = self.root.join(".mdkb/code-index");
            let facade = IndexFacade::open_or_create(&index_path)
                .map_err(|e| mcp_error(format!("Failed to open code index: {}", e)))?;
            *idx_guard = Some(facade);
        }
        Ok(())
    }

    /// Resolve a symbol by ID or name, returning an error for disambiguation.
    ///
    /// If `symbol_id` is provided, looks up by ID directly.
    /// If only `name` is provided, finds all matches. Returns an error with
    /// a disambiguation list if multiple symbols share the name.
    fn resolve_symbol(
        facade: &IndexFacade,
        name: &str,
        symbol_id: Option<u32>,
    ) -> Result<crate::code::symbol::Symbol, McpError> {
        if let Some(id) = symbol_id {
            let sid = SymbolId::new(id)
                .ok_or_else(|| mcp_error("Invalid symbol_id: 0 is reserved. Use `search(query, scope=\"symbols\")` to get valid IDs."))?;
            return facade
                .get_symbol(sid)
                .ok_or_else(|| mcp_error(format!("Symbol not found: sym#{id}. Use `search(query, scope=\"symbols\")` to get current IDs.")));
        }

        let matches = facade.find_symbols_by_name(name);
        match matches.len() {
            0 => Err(mcp_error(format!("No symbol found with name '{name}'. Try `search(query, scope=\"symbols\")` for fuzzy matching."))),
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => {
                let mut msg = format!(
                    "Multiple symbols found for '{}'. Use symbol_id to disambiguate:\n",
                    name
                );
                for sym in &matches {
                    msg.push_str(&format!(
                        "  sym#{} - {:?} {} in {} ({})\n",
                        sym.id.value(),
                        sym.kind,
                        sym.name,
                        sym.file_path,
                        sym.range,
                    ));
                }
                Err(mcp_error(msg))
            }
        }
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

        let scope = params.scope.as_deref().unwrap_or("docs");

        let (output, tokens, result_count) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            match scope {
                "docs" => {
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
                    (output, tokens, results.len())
                }
                "memory" => {
                    let entries = memory::search_entries(&ctx.conn, &params.query, params.limit)
                        .map_err(|e| mcp_error(format!("Memory search failed: {}", e)))?;

                    let output = format_memory_search_results(&entries);
                    let tokens = count_tokens(&output);
                    (output, tokens, entries.len())
                }
                "all" => {
                    let doc_results = handle_hybrid_search(
                        ctx,
                        &params.query,
                        params.limit,
                        params.collection.as_deref(),
                        params.include_superseded,
                    )
                    .map_err(|e| mcp_error(format!("Search failed: {}", e)))?;

                    let mem_entries = memory::search_entries(&ctx.conn, &params.query, params.limit)
                        .map_err(|e| mcp_error(format!("Memory search failed: {}", e)))?;

                    let total = doc_results.len() + mem_entries.len();
                    let mut output = format!("Found {} results:\n\n", total);

                    if !doc_results.is_empty() {
                        output.push_str("## Documents\n\n");
                        for r in &doc_results {
                            output.push_str(&format!(
                                "- [DOC] {} (score: {:.2})\n",
                                r.path, r.score
                            ));
                            if let Some(ref title) = r.title {
                                output.push_str(&format!("  Title: {}\n", title));
                            }
                        }
                        output.push('\n');
                    }

                    if !mem_entries.is_empty() {
                        output.push_str("## Memory Entries\n\n");
                        for entry in &mem_entries {
                            output.push_str(&format!(
                                "- [MEM] {} ({}): {}\n",
                                entry.title,
                                entry.id,
                                truncate_text(&entry.content, 100)
                            ));
                        }
                    }

                    if total == 0 {
                        output = "No results found in documents or memory. Try a different query.".to_string();
                    }

                    let tokens = count_tokens(&output);
                    (output, tokens, total)
                }
                "code" | "symbols" => {
                    // Drop ctx_guard before acquiring code_index lock
                    drop(ctx_guard);

                    {
                        self.ensure_code_index().await?;

                        let idx_guard = self.code_index.lock().await;
                        let facade = idx_guard
                            .as_ref()
                            .ok_or_else(|| mcp_error("Code index not initialized"))?;

                        if scope == "code" {
                            // Semantic search (embedding similarity)
                            let mut results = facade
                                .semantic_search(&params.query, params.limit, params.threshold)
                                .map_err(|e| mcp_error(format!("Semantic code search failed: {e}")))?;

                            // Apply kind filter
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
                                let output = "No semantic matches found. Try broader terms, lower the threshold, or use scope='symbols' for keyword search.".to_string();
                                let tokens = count_tokens(&output);
                                (output, tokens, 0)
                            } else {
                                let mut out = format!("Found {} semantic match(es):\n\n", results.len());
                                for (sym, score) in &results {
                                    out.push_str(&format_symbol(sym));
                                    out.push_str(&format!("    Similarity: {:.3}\n", score));
                                    out.push('\n');
                                }
                                let result_count = results.len();
                                let tokens = count_tokens(&out);
                                (out, tokens, result_count)
                            }
                        } else {
                            // symbols scope: fuzzy text match
                            let mut symbols = facade.search_symbols(&params.query, params.limit);

                            // Apply kind filter
                            if let Some(ref kind_str) = params.kind {
                                if let Ok(kind) = kind_str.parse::<crate::code::types::SymbolKind>() {
                                    symbols.retain(|s| s.kind == kind);
                                } else {
                                    return Err(mcp_error(format!(
                                        "Unknown symbol kind: '{kind_str}'. Valid kinds: function, method, struct, enum, trait, interface, class, module, variable, constant, field, parameter, type_alias, macro"
                                    )));
                                }
                            }

                            // Apply file filter (substring match)
                            if let Some(ref file_pattern) = params.file {
                                symbols.retain(|s| s.file_path.contains(file_pattern.as_str()));
                            }

                            if symbols.is_empty() {
                                let output = "No symbols found. Try different search terms, scope='code' for semantic matching, or check `status` to verify the code index has content.".to_string();
                                let tokens = count_tokens(&output);
                                (output, tokens, 0)
                            } else {
                                let mut out = format!("Found {} symbol(s):\n\n", symbols.len());
                                for sym in &symbols {
                                    out.push_str(&format_symbol(sym));
                                    out.push('\n');
                                }
                                let result_count = symbols.len();
                                let tokens = count_tokens(&out);
                                (out, tokens, result_count)
                            }
                        }
                    }
                }
                _ => {
                    return Err(mcp_error(format!(
                        "Invalid scope: '{}'. Valid values: 'docs' (default), 'memory', 'all', 'code', 'symbols'.",
                        scope
                    )));
                }
            }
        }; // ctx_guard dropped here before record_persistent_call

        self.metrics.record_search(tokens, result_count);
        self.record_persistent_call("search", tokens, result_count, false).await;
        tracing::debug!("mdkb_search: {} tokens, {} results", tokens, result_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve a document by ID or path, with optional line range.
    /// Also accepts memory slugs, glob patterns, and comma-separated lists.
    #[tool(description = "Retrieve a document by ID or path, with optional line range. Also accepts memory slugs (e.g., 'auth-oauth2-pkce') to retrieve memory entries.")]
    async fn get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let id = &params.id;

        // Comma-separated list → batch retrieve
        if id.contains(',') {
            return self.get_batch(id, &params.lines).await;
        }

        // Glob pattern → batch retrieve by pattern
        if id.contains('*') || id.contains('?') {
            return self.get_glob(id).await;
        }

        let ctx_guard = self.ctx.lock().await;
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;

        // Resolution strategy:
        // 1. Numeric ID → document lookup
        // 2. Contains / or . → path resolution across collections
        // 3. Slug (hyphens, no slashes/dots) → memory entry lookup
        // 4. Fallback: try full resolve_document for edge cases

        // Try numeric ID first
        if let Ok(numeric_id) = id.parse::<i64>() {
            if let Some(doc) = documents::get_document(&ctx.conn, numeric_id)
                .map_err(|e| mcp_error(format!("Failed to get document: {}", e)))? {
                let output = self.get_document_content(ctx, &doc, &params.lines)?;
                let tokens = count_tokens(&output);
                drop(ctx_guard);
                return self.finish_get(output, tokens).await;
            }
        }

        // Try path resolution (contains / or .)
        if id.contains('/') || id.contains('.') {
            if let Ok(doc) = resolve_document(&ctx.conn, id) {
                let output = self.get_document_content(ctx, &doc, &params.lines)?;
                let tokens = count_tokens(&output);
                drop(ctx_guard);
                return self.finish_get(output, tokens).await;
            }
        }

        // Try memory slug
        if let Ok(Some(entry)) = memory::get_entry(&ctx.conn, id) {
            let output = format!(
                "# {} ({})\n\nType: {} | Status: {} | Tags: {}\nAccessed: {} times\n\n{}",
                entry.title,
                entry.id,
                entry.entry_type,
                entry.status,
                if entry.tags.is_empty() { "none".to_string() } else { entry.tags.join(", ") },
                entry.access_count,
                entry.content
            );
            let tokens = count_tokens(&output);
            drop(ctx_guard);
            self.record_persistent_call("get", tokens, 1, false).await;
            return Ok(CallToolResult::success(vec![Content::text(output)]));
        }

        // Fallback: try full resolve_document for edge cases
        if let Ok(doc) = resolve_document(&ctx.conn, id) {
            let output = self.get_document_content(ctx, &doc, &params.lines)?;
            let tokens = count_tokens(&output);
            drop(ctx_guard);
            return self.finish_get(output, tokens).await;
        }

        Err(mcp_error(format!(
            "Not found: '{}'. Accepts: numeric document ID, file path (e.g., 'docs/api.md'), or memory slug (e.g., 'auth-oauth2'). Use `search(query)` to find content.",
            params.id
        )))
    }

    /// Get index status including documents, collections, and code index.
    #[tool(description = "Get the current index status (collections, documents, etc.) including code index stats when available")]
    async fn status(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let mut output = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let index_status = search::get_status(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to get status: {}", e)))?;

            let mut output = format!(
                "## Index Status\n\nDocuments: {}\nStale: {}\nDB Size: {} bytes\n",
                index_status.documents, index_status.stale_documents, index_status.db_size_bytes
            );

            // Collection listing with source tags
            let coll_list = collections::list_collections(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to list collections: {}", e)))?;

            output.push_str(&format!("\n## Collections ({})\n\n", coll_list.len()));
            if coll_list.is_empty() {
                output.push_str("No collections configured. Markdown files are indexed via collections (use CLI: `mdkb collection add <name> <path>`).\n");
            } else {
                for coll in &coll_list {
                    let doc_count = collections::get_collection_document_count(&ctx.conn, &coll.name)
                        .unwrap_or(0);
                    let source_tag = if coll.source == "convention" { "[convention]" } else { "[manual]" };
                    output.push_str(&format!(
                        "- {} {} ({}): {} docs, pattern: {}\n",
                        coll.name, source_tag, coll.path, doc_count, coll.pattern
                    ));
                }
            }

            output
        }; // ctx_guard dropped here

        // Always show code index stats (initialize if needed)
        {
            // Try to open the code index for stats
            let _ = self.ensure_code_index().await;
            let idx_guard = self.code_index.lock().await;
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

        let tokens = count_tokens(&output);
        self.metrics.record_status(tokens);
        self.record_persistent_call("status", tokens, 1, false).await;
        tracing::debug!("mdkb_status: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Reindex everything: documents (from collections) and source code (from project root).
    #[tool(description = "Trigger a differential reindex of all collections")]
    async fn update(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        // Phase 1: Reindex documents (markdown collections)
        let doc_output = {
            let mut ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_mut()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let result = handle_update(ctx, &self.root)
                .map_err(|e| mcp_error(format!("Document update failed: {}", e)))?;

            format!(
                "## Documents\n\nAdded: {}\nUpdated: {}\nRemoved: {}\nUnchanged: {}",
                result.added, result.updated, result.removed, result.unchanged
            )
        }; // ctx_guard dropped here

        // Phase 2: Reindex source code (tree-sitter + Tantivy)
        let code_output = {
            self.ensure_code_index().await?;
            let mut idx_guard = self.code_index.lock().await;
            if let Some(facade) = idx_guard.as_mut() {
                match facade.reindex(&self.root) {
                    Ok(stats) => {
                        format!(
                            "\n\n## Code\n\nFiles: {}\nSymbols: {}\nRelationships: {}",
                            stats.files_indexed, stats.symbols_indexed, stats.relationships_collected
                        )
                    }
                    Err(e) => {
                        tracing::error!("Code reindex failed: {}", e);
                        format!("\n\n## Code\n\nReindex failed: {}", e)
                    }
                }
            } else {
                String::new()
            }
        }; // idx_guard dropped here

        let output = format!("{}{}", doc_output, code_output);
        let tokens = count_tokens(&output);
        self.metrics.record_update(tokens);
        self.record_persistent_call("update", tokens, 1, false).await;
        tracing::debug!("mdkb_update: {} tokens", tokens);

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
                .map_err(|e: String| mcp_error(format!("{e}. Valid types: topic, problem, decision")))?;

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


    /// Delete a memory entry by ID.
    #[tool(description = "Delete a memory entry permanently. Use `search(query, scope=\"memory\")` to find entry IDs.")]
    async fn memory_delete(
        &self,
        Parameters(params): Parameters<MemoryDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let (output, tokens) = {
            let ctx_guard = self.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let deleted = memory::delete_entry(&ctx.conn, &params.id)
                .map_err(|e| mcp_error(format!("Failed to delete memory entry: {}", e)))?;

            let output = if deleted {
                format!("Deleted memory entry '{}'.", params.id)
            } else {
                format!("Memory entry '{}' not found. Use `search(query, scope=\"memory\")` to find entries.", params.id)
            };
            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_delete", tokens, 1, false).await;
        tracing::debug!("mdkb_memory_delete: {}", output);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    // -----------------------------------------------------------------------
    // Code intelligence tools
    // -----------------------------------------------------------------------

    /// Query the code call graph: outgoing calls, incoming callers, or impact radius.
    #[tool(description = "Query code call graph. Use direction='calls' (default) for outgoing calls, 'callers' for incoming calls, or 'impact' for transitive dependency graph (with max_depth). Provide symbol_id to disambiguate when multiple symbols share the same name.")]
    async fn code_graph(
        &self,
        Parameters(params): Parameters<CodeGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_code_index().await?;

        let output = {
            let idx_guard = self.code_index.lock().await;
            let facade = idx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Code index not initialized"))?;

            let symbol = Self::resolve_symbol(facade, &params.name, params.symbol_id)?;

                match params.direction.as_str() {
                    "calls" => {
                        let called = facade.get_called_functions(symbol.id);
                        if called.is_empty() {
                            format!("{} ({:?}) does not call any indexed functions.", symbol.name, symbol.kind)
                        } else {
                            let mut out = format!(
                                "{} ({:?}) calls {} function(s):\n\n",
                                symbol.name, symbol.kind, called.len()
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
                            format!("{} ({:?}) has no indexed callers.", symbol.name, symbol.kind)
                        } else {
                            let mut out = format!(
                                "{} ({:?}) is called by {} function(s):\n\n",
                                symbol.name, symbol.kind, callers.len()
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
                                symbol.name, symbol.kind, impacted_ids.len(), params.max_depth
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
                            "Invalid direction: '{}'. Valid values: 'calls' (default), 'callers', 'impact'.",
                            params.direction
                        )));
                    }
                }
            }; // idx_guard dropped

        let tokens = count_tokens(&output);
        self.record_persistent_call("code_graph", tokens, 1, false).await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

}

/// Format a code symbol for MCP output.
fn format_symbol(sym: &crate::code::symbol::Symbol) -> String {
    let mut out = format!(
        "  sym#{} {:?} {} in {}:{}\n",
        sym.id.value(),
        sym.kind,
        sym.name,
        sym.file_path,
        sym.range.start_line,
    );
    if let Some(ref sig) = sym.signature {
        out.push_str(&format!("    Signature: {}\n", sig));
    }
    if let Some(ref doc) = sym.doc_comment {
        let truncated = if doc.len() > 120 {
            format!("{}...", &doc[..120])
        } else {
            doc.to_string()
        };
        out.push_str(&format!("    Doc: {}\n", truncated));
    }
    out
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

/// Transport mode for the MCP server.
#[derive(Debug, Clone)]
pub enum TransportMode {
    /// Standard IO (stdin/stdout) - default for CLI tools.
    Stdio,
    /// HTTP server with optional bearer token auth.
    Http {
        bind: String,
        token: Option<String>,
    },
    /// HTTPS server with self-signed cert and optional bearer token auth.
    Https {
        bind: String,
        token: Option<String>,
    },
}

/// Run the MCP server with file watching.
pub async fn run_server(root: PathBuf, transport: TransportMode) -> crate::error::Result<()> {
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
    let instructions = load_server_instructions(&root, warmup_limit);

    let server = McpServer::with_warmup(root.clone(), mcp_config, Some(instructions));

    // Start file watcher in background
    let watcher_root = root.clone();
    let watcher_ctx = server.ctx.clone();
    let watcher_code_index = server.code_index.clone();
    tokio::spawn(async move {
        if let Err(e) = run_file_watcher(
            watcher_root,
            watcher_ctx,
            watcher_code_index,
        )
        .await
        {
            tracing::error!("File watcher error: {}", e);
        }
    });

    match transport {
        TransportMode::Stdio => {
            tracing::info!("Starting mdkb MCP server on stdio...");
            let (stdin, stdout) = rmcp::transport::io::stdio();
            let service = server
                .serve((stdin, stdout))
                .await
                .map_err(|e| crate::error::Error::mcp(format!("Failed to start server: {e}")))?;
            service
                .waiting()
                .await
                .map_err(|e| crate::error::Error::mcp(format!("Server error: {e}")))?;
        }
        #[cfg(feature = "http-server")]
        TransportMode::Http { bind, token } => {
            super::http_server::run_http_server(server, &bind, token.as_deref()).await?;
        }
        #[cfg(not(feature = "http-server"))]
        TransportMode::Http { .. } => {
            return Err(crate::error::Error::mcp(
                "HTTP server not enabled. Build with --features http-server".to_string(),
            ));
        }
        #[cfg(feature = "https-server")]
        TransportMode::Https { bind, token } => {
            super::https_server::run_https_server(server, &bind, token.as_deref()).await?;
        }
        #[cfg(all(feature = "http-server", not(feature = "https-server")))]
        TransportMode::Https { .. } => {
            return Err(crate::error::Error::mcp(
                "HTTPS server not enabled. Build with --features https-server".to_string(),
            ));
        }
        #[cfg(not(feature = "http-server"))]
        TransportMode::Https { .. } => {
            return Err(crate::error::Error::mcp(
                "HTTPS server not enabled. Build with --features https-server".to_string(),
            ));
        }
    }

    Ok(())
}

/// Run the file watcher and trigger reindex on changes.
async fn run_file_watcher(
    root: PathBuf,
    ctx: Arc<Mutex<Option<Context>>>,
    code_index: Arc<Mutex<Option<IndexFacade>>>,
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

    // Watch all collection paths (for document reindex)
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

    // Watch root for source code changes (code intelligence)
    {
        let code_config = {
            let config_path = root.join(".mdkb/config.toml");
            crate::Config::load_or_default(&config_path).code
        };
        if code_config.enabled {
            if let Err(e) = watcher.watch(&root.to_path_buf()) {
                tracing::warn!("Failed to watch root for code changes: {}", e);
            } else {
                tracing::info!("Watching root for source code changes");
            }
        }
    }

    // Process file changes
    while let Some(change) = watcher.recv().await {
        tracing::debug!("File change detected: {:?}", change.path);

        // Check if this is a source code file change
        {
            use crate::code::parsing::language::Language;
            if Language::from_path(&change.path).is_some() {
                let mut idx_guard = code_index.lock().await;
                if let Some(facade) = idx_guard.as_mut() {
                    match facade.reindex(&root) {
                        Ok(stats) => {
                            if stats.symbols_indexed > 0 {
                                tracing::info!(
                                    "Code reindexed: {} files, {} symbols",
                                    stats.files_indexed,
                                    stats.symbols_indexed,
                                );
                            }
                        }
                        Err(e) => {
                            tracing::error!("Code reindex failed: {}", e);
                        }
                    }
                }
                // Also trigger document reindex in case it's in a collection
            }
        }

        // Re-acquire context and trigger document update
        let mut ctx_guard = ctx.lock().await;
        if let Some(ctx_ref) = ctx_guard.as_mut() {
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

/// Base instructions explaining what mdkb is and how to use it.
///
/// These are always included in server instructions, regardless of whether
/// memory entries exist. They tell the LLM what mdkb does and how to interact.
const BASE_INSTRUCTIONS: &str = "\
# mdkb - Markdown Knowledge Base

mdkb is a local knowledge base that indexes your project's documentation and source code. \
It provides hybrid search (keyword + semantic) across everything. Use it to find project docs, \
code symbols, solutions to past problems, and architectural decisions.

## Core Tools

- `search(query)`: Find documents using hybrid search. Start here.
  - `scope`: `\"docs\"` (default), `\"memory\"`, `\"all\"`, `\"code\"`, or `\"symbols\"`.
  - `scope=\"code\"`: Semantic similarity over code symbols. Optional: `kind` (e.g., \"function\", \"struct\"), `threshold` (0.0-1.0).
  - `scope=\"symbols\"`: Fuzzy text match over symbol names/signatures. Optional: `kind`, `file` (path substring filter).
- `get(id)`: Retrieve full content by numeric ID, file path (e.g., 'docs/api.md'), or memory slug (e.g., 'auth-oauth2').
  - Also accepts glob patterns (e.g., 'docs/*.md') and comma-separated lists (e.g., '42,43,44').
- `code_graph(name)`: Query code call graph. `direction`: `\"calls\"` (default), `\"callers\"`, or `\"impact\"` (transitive, with `max_depth`).
- `status`: Check index health (collections, documents, code index stats).
- `update`: Trigger reindex of everything (documents and source code).
- `memory_write(id, title, content, type, tags)`: Save knowledge for future sessions.
- `memory_delete(id)`: Delete a memory entry permanently.

### When to Write Memories
- After solving a problem: type=problem, title=symptom
- After making architectural decisions: type=decision, title=options
- After learning important patterns: type=topic, title=concept

## Getting Started

If `status` shows 0 symbols or stale documents, run `update` to reindex everything.
";

/// Build server instructions combining base instructions with memory index.
///
/// Always includes base instructions explaining what mdkb is.
/// Appends memory warmup index when memory entries exist.
fn build_server_instructions(index: &[String]) -> String {
    let mut instructions = BASE_INSTRUCTIONS.to_string();

    if !index.is_empty() {
        instructions.push_str("\n## Available Memories\n\n");
        for entry in index {
            instructions.push_str(entry);
            instructions.push('\n');
        }
        instructions.push_str("\nUse `get(id)` to retrieve full content.\n");
    }

    instructions
}

/// Load server instructions with optional memory warmup from database.
///
/// Always returns base instructions explaining what mdkb is.
/// If the database exists and has memory entries, appends the memory index.
fn load_server_instructions(root: &std::path::Path, limit: usize) -> String {
    // Try to load memory entries from existing database
    let memory_index = Context::open(root)
        .ok()
        .and_then(|ctx| {
            crate::store::schema::init_schema(&ctx.conn).ok()?;
            memory::get_warmup_index(&ctx.conn, limit).ok()
        })
        .unwrap_or_default();

    if !memory_index.is_empty() {
        tracing::info!("Memory warmup: {} entries", memory_index.len());
    }

    let instructions = build_server_instructions(&memory_index);
    let tokens = count_tokens(&instructions);
    tracing::info!("Server instructions: ~{} tokens", tokens);
    instructions
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
        return "No results found. Try broader terms, check `status` for indexed content, or `update` to reindex.".to_string();
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

/// Format memory search results for output.
fn format_memory_search_results(entries: &[memory::MemoryEntry]) -> String {
    if entries.is_empty() {
        return "No matching memory entries found. Use `memory_write` to create one.".to_string();
    }

    let mut out = format!("Found {} memory entries:\n\n", entries.len());
    for entry in entries {
        out.push_str(&format!(
            "- [{}] {} ({}): {}\n",
            entry.id,
            entry.title,
            entry.entry_type,
            truncate_text(&entry.content, 100)
        ));
    }
    out
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
        assert!(output.starts_with("No results found."), "Should start with 'No results found.', got: {}", output);
        assert!(output.contains("status"), "Should suggest status, got: {}", output);
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
    fn test_build_server_instructions_empty_index_has_base() {
        let index: Vec<String> = vec![];
        let result = build_server_instructions(&index);

        // Base instructions always present
        assert!(result.contains("mdkb"));
        assert!(result.contains("knowledge base"));
        assert!(result.contains("search"));
        assert!(!result.contains("Available Memories"));
    }

    #[test]
    fn test_build_server_instructions_with_entries() {
        let index = vec![
            "auth-oauth2: OAuth2 PKCE implementation #auth #security".to_string(),
            "bug-null-email: Null email panic fix #bug #users".to_string(),
        ];
        let result = build_server_instructions(&index);

        // Check base instructions present
        assert!(result.contains("knowledge base"));
        assert!(result.contains("memory_write"));
        assert!(result.contains("get(id)"));

        // Check guidance
        assert!(result.contains("When to Write Memories"));
        assert!(result.contains("type=problem"));
        assert!(result.contains("type=decision"));
        assert!(result.contains("type=topic"));

        // Check entries included
        assert!(result.contains("Available Memories"));
        assert!(result.contains("auth-oauth2: OAuth2 PKCE implementation #auth #security"));
        assert!(result.contains("bug-null-email: Null email panic fix #bug #users"));
    }

    #[test]
    fn test_build_server_instructions_token_budget() {
        // 50 entries should be around 1.5K tokens
        let mut index = Vec::new();
        for i in 0..50 {
            index.push(format!("entry-{i}: Some title for entry number {i} #tag1 #tag2"));
        }
        let result = build_server_instructions(&index);
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

    /// Test that the MCP server auto-initializes when .mdkb/ doesn't exist.
    #[tokio::test]
    async fn test_mcp_auto_init_on_first_tool_call() {
        use rmcp::model::EmptyObject;

        // Create temp directory WITHOUT running mdkb init
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();

        // Verify .mdkb/ does not exist
        assert!(!root.join(".mdkb").exists());

        // Create server pointing at uninitialized directory
        let server = McpServer::new(root.clone());

        // First tool call should auto-initialize and succeed
        let result = server.status(Parameters(EmptyObject {})).await;
        assert!(result.is_ok(), "status() should auto-init and succeed, got: {:?}", result.err());

        // Verify .mdkb/ was created
        assert!(root.join(".mdkb").exists(), ".mdkb/ should have been created");
        assert!(root.join(".mdkb/index.sqlite").exists(), "database should exist");
        assert!(root.join(".mdkb/config.toml").exists(), "config should exist");
    }

    #[test]
    fn test_build_server_instructions_contains_base_instructions() {
        // Even with no memories, instructions should explain what mdkb is
        let index: Vec<String> = vec![];
        let result = build_server_instructions(&index);

        // Must contain base instructions explaining mdkb purpose
        assert!(result.contains("mdkb"), "Should mention mdkb");
        assert!(result.contains("knowledge base"), "Should explain what mdkb is");
        assert!(result.contains("search"), "Should mention search tool");
        assert!(result.contains("memory_write"), "Should mention memory tools");
        assert!(result.contains("collection"), "Should mention collections");
    }

    #[test]
    fn test_build_server_instructions_includes_memory_when_present() {
        let index = vec![
            "auth-flow: OAuth2 implementation #auth".to_string(),
        ];
        let result = build_server_instructions(&index);

        // Should contain both base instructions and memory
        assert!(result.contains("knowledge base"), "Should have base instructions");
        assert!(result.contains("auth-flow"), "Should include memory entries");
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

        // Call status tool multiple times to verify no deadlock
        let tools_to_test = vec![
            "status",
            "status",
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

    /// Extract text from the first Content item in a CallToolResult.
    fn extract_text(result: &CallToolResult) -> &str {
        result
            .content
            .first()
            .and_then(|c| c.as_text())
            .map(|t| t.text.as_str())
            .expect("Expected text content")
    }

    /// Test memory_delete tool deletes a memory entry.
    #[tokio::test]
    async fn test_mcp_memory_delete() {
        use std::time::Duration;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

        let server = McpServer::new(root);

        // First write a memory entry
        server.memory_write(Parameters(MemoryWriteParams {
            id: "test-delete-me".to_string(),
            title: "Deletable entry".to_string(),
            content: "This will be deleted.".to_string(),
            entry_type: "topic".to_string(),
            tags: vec![],
        }))
        .await
        .expect("Failed to write memory entry");

        // Delete it
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.memory_delete(Parameters(MemoryDeleteParams {
                id: "test-delete-me".to_string(),
            })),
        )
        .await;

        match result {
            Ok(Ok(r)) => {
                let text = extract_text(&r);
                assert!(text.contains("Deleted memory entry 'test-delete-me'"), "Got: {}", text);
            }
            Ok(Err(e)) => panic!("memory_delete failed: {:?}", e),
            Err(_) => panic!("memory_delete timed out - likely deadlock!"),
        }

        // Verify it's gone - get should return not found
        let get_result = server
            .get(Parameters(GetParams {
                id: "test-delete-me".to_string(),
                lines: None,
            }))
            .await;
        assert!(get_result.is_err(), "get should fail for deleted memory entry");

        // Deleting nonexistent entry should report not found
        let result = server
            .memory_delete(Parameters(MemoryDeleteParams {
                id: "nonexistent".to_string(),
            }))
            .await
            .expect("Should not error");
        let text = extract_text(&result);
        assert!(text.contains("not found"), "Got: {}", text);
    }

    /// Regression test: new tools don't deadlock when called sequentially.
    #[tokio::test]
    async fn test_mcp_new_tools_no_deadlock() {
        use std::time::Duration;
        use rmcp::model::EmptyObject;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

        let server = McpServer::new(root);
        let timeout = Duration::from_secs(5);

        // Sequence: status -> memory_write -> memory_delete -> status
        tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
            .await.expect("timeout").expect("status failed");

        tokio::time::timeout(timeout, server.memory_write(Parameters(MemoryWriteParams {
            id: "deadlock-test".to_string(),
            title: "Test".to_string(),
            content: "Content".to_string(),
            entry_type: "topic".to_string(),
            tags: vec![],
        })))
        .await.expect("timeout").expect("memory_write failed");

        tokio::time::timeout(timeout, server.memory_delete(Parameters(MemoryDeleteParams {
            id: "deadlock-test".to_string(),
        })))
        .await.expect("timeout").expect("memory_delete failed");

        tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
            .await.expect("timeout").expect("status failed");
    }

    /// Extract the error message from a failed MCP tool result.
    fn extract_error_msg(result: Result<CallToolResult, McpError>) -> String {
        match result {
            Err(e) => e.message.into_owned(),
            Ok(r) => panic!("Expected error, got success: {}", extract_text(&r)),
        }
    }

    // --- Error message hint tests ---
    //
    // Every MCP tool error should include a usage hint suggesting how to
    // fix the problem. These tests verify that error messages contain
    // actionable guidance for LLMs.

    #[tokio::test]
    async fn test_get_not_found_error_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server.get(Parameters(GetParams {
            id: "nonexistent".to_string(),
            lines: None,
        })).await;

        let msg = extract_error_msg(result);
        assert!(msg.contains("search"), "Error should suggest search tool, got: {}", msg);
    }

    #[tokio::test]
    async fn test_get_numeric_id_not_found_error_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server.get(Parameters(GetParams {
            id: "999999".to_string(),
            lines: None,
        })).await;

        let msg = extract_error_msg(result);
        assert!(msg.contains("search"), "Error should suggest search tool, got: {}", msg);
    }

    #[tokio::test]
    async fn test_get_slug_not_found_error_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server.get(Parameters(GetParams {
            id: "nonexistent-memory-slug".to_string(),
            lines: None,
        })).await;

        let msg = extract_error_msg(result);
        assert!(msg.contains("search"),
            "Error should suggest search tool, got: {}", msg);
    }

    #[tokio::test]
    async fn test_search_no_results_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server.search(Parameters(SearchParams {
            query: "zzzznonexistentquery99999".to_string(),
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: None,
            kind: None,
            threshold: 0.3,
            file: None,
        })).await.expect("search should not error");

        let text = extract_text(&result);
        assert!(text.contains("status") || text.contains("broader") || text.contains("collection"),
            "No results should suggest broadening query or checking status, got: {}", text);
    }

    #[tokio::test]
    async fn test_get_glob_no_results_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server.get(Parameters(GetParams {
            id: "nonexistent/**/*.md".to_string(),
            lines: None,
        })).await.expect("get with glob should not error");

        let text = extract_text(&result);
        assert!(text.contains("status") || text.contains("search"),
            "No results should suggest status or search, got: {}", text);
    }

    #[tokio::test]
    async fn test_search_memory_scope_no_results_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server.search(Parameters(SearchParams {
            query: "zzzznonexistentquery99999".to_string(),
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: Some("memory".to_string()),
            kind: None,
            threshold: 0.3,
            file: None,
        })).await.expect("search with memory scope should not error");

        let text = extract_text(&result);
        assert!(text.contains("memory_write"),
            "No results should suggest memory_write, got: {}", text);
    }

    #[tokio::test]
    async fn test_memory_delete_not_found_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server.memory_delete(Parameters(MemoryDeleteParams {
            id: "nonexistent-entry".to_string(),
        })).await.expect("memory_delete should not error");

        let text = extract_text(&result);
        assert!(text.contains("search"),
            "Not found should suggest search, got: {}", text);
    }

    // --- Code intelligence tool tests ---

    mod code_intel_tests {
        use super::*;
        use std::time::Duration;

        /// Create a temp directory with Rust source files and a pre-populated code index.
        /// Returns (temp_dir, McpServer) where the server is ready to use.
        fn setup_indexed_server() -> (tempfile::TempDir, McpServer) {
            let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
            let root = temp_dir.path().to_path_buf();

            // Initialize mdkb
            crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

            // Create source files
            let src_dir = root.join("src");
            std::fs::create_dir_all(&src_dir).unwrap();

            std::fs::write(src_dir.join("main.rs"), r#"
/// Entry point for the application.
fn main() {
    let result = process_data("hello");
    println!("{}", result);
}

/// Process input data and return a formatted string.
fn process_data(input: &str) -> String {
    let validated = validate(input);
    format!("processed: {}", validated)
}

/// Validate input data.
fn validate(input: &str) -> &str {
    input.trim()
}
"#).unwrap();

            std::fs::write(src_dir.join("lib.rs"), r#"
/// A helper struct for data operations.
pub struct DataHelper {
    pub name: String,
}

impl DataHelper {
    /// Create a new DataHelper.
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string() }
    }

    /// Transform the data.
    pub fn transform(&self) -> String {
        format!("transformed: {}", self.name)
    }
}

/// Top-level utility function.
pub fn utility() -> i32 {
    42
}
"#).unwrap();

            // Create code index
            let index_path = root.join(".mdkb/code-index");
            let mut facade = IndexFacade::create(&index_path)
                .expect("Failed to create code index");
            facade.index_directory(&src_dir)
                .expect("Failed to index source files");

            let server = McpServer::new(root);
            (temp_dir, server)
        }

        #[tokio::test]
        async fn test_search_symbols_scope() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "main".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols failed");

            let text = extract_text(&result);
            assert!(text.contains("main"), "Should find main: {}", text);
            assert!(text.contains("sym#"), "Should include symbol ID: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_not_found() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "nonexistent_symbol_xyz".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols failed");

            let text = extract_text(&result);
            assert!(text.contains("No symbols found"), "Got: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_with_kind_filter() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "DataHelper".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: Some("struct".to_string()),
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols kind=struct failed");

            let text = extract_text(&result);
            assert!(text.contains("DataHelper"), "Should find DataHelper: {}", text);
            assert!(text.contains("Struct"), "Should be a struct: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_with_file_filter() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "utility".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: Some("lib.rs".to_string()),
            })))
            .await.expect("timeout").expect("search scope=symbols file=lib.rs failed");

            let text = extract_text(&result);
            assert!(text.contains("utility"), "Should find utility: {}", text);
            assert!(text.contains("lib.rs"), "Should be in lib.rs: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_invalid_kind() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "main".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: Some("invalid_kind".to_string()),
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout");

            assert!(result.is_err(), "Should error on invalid kind");
        }

        #[tokio::test]
        async fn test_search_symbols_fuzzy() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "process".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols failed");

            let text = extract_text(&result);
            assert!(text.contains("process_data"), "Should find process_data: {}", text);
        }

        #[tokio::test]
        async fn test_code_graph_calls() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.code_graph(Parameters(CodeGraphParams {
                name: "process_data".to_string(),
                direction: "calls".to_string(),
                symbol_id: None,
                max_depth: 3,
            })))
            .await.expect("timeout").expect("code_graph direction=calls failed");

            let text = extract_text(&result);
            assert!(
                text.contains("validate") || text.contains("does not call"),
                "Should list called functions or report none: {}",
                text
            );
        }

        #[tokio::test]
        async fn test_code_graph_callers() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.code_graph(Parameters(CodeGraphParams {
                name: "process_data".to_string(),
                direction: "callers".to_string(),
                symbol_id: None,
                max_depth: 3,
            })))
            .await.expect("timeout").expect("code_graph direction=callers failed");

            let text = extract_text(&result);
            assert!(
                text.contains("main") || text.contains("no indexed callers"),
                "Should list callers or report none: {}",
                text
            );
        }

        #[tokio::test]
        async fn test_code_graph_impact() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(timeout, server.code_graph(Parameters(CodeGraphParams {
                name: "validate".to_string(),
                direction: "impact".to_string(),
                symbol_id: None,
                max_depth: 3,
            })))
            .await.expect("timeout").expect("code_graph direction=impact failed");

            let text = extract_text(&result);
            assert!(
                text.contains("symbol(s)") || text.contains("no reachable symbols"),
                "Should report impact or no impact: {}",
                text
            );
        }

        #[tokio::test]
        async fn test_status_includes_code_index() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // Ensure code index is populated by doing a symbols search first
            tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "main".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols failed");

            let result = tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
                .await.expect("timeout").expect("status failed");

            let text = extract_text(&result);
            assert!(text.contains("Code Index"), "Should include code index section: {}", text);
            assert!(text.contains("Symbols:"), "Should list symbols: {}", text);
            assert!(text.contains("Files:"), "Should list files: {}", text);
            assert!(text.contains("Relationships:"), "Should list relationships: {}", text);
        }

        #[tokio::test]
        async fn test_code_intel_tools_no_deadlock() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // Call multiple code-intel tools in sequence to verify no deadlock
            tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
                .await.expect("timeout").expect("status failed");

            tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "main".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols failed");

            tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "data".to_string(),
                limit: 5,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols failed (second call)");

            tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
                .await.expect("timeout").expect("status failed (second call)");
        }

        #[tokio::test]
        async fn test_resolve_symbol_not_found() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // code_graph with nonexistent symbol should return an error
            let result = tokio::time::timeout(timeout, server.code_graph(Parameters(CodeGraphParams {
                name: "nonexistent_fn".to_string(),
                direction: "calls".to_string(),
                symbol_id: None,
                max_depth: 3,
            })))
            .await.expect("timeout");

            assert!(result.is_err(), "Should error for nonexistent symbol");
        }

        #[tokio::test]
        async fn test_resolve_symbol_by_id() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // First find a symbol to get its ID via search scope=symbols
            let find_result = tokio::time::timeout(timeout, server.search(Parameters(SearchParams {
                query: "main".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("symbols".to_string()),
                kind: None,
                threshold: 0.3,
                file: None,
            })))
            .await.expect("timeout").expect("search scope=symbols failed");

            let text = extract_text(&find_result);
            // Extract symbol ID from "sym#N"
            let sym_id: u32 = text
                .split("sym#")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .expect("Should find sym# in output");

            // Now use that ID with code_graph
            let result = tokio::time::timeout(timeout, server.code_graph(Parameters(CodeGraphParams {
                name: "main".to_string(),
                direction: "calls".to_string(),
                symbol_id: Some(sym_id),
                max_depth: 3,
            })))
            .await.expect("timeout").expect("code_graph with ID failed");

            let text = extract_text(&result);
            assert!(
                text.contains("main") || text.contains("does not call"),
                "Should work with symbol_id: {}",
                text
            );
        }
    }
}
