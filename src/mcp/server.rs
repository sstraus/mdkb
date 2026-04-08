//! MCP server implementation.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use rmcp::ServiceExt;
use rmcp::handler::server::ServerHandler;
use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, EmptyObject, ErrorCode, Implementation, ServerCapabilities, ServerInfo,
};
use rmcp::service::NotificationContext;
use rmcp::service::RoleServer;
use rmcp::{ErrorData as McpError, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::daemon::registry::{RepoHandle, RepoRegistry};

use crate::cli::handlers::{
    Context, handle_hybrid_search, handle_mget, handle_session_index, handle_update,
};
use crate::code::indexing::IndexFacade;
use crate::code::types::SymbolId;
use crate::config::McpConfig;
use crate::domain::SearchResult;
use crate::metrics::{
    UsageMetrics, count_tokens, truncate_with_continuation, truncate_with_ellipsis,
};
use crate::store::{collections, documents, evolution, memory, search, stats};
use crate::watcher::{FileWatcher, WatcherConfig};

use super::tools::{
    CodeGraphParams, GetParams, MemoryDeleteParams, MemoryListParams, MemoryWriteBatchParams,
    MemoryWriteParams, SearchParams,
};

/// Create an MCP error from a message.
fn mcp_error(message: impl Into<Cow<'static, str>>) -> McpError {
    McpError {
        code: ErrorCode::INTERNAL_ERROR,
        message: message.into(),
        data: None,
    }
}

/// Core logic for writing a single memory entry. Used by both `memory_write` and `memory_write_batch`.
fn write_single_memory(
    conn: &rusqlite::Connection,
    id: &str,
    title: &str,
    content: &str,
    entry_type_str: &str,
    source_type_str: &str,
    tags: &[String],
) -> Result<String, McpError> {
    memory::validate_entry_input(id, title, tags, content)
        .map_err(|e| mcp_error(e.to_string()))?;

    let existing = memory::get_entry_without_tracking(conn, id)
        .map_err(|e| mcp_error(format!("Failed to check existing entry: {e}")))?;

    let entry_type: memory::EntryType = entry_type_str
        .parse()
        .map_err(|e: String| mcp_error(format!("{e}. Valid types: topic, problem, decision")))?;

    let source_type: memory::SourceType = source_type_str
        .parse()
        .map_err(|e: String| mcp_error(e))?;

    let now = chrono::Utc::now().timestamp();
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
                                let similarity =
                                    1.0 - (*distance as f64 * *distance as f64 / 2.0);
                                return Err(mcp_error(format!(
                                    "Near-duplicate entry exists: \"{}\" (id: {}, similarity: {:.0}%). \
                                     Update that entry instead, or use a more distinct title/content.",
                                    dup.title, dup.id, similarity * 100.0
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
                        tracing::warn!(
                            "Failed to store memory embedding for '{id}': {e}"
                        );
                    }

                    if is_new {
                        let warnings =
                            memory::find_similar_entries(conn, &embedding, rowid, id);
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

/// MCP server for mdkb.
#[derive(Clone)]
pub struct McpServer {
    /// Path to the mdkb root directory.
    root: PathBuf,
    /// Shared database context.
    ctx: Arc<Mutex<Option<Context>>>,
    /// Code intelligence index (SQLite-backed).
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
    /// Full project config (cached at startup to avoid hot-path I/O).
    full_config: crate::Config,
    /// Glob patterns to exclude from code indexing (e.g. `**/node_modules/**`).
    code_ignore_patterns: Vec<String>,
    /// True while the startup task holds ctx for doc/session reindex.
    doc_reindex_active: Arc<AtomicBool>,
    /// True while startup code reindex is in progress (prevents concurrent facade creation).
    code_reindex_active: Arc<AtomicBool>,
    /// Multi-repo registry for global mode. None in standalone mode.
    registry: Option<Arc<RepoRegistry>>,
    /// Cached standalone handle (wraps self's Arcs). None in global mode.
    standalone_handle: Option<Arc<RepoHandle>>,
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
        let ctx = Arc::new(Mutex::new(None));
        let code_index = Arc::new(Mutex::new(None));
        let doc_reindex_active = Arc::new(AtomicBool::new(false));
        let code_reindex_active = Arc::new(AtomicBool::new(false));
        let full_config = crate::Config::default();
        let standalone_handle = Arc::new(RepoHandle::from_shared(
            root.clone(),
            ctx.clone(),
            code_index.clone(),
            full_config.clone(),
            Vec::new(),
            doc_reindex_active.clone(),
            code_reindex_active.clone(),
        ));
        Self {
            root,
            ctx,
            code_index,
            tool_router: Self::tool_router(),
            metrics: Arc::new(UsageMetrics::new()),
            config,
            session_id: Arc::new(AtomicI64::new(0)),
            warmup_instructions: None,
            full_config,
            code_ignore_patterns: Vec::new(),
            code_reindex_active,
            doc_reindex_active,
            registry: None,
            standalone_handle: Some(standalone_handle),
        }
    }

    /// Create a new MCP server with warmup instructions and full config pre-loaded.
    pub fn with_warmup(
        root: PathBuf,
        full_config: crate::Config,
        warmup: Option<String>,
        code_ignore_patterns: Vec<String>,
    ) -> Self {
        let config = full_config.mcp.clone();
        let ctx = Arc::new(Mutex::new(None));
        let code_index = Arc::new(Mutex::new(None));
        let doc_reindex_active = Arc::new(AtomicBool::new(false));
        let code_reindex_active = Arc::new(AtomicBool::new(false));
        let standalone_handle = Arc::new(RepoHandle::from_shared(
            root.clone(),
            ctx.clone(),
            code_index.clone(),
            full_config.clone(),
            code_ignore_patterns.clone(),
            doc_reindex_active.clone(),
            code_reindex_active.clone(),
        ));
        Self {
            root,
            ctx,
            code_index,
            tool_router: Self::tool_router(),
            metrics: Arc::new(UsageMetrics::new()),
            config,
            session_id: Arc::new(AtomicI64::new(0)),
            warmup_instructions: warmup,
            full_config,
            code_ignore_patterns,
            code_reindex_active,
            doc_reindex_active,
            registry: None,
            standalone_handle: Some(standalone_handle),
        }
    }

    /// Create a global-mode server with a RepoRegistry.
    /// Roots are populated via MCP `roots/list` after handshake.
    pub fn global(registry: Arc<RepoRegistry>) -> Self {
        Self {
            root: PathBuf::new(), // placeholder, not used in global mode
            ctx: Arc::new(Mutex::new(None)),
            code_index: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
            metrics: Arc::new(UsageMetrics::new()),
            config: McpConfig::default(),
            session_id: Arc::new(AtomicI64::new(0)),
            warmup_instructions: None,
            full_config: crate::Config::default(),
            code_ignore_patterns: Vec::new(),
            code_reindex_active: Arc::new(AtomicBool::new(false)),
            doc_reindex_active: Arc::new(AtomicBool::new(false)),
            registry: Some(registry),
            standalone_handle: None, // global mode uses registry instead
        }
    }

    /// Whether this server is in global (multi-repo) mode.
    pub fn is_global(&self) -> bool {
        self.registry.is_some()
    }

    /// Cross-repo search: fan out to all registered repos, merge results with RRF.
    async fn cross_repo_search(&self, params: &SearchParams) -> Result<CallToolResult, McpError> {
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| mcp_error("Cross-repo search requires global mode (--global)."))?;

        let handles = registry.all_handles();
        if handles.is_empty() {
            return Err(mcp_error(
                "No repos registered. Waiting for MCP roots from client.",
            ));
        }

        let scope = params.scope.as_deref();
        let limit = params.limit.min(100);

        // Only docs/memory/default scopes support cross-repo (code index is per-repo)
        if matches!(scope, Some("code") | Some("symbols")) {
            return Err(mcp_error(
                "Cross-repo search is not supported for code/symbols scope. Specify a root.",
            ));
        }

        let mut all_results: Vec<SearchResult> = Vec::new();

        for handle in &handles {
            Self::ensure_handle_context(handle).await?;
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
                    .map_err(|e| mcp_error(format!("Search failed on {}: {e}", repo_tag)))?;

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
                    )
                    .map_err(|e| mcp_error(format!("Memory search failed on {}: {e}", repo_tag)))?;

                    // Memory results are formatted separately, return early per-repo
                    // For simplicity, collect and merge textually
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
                        // Use first entry as representative
                        if let Some(e) = entries.first() {
                            pseudo.path = e.id.clone();
                            pseudo.title = Some(e.title.clone());
                        }
                        all_results.push(pseudo);
                    }
                }
                _ => {} // handled above
            }
        }

        // Sort by score descending, take top N
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

        let tokens = count_tokens(&output);
        let result_count = all_results.len();
        self.metrics.record_search(tokens, result_count);
        self.record_persistent_call("search", tokens, result_count, false)
            .await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Resolve the repo handle for a tool call.
    ///
    /// - Standalone mode: returns cached handle (shares Arcs with self).
    /// - Global mode, `root` = None, 1 registered repo: auto-selects it.
    /// - Global mode, `root` = None, N > 1 repos: error listing available roots.
    /// - Global mode, `root` = Some(path): resolves from registry.
    /// - `root` = "*": reserved for cross-repo search (not yet implemented).
    async fn resolve_handle(&self, root: Option<&str>) -> Result<Arc<RepoHandle>, McpError> {
        if let Some(registry) = &self.registry {
            // Global mode: resolve from registry
            let handle = match root {
                None => {
                    let handles = registry.all_handles();
                    match handles.len() {
                        0 => {
                            return Err(mcp_error(
                                "No repos registered. Waiting for MCP roots from client.",
                            ));
                        }
                        1 => handles.into_iter().next().unwrap(),
                        _ => {
                            let roots: Vec<_> = registry
                                .list()
                                .into_iter()
                                .map(|(p, _)| p.display().to_string())
                                .collect();
                            return Err(mcp_error(format!(
                                "Multiple repos registered. Specify root: {}",
                                roots.join(", ")
                            )));
                        }
                    }
                }
                Some("*") => {
                    return Err(mcp_error(
                        "Cross-repo search (root=\"*\") not yet implemented.",
                    ));
                }
                Some(path) => registry
                    .get_or_open(Path::new(path))
                    .map_err(|e| mcp_error(format!("{e}")))?,
            };
            Self::ensure_handle_context(&handle).await?;
            Ok(handle)
        } else {
            // Standalone mode: use cached handle that shares self's Arcs
            let handle = self
                .standalone_handle
                .as_ref()
                .expect("standalone_handle must be set in non-global mode");
            self.ensure_context().await?;
            Ok(Arc::clone(handle))
        }
    }

    /// Initialize database context on a RepoHandle (global mode).
    /// Auto-creates `.mdkb/` if it doesn't exist.
    async fn ensure_handle_context(handle: &RepoHandle) -> Result<(), McpError> {
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

    /// Acquire the code index lock on a RepoHandle, lazily initializing if needed.
    async fn acquire_handle_code_index(
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
            if !handle.code_ignore_patterns.is_empty() {
                let config = crate::code::indexing::pipeline::PipelineConfig {
                    ignore_patterns: handle.code_ignore_patterns.clone(),
                    ..Default::default()
                };
                facade = facade.with_config(config);
            }
            *idx_guard = Some(facade);
        }
        Ok(idx_guard)
    }

    /// Query MCP roots from the client and register them in the registry.
    async fn sync_roots_from_client(
        &self,
        context: &NotificationContext<RoleServer>,
        registry: &RepoRegistry,
    ) {
        match context.peer.list_roots().await {
            Ok(result) => {
                for root in &result.roots {
                    if let Some(path) = uri_to_path(&root.uri) {
                        match registry.get_or_open(&path) {
                            Ok(_) => {
                                tracing::info!(
                                    "Registered root from MCP client: {}",
                                    path.display()
                                );
                            }
                            Err(e) => {
                                tracing::warn!("Failed to register root {}: {e}", path.display());
                            }
                        }
                    } else {
                        tracing::warn!("Ignoring non-file root URI: {}", root.uri);
                    }
                }
            }
            Err(e) => {
                tracing::debug!("Client does not support roots/list: {e}");
            }
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
            // Startup task has taken ctx out for reindex — don't create a new one
            if self.doc_reindex_active.load(Ordering::Relaxed) {
                return Err(mcp_error("Server initializing, retry shortly"));
            }
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
    fn apply_conventions(
        &self,
        ctx: &Context,
    ) -> std::result::Result<(), Box<dyn std::error::Error>> {
        if !self.full_config.conventions.enabled {
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
                        if let Ok(Some(source)) =
                            documents::get_document(&ctx.conn, evo.source_doc_id)
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
    async fn get_batch(
        &self,
        handle: &RepoHandle,
        ids: &str,
        lines: &Option<String>,
    ) -> Result<CallToolResult, McpError> {
        let ctx_guard = handle.ctx.lock().await;
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
                    match self.get_document_content(ctx, &doc, lines) {
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

            // Try path resolution
            if let Ok(doc) = resolve_document(&ctx.conn, id) {
                match self.get_document_content(ctx, &doc, lines) {
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
            return Err(mcp_error("None of the requested items were found."));
        }

        let tokens = count_tokens(&output);
        drop(ctx_guard);
        self.finish_get(output, tokens).await
    }

    /// Retrieve multiple documents matching a glob pattern.
    async fn get_glob(
        &self,
        handle: &RepoHandle,
        pattern: &str,
    ) -> Result<CallToolResult, McpError> {
        let doc_limit = self.config.max_document_tokens;
        let truncate_ellipsis = self.config.truncate_with_ellipsis;
        let max_response_tokens = self.config.max_response_tokens;

        let (output, result_count) = {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let results = handle_mget(ctx, pattern, None)
                .map_err(|e| mcp_error(format!("Glob retrieval failed: {}", e)))?;

            if results.is_empty() {
                return Ok(CallToolResult::success(vec![Content::text(
                    "No documents matched pattern.",
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
        self.record_persistent_call("get", tokens, result_count, truncated)
            .await;
        tracing::debug!(
            "mdkb_get_glob: {} tokens, {} docs, truncated={}",
            tokens,
            result_count,
            truncated
        );

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Opens or creates the SQLite code index at `.mdkb/code.sqlite`.
    /// Acquire the code index lock, lazily initializing the facade if needed.
    ///
    /// Single lock acquisition eliminates the TOCTOU gap of the old
    /// ensure + re-lock pattern.
    async fn acquire_code_index(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<IndexFacade>>, McpError> {
        // If startup reindex took the facade, return the (empty) guard — callers
        // already handle the None case.
        if self.code_reindex_active.load(Ordering::Relaxed) {
            return Ok(self.code_index.lock().await);
        }
        let mut idx_guard = self.code_index.lock().await;
        if idx_guard.is_none() {
            let index_path = self.root.join(".mdkb/code.sqlite");
            let mut facade = IndexFacade::open_or_create(&index_path)
                .map_err(|e| mcp_error(format!("Failed to open code index: {}", e)))?;
            if !self.code_ignore_patterns.is_empty() {
                let config = crate::code::indexing::pipeline::PipelineConfig {
                    ignore_patterns: self.code_ignore_patterns.clone(),
                    ..Default::default()
                };
                facade = facade.with_config(config);
            }
            *idx_guard = Some(facade);
        }
        Ok(idx_guard)
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
            let sid =
                SymbolId::new(id).ok_or_else(|| mcp_error("Invalid symbol_id: 0 is reserved."))?;
            return facade
                .get_symbol(sid)
                .ok_or_else(|| mcp_error(format!("Symbol not found: sym#{id}.")));
        }

        let matches = facade.find_symbols_by_name(name);
        match matches.len() {
            0 => Err(mcp_error(format!(
                "No symbol found with name '{name}'. Try `search(query, scope=\"symbols\")` for fuzzy matching."
            ))),
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
            if let Err(e) =
                stats::record_call(&ctx.conn, session_id, tool_name, tokens, results, truncated)
            {
                tracing::warn!("Failed to record call stats: {}", e);
            }
        }
    }

    /// Search documents using hybrid search (BM25 + semantic with RRF fusion).
    #[tool(
        description = "Search docs+memory (default), code symbols (scope=\"symbols\"), or semantic code (scope=\"code\")."
    )]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        // Cross-repo search: fan out to all registered repos
        if params.root.as_deref() == Some("*") {
            return self.cross_repo_search(&params).await;
        }

        let handle = self.resolve_handle(params.root.as_deref()).await?;

        let scope = params.scope.as_deref();
        let limit = params.limit.min(100); // Cap to prevent unbounded result sets

        let (output, tokens, result_count) = {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            match scope {
                Some("docs") => {
                    let results = handle_hybrid_search(
                        ctx,
                        &params.query,
                        limit,
                        params.collection.as_deref(),
                        params.include_superseded,
                    )
                    .map_err(|e| mcp_error(format!("Search failed: {}", e)))?;

                    let top_score = results.first().map(|r| r.score);
                    let mut output = format_search_results(&results, limit);
                    if let Some(hint) = ood_hint(results.len(), top_score) {
                        output.push_str(hint);
                    }
                    let tokens = count_tokens(&output);
                    (output, tokens, results.len())
                }
                Some("memory") => {
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
                    )
                    .map_err(|e| mcp_error(format!("Memory search failed: {}", e)))?;

                    let mut output = format_memory_search_results(&entries);
                    if let Some(hint) = ood_hint(entries.len(), None) {
                        output.push_str(hint);
                    }
                    let tokens = count_tokens(&output);
                    (output, tokens, entries.len())
                }
                None => {
                    let doc_results = handle_hybrid_search(
                        ctx,
                        &params.query,
                        limit,
                        params.collection.as_deref(),
                        params.include_superseded,
                    )
                    .map_err(|e| mcp_error(format!("Search failed: {}", e)))?;

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
                    )
                    .map_err(|e| mcp_error(format!("Memory search failed: {}", e)))?;

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

                    let tokens = count_tokens(&output);
                    (output, tokens, total)
                }
                Some("code") | Some("symbols") => {
                    // Drop ctx_guard before acquiring code_index lock
                    drop(ctx_guard);

                    {
                        let mut idx_guard = Self::acquire_handle_code_index(&handle).await?;
                        let facade = match idx_guard.as_mut() {
                            Some(f) => f,
                            None => {
                                return Ok(CallToolResult::success(vec![Content::text(
                                    "Code index is being rebuilt, retry shortly.",
                                )]));
                            }
                        };

                        if scope == Some("code") {
                            // Check if semantic search is enabled
                            if !handle.config.code.semantic_search.enabled {
                                return Err(mcp_error(
                                    "Semantic code search is disabled. Enable it in mdkb.toml: [code.semantic_search] enabled = true, then re-index.".to_string(),
                                ));
                            }
                            // Semantic search (embedding similarity)
                            // Cap at 5 to avoid flooding context with low-quality matches
                            let code_limit = params.limit.min(5);
                            let mut results = facade
                                .semantic_search(&params.query, code_limit, params.threshold)
                                .map_err(|e| {
                                    tracing::error!("Semantic code search failed: {e}");
                                    mcp_error("Semantic code search failed. The embedding model may not be installed — run `mdkb code index` to initialize.".to_string())
                                })?;

                            // Apply kind filter
                            if let Some(ref kind_str) = params.kind {
                                if let Ok(kind) = kind_str.parse::<crate::code::types::SymbolKind>()
                                {
                                    results.retain(|(s, _)| s.kind == kind);
                                } else {
                                    return Err(mcp_error(format!(
                                        "Unknown symbol kind: '{kind_str}'. Valid kinds: function, method, struct, enum, trait, interface, class, module, variable, constant, field, parameter, type_alias, macro"
                                    )));
                                }
                            }

                            if results.is_empty() {
                                let output = "No semantic matches found.".to_string();
                                let tokens = count_tokens(&output);
                                (output, tokens, 0)
                            } else {
                                let mut out =
                                    format!("Found {} semantic match(es):\n\n", results.len());
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
                            // symbols scope: file-first or text search
                            let mut symbols = if let Some(ref file_pattern) = params.file {
                                // File filter present → query by file path directly
                                let mut results = facade.find_symbols_by_file(file_pattern, params.limit * 2);
                                // If query is non-trivial, further filter by name
                                if !params.query.is_empty() && params.query != "*" {
                                    let q = params.query.to_lowercase();
                                    results.retain(|s| s.name.to_lowercase().contains(&q));
                                }
                                results
                            } else {
                                facade.search_symbols(&params.query, params.limit)
                            };

                            // Apply kind filter
                            if let Some(ref kind_str) = params.kind {
                                if let Ok(kind) = kind_str.parse::<crate::code::types::SymbolKind>()
                                {
                                    symbols.retain(|s| s.kind == kind);
                                } else {
                                    return Err(mcp_error(format!(
                                        "Unknown symbol kind: '{kind_str}'. Valid kinds: function, method, struct, enum, trait, interface, class, module, variable, constant, field, parameter, type_alias, macro"
                                    )));
                                }
                            }

                            if symbols.is_empty() {
                                let output = "No symbols found.".to_string();
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
                Some(invalid) => {
                    return Err(mcp_error(format!(
                        "Invalid scope: '{}'. Valid: docs, memory, code, symbols.",
                        invalid
                    )));
                }
            }
        }; // ctx_guard dropped here before record_persistent_call

        self.metrics.record_search(tokens, result_count);
        self.record_persistent_call("search", tokens, result_count, false)
            .await;
        tracing::debug!("mdkb_search: {} tokens, {} results", tokens, result_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve a document by ID or path, with optional line range.
    /// Also accepts memory slugs, glob patterns, and comma-separated lists.
    #[tool(
        description = "Retrieve a document by ID, path, or memory slug, with optional line range."
    )]
    async fn get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;

        let id = &params.id;

        // Comma-separated list → batch retrieve
        if id.contains(',') {
            return self.get_batch(&handle, id, &params.lines).await;
        }

        // Glob pattern → batch retrieve by pattern
        if id.contains('*') || id.contains('?') {
            return self.get_glob(&handle, id).await;
        }

        let ctx_guard = handle.ctx.lock().await;
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
                .map_err(|e| mcp_error(format!("Failed to get document: {}", e)))?
            {
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
            // History format: return revision diffs instead of content
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
                let tokens = count_tokens(&output);
                drop(ctx_guard);
                self.record_persistent_call("get", tokens, 1, false).await;
                return Ok(CallToolResult::success(vec![Content::text(output)]));
            }

            let is_summary = params.format.as_deref() == Some("summary");
            let body = if is_summary {
                // Summary: first paragraph only
                entry
                    .content
                    .split("\n\n")
                    .next()
                    .unwrap_or(&entry.content)
                    .to_string()
            } else {
                entry.content.clone()
            };
            // Confidence breakdown
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

            // Revision history summary
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

            let output = format!(
                "# {} ({})\n\nType: {} | Status: {} | Tags: {}\nAccessed: {} times | {}{}\n\n{}",
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
                conf_line,
                rev_line,
                body
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

        Err(mcp_error(format!("Not found: '{}'.", params.id)))
    }

    /// Get index status including documents, collections, and code index.
    #[tool(description = "Get index status: collections, documents, code index stats.")]
    async fn status(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(None).await?;

        let mut output = {
            let ctx_guard = handle.ctx.lock().await;
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
                    let doc_count =
                        collections::get_collection_document_count(&ctx.conn, &coll.name)
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
        }; // ctx_guard dropped here

        // Always show code index stats (initialize if needed)
        if let Ok(idx_guard) = Self::acquire_handle_code_index(&handle).await {
            if let Some(facade) = idx_guard.as_ref() {
                let symbols = facade.symbol_count();
                let files = facade.file_count();
                let relationships = facade.relationship_count();
                output.push_str(&format!(
                    "\n## Code Index\n\nSymbols: {}\nFiles: {}\nRelationships: {}\n",
                    symbols, files, relationships
                ));
                if symbols == 0 {
                    output
                        .push_str("\nNo symbols indexed yet. Run `update` to index source code.\n");
                }
            }
        }

        let tokens = count_tokens(&output);
        self.metrics.record_status(tokens);
        self.record_persistent_call("status", tokens, 1, false)
            .await;
        tracing::debug!("mdkb_status: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Reindex everything: documents (from collections) and source code (from project root).
    #[tool(description = "Differential reindex of all collections.")]
    async fn update(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(None).await?;

        // Phase 1: Reindex documents (markdown collections)
        let doc_output = {
            let mut ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_mut()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let result = handle_update(ctx, &handle.root)
                .map_err(|e| mcp_error(format!("Document update failed: {}", e)))?;

            format!(
                "## Documents\n\nAdded: {}\nUpdated: {}\nRemoved: {}\nUnchanged: {}",
                result.added, result.updated, result.removed, result.unchanged
            )
        }; // ctx_guard dropped here

        // Phase 2: Reindex source code (tree-sitter + SQLite)
        let code_output = {
            let mut idx_guard = Self::acquire_handle_code_index(&handle).await?;
            if let Some(facade) = idx_guard.as_mut() {
                match facade.reindex(&handle.root) {
                    Ok(stats) => {
                        format!(
                            "\n\n## Code\n\nFiles: {}\nSymbols: {}\nRelationships: {}",
                            stats.files_indexed,
                            stats.symbols_indexed,
                            stats.relationships_collected
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

        // Phase 3: Reindex Claude Code sessions
        let session_output = {
            let sessions_base = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                .join(".claude/projects");
            let project_root = handle.root.to_string_lossy().to_string();

            let mut ctx_guard = handle.ctx.lock().await;
            if let Some(ctx) = ctx_guard.as_mut() {
                match handle_session_index(ctx, &sessions_base, &project_root) {
                    Ok(sr) if sr.added > 0 || sr.updated > 0 => {
                        format!(
                            "\n\n## Sessions\n\nAdded: {}\nUpdated: {}\nUnchanged: {}",
                            sr.added, sr.updated, sr.unchanged
                        )
                    }
                    Ok(_) => String::new(),
                    Err(e) => {
                        tracing::warn!("Session indexing failed: {}", e);
                        String::new()
                    }
                }
            } else {
                String::new()
            }
        };

        let output = format!("{}{}{}", doc_output, code_output, session_output);
        let tokens = count_tokens(&output);
        self.metrics.record_update(tokens);
        self.record_persistent_call("update", tokens, 1, false)
            .await;
        tracing::debug!("mdkb_update: {} tokens", tokens);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Write or update a memory entry.
    #[tool(
        description = "Create or update a memory entry. Types: problem, decision, topic. Slug ID, title max 50 chars."
    )]
    async fn memory_write(
        &self,
        Parameters(params): Parameters<MemoryWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;

        let (output, tokens) = {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let output = write_single_memory(
                &ctx.conn,
                &params.id,
                &params.title,
                &params.content,
                &params.entry_type,
                &params.source_type,
                &params.tags,
            )?;

            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_write", tokens, 1, false)
            .await;
        tracing::debug!("mdkb_memory_write: {}", output);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Write multiple memory entries in one call.
    #[tool(
        description = "Create or update multiple memory entries at once. Same semantics as memory_write, batched. Max 20 entries."
    )]
    async fn memory_write_batch(
        &self,
        Parameters(params): Parameters<MemoryWriteBatchParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.entries.is_empty() {
            return Err(mcp_error("entries array must not be empty"));
        }
        if params.entries.len() > 20 {
            return Err(mcp_error("max 20 entries per batch"));
        }

        let handle = self.resolve_handle(params.root.as_deref()).await?;

        let (output, tokens, count) = {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let mut results = Vec::with_capacity(params.entries.len());
            for entry in &params.entries {
                let result = write_single_memory(
                    &ctx.conn,
                    &entry.id,
                    &entry.title,
                    &entry.content,
                    &entry.entry_type,
                    &entry.source_type,
                    &entry.tags,
                )?;
                results.push(result);
            }

            let output = results.join("\n");
            let tokens = count_tokens(&output);
            let count = results.len();
            (output, tokens, count)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_write_batch", tokens, count, false)
            .await;
        tracing::debug!("mdkb_memory_write_batch: {} entries", count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Delete a memory entry by ID.
    #[tool(description = "Delete a memory entry by ID.")]
    async fn memory_delete(
        &self,
        Parameters(params): Parameters<MemoryDeleteParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;

        let (output, tokens) = {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let deleted = memory::delete_entry(&ctx.conn, &params.id)
                .map_err(|e| mcp_error(format!("Failed to delete memory entry: {}", e)))?;

            let output = if deleted {
                format!("Deleted memory entry '{}'.", params.id)
            } else {
                format!("Memory entry '{}' not found.", params.id)
            };
            let tokens = count_tokens(&output);
            (output, tokens)
        }; // ctx_guard dropped here

        self.record_persistent_call("memory_delete", tokens, 1, false)
            .await;
        tracing::debug!("mdkb_memory_delete: {}", output);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// List memory entries with configurable sort order.
    #[tool(description = "List memory entries sorted by recency, popularity, or creation date.")]
    async fn memory_list(
        &self,
        Parameters(params): Parameters<MemoryListParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;

        let sort: memory::MemorySortOrder =
            params.sort.parse().map_err(|e: String| mcp_error(e))?;

        let (output, tokens, count) = {
            let ctx_guard = handle.ctx.lock().await;
            let ctx = ctx_guard
                .as_ref()
                .ok_or_else(|| mcp_error("Database not initialized"))?;

            let entries = memory::list_entries_sorted(
                &ctx.conn,
                params.limit,
                sort,
                Some(memory::EntryStatus::Active),
            )
            .map_err(|e| mcp_error(format!("Failed to list memory entries: {}", e)))?;

            if entries.is_empty() {
                let output = "No memory entries.".to_string();
                let tokens = count_tokens(&output);
                (output, tokens, 0)
            } else {
                let mut out = format!("Found {} memory entries:\n\n", entries.len());
                for e in &entries {
                    let tags = e
                        .tags
                        .iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    out.push_str(&format!(
                        "- [{}] {} ({}, {}): {} {}\n",
                        e.entry_type,
                        e.id,
                        e.title,
                        relative_time_ago(e.updated_at),
                        truncate_text(&e.content, 80),
                        tags,
                    ));
                }
                let count = entries.len();
                let tokens = count_tokens(&out);
                (out, tokens, count)
            }
        }; // ctx_guard dropped here

        self.metrics.record_search(tokens, count);
        self.record_persistent_call("memory_list", tokens, count, false)
            .await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    // -----------------------------------------------------------------------
    // Code intelligence tools
    // -----------------------------------------------------------------------

    /// Query the code call graph: outgoing calls, incoming callers, or impact radius.
    #[tool(description = "Query code call graph. Directions: calls (default), callers, impact.")]
    async fn code_graph(
        &self,
        Parameters(params): Parameters<CodeGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;

        let output = {
            let idx_guard = Self::acquire_handle_code_index(&handle).await?;
            let facade = match idx_guard.as_ref() {
                Some(f) => f,
                None => {
                    return Ok(CallToolResult::success(vec![Content::text(
                        "Code index is being rebuilt, retry shortly.",
                    )]));
                }
            };

            let symbol = Self::resolve_symbol(facade, &params.name, params.symbol_id)?;

            match params.direction.as_str() {
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
                                out.push_str(&format!(
                                    "  sym#{} (not found in index)\n",
                                    sid.value()
                                ));
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
            }
        }; // idx_guard dropped

        let tokens = count_tokens(&output);
        self.record_persistent_call("code_graph", tokens, 1, false)
            .await;

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
        let truncated = truncate_text(doc, 120);
        out.push_str(&format!("    Doc: {}\n", truncated));
    }
    out
}

/// Resolve a document by path or ID.
/// Convert a `file://` URI to a local filesystem path.
fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let path_str = uri.strip_prefix("file://")?;
    let path = PathBuf::from(path_str);
    if path.is_absolute() { Some(path) } else { None }
}

fn resolve_document(
    conn: &rusqlite::Connection,
    path_or_id: &str,
) -> crate::error::Result<crate::domain::Document> {
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

    Err(crate::error::Error::from(
        crate::error::ErrorKind::DocumentNotFound {
            id: path_or_id.to_string(),
        },
    ))
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

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Some(registry) = &self.registry {
            self.sync_roots_from_client(&context, registry).await;
        }
    }

    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        if let Some(registry) = &self.registry {
            self.sync_roots_from_client(&context, registry).await;
        }
    }
}

/// Transport mode for the MCP server.
#[derive(Debug, Clone)]
pub enum TransportMode {
    /// Standard IO (stdin/stdout) - default for CLI tools.
    Stdio,
    /// HTTP server with optional bearer token auth.
    Http { bind: String, token: Option<String> },
    /// HTTPS server with self-signed cert and optional bearer token auth.
    Https { bind: String, token: Option<String> },
}

/// Run the MCP server with file watching.
pub async fn run_server(root: PathBuf, transport: TransportMode) -> crate::error::Result<()> {
    // Load config if available
    let config_path = root.join(".mdkb/config.toml");
    let full_config = if config_path.exists() {
        match crate::Config::load(&config_path) {
            Ok(config) => config,
            Err(e) => {
                tracing::warn!("Failed to load config, using defaults: {}", e);
                crate::Config::default()
            }
        }
    } else {
        crate::Config::default()
    };
    let code_ignore_patterns = full_config.code.indexing.ignore_patterns.clone();

    tracing::info!(
        "MCP config: max_response_tokens={}, max_document_tokens={}",
        full_config.mcp.max_response_tokens,
        full_config.mcp.max_document_tokens
    );

    // Load memory warmup before starting server
    let warmup_limit = full_config.memory.warmup_limit;
    let instructions = load_server_instructions(&root, warmup_limit);

    let server = McpServer::with_warmup(
        root.clone(),
        full_config,
        Some(instructions),
        code_ignore_patterns,
    );

    // Auto-index on startup: initialize context and run initial indexing in background
    {
        let startup_server = server.clone();
        let startup_root = root.clone();
        tokio::spawn(async move {
            // Initialize context (auto-creates .mdkb/ if needed, detects conventions)
            if let Err(e) = startup_server.ensure_context().await {
                tracing::error!("Startup auto-init failed: {:?}", e);
                return;
            }

            // Take ctx out of the lock for doc/session reindex so tool calls
            // get "initializing, retry shortly" instead of blocking for minutes.
            startup_server
                .doc_reindex_active
                .store(true, Ordering::Relaxed);
            let taken_ctx = {
                let mut ctx_guard = startup_server.ctx.lock().await;
                ctx_guard.take()
            }; // lock released immediately

            if let Some(mut ctx) = taken_ctx {
                match handle_update(&mut ctx, &startup_root) {
                    Ok(result) => {
                        if result.added > 0 || result.updated > 0 || result.removed > 0 {
                            tracing::info!(
                                "Startup index: {} added, {} updated, {} removed docs",
                                result.added,
                                result.updated,
                                result.removed
                            );
                        }
                    }
                    Err(e) => tracing::error!("Startup doc reindex failed: {}", e),
                }

                let sessions_base =
                    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                        .join(".claude/projects");
                let project_root = startup_root.to_string_lossy().to_string();
                match handle_session_index(&ctx, &sessions_base, &project_root) {
                    Ok(sr) if sr.added > 0 || sr.updated > 0 => {
                        tracing::info!(
                            "Startup session index: {} added, {} updated",
                            sr.added,
                            sr.updated
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("Session indexing failed: {}", e),
                }

                // Put ctx back so tool calls can proceed
                {
                    let mut ctx_guard = startup_server.ctx.lock().await;
                    *ctx_guard = Some(ctx);
                }
            }
            startup_server
                .doc_reindex_active
                .store(false, Ordering::Relaxed);

            // Run code reindex (incremental if index already exists).
            // Take the facade out of the lock to avoid blocking tool calls
            // during the (potentially slow) reindex + embedding generation.
            let taken_facade = match startup_server.acquire_code_index().await {
                Ok(mut guard) => {
                    startup_server
                        .code_reindex_active
                        .store(true, Ordering::Relaxed);
                    guard.take()
                }
                Err(e) => {
                    tracing::error!("Startup code index init failed: {:?}", e);
                    return;
                }
            }; // lock released immediately

            if let Some(mut facade) = taken_facade {
                let result = if facade.file_count() == 0 {
                    facade.reindex(&startup_root)
                } else {
                    let all_files = crate::code::indexing::walker::discover_files(
                        &startup_root,
                        &startup_server.code_ignore_patterns,
                    );
                    facade.reindex_files(&startup_root, &all_files)
                };
                crate::llm::release_cached_service();
                match result {
                    Ok(stats) => {
                        if stats.symbols_indexed > 0 || stats.files_indexed > 0 {
                            tracing::info!(
                                "Startup index: {} files, {} symbols",
                                stats.files_indexed,
                                stats.symbols_indexed
                            );
                        }
                    }
                    Err(e) => tracing::error!("Startup code reindex failed: {}", e),
                }

                // Put the facade back and clear the flag
                let mut idx_guard = startup_server.code_index.lock().await;
                *idx_guard = Some(facade);
            }
            startup_server
                .code_reindex_active
                .store(false, Ordering::Relaxed);
        });
    }

    // Start file watcher in background
    let watcher_root = root.clone();
    let watcher_ctx = server.ctx.clone();
    let watcher_code_index = server.code_index.clone();
    let watcher_code_enabled = server.full_config.code.enabled;
    let watcher_ignore_patterns = server.code_ignore_patterns.clone();
    tokio::spawn(async move {
        if let Err(e) = run_file_watcher(
            watcher_root,
            watcher_ctx,
            watcher_code_index,
            watcher_code_enabled,
            watcher_ignore_patterns,
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

/// Classify a file change into code and/or document categories.
///
/// Returns `(is_code, is_doc)` — a file can be both (e.g., a `.rs` file
/// inside a collection path).
/// Build a GlobSet from ignore patterns for filtering code changes.
fn build_code_excludes(root: &Path, patterns: &[String]) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for pattern in patterns {
        // Patterns are relative globs (e.g. "**/node_modules/**").
        // Prefix with root so they match absolute paths from the watcher.
        let abs_pattern = format!("{}/{pattern}", root.display());
        match globset::Glob::new(&abs_pattern) {
            Ok(g) => {
                builder.add(g);
            }
            Err(e) => tracing::warn!("Invalid code exclude pattern '{pattern}': {e}"),
        }
    }
    builder.build().unwrap_or_else(|e| {
        tracing::error!("Failed to build code exclude GlobSet: {e}");
        globset::GlobSet::empty()
    })
}

fn classify_change(
    path: &Path,
    collection_paths: &[PathBuf],
    code_excludes: &globset::GlobSet,
) -> (bool, bool) {
    use crate::code::parsing::language::Language;
    let is_code = Language::from_path(path).is_some() && !code_excludes.is_match(path);
    let is_doc = collection_paths.iter().any(|cp| path.starts_with(cp));
    (is_code, is_doc)
}

/// Batch window for collecting rapid-fire code changes before reindexing.
/// Idle timeout before flushing accumulated code changes as a batch reindex.
///
/// During active development, files change in rapid succession. A short timeout
/// triggers frequent reindexes — each loading the ONNX model, generating
/// embeddings, and allocating memory arenas. 30 seconds lets file saves
/// accumulate into a single efficient batch while still feeling responsive.
const CODE_BATCH_IDLE_MS: u64 = 30_000;

/// Run the file watcher and trigger reindex on changes.
async fn run_file_watcher(
    root: PathBuf,
    ctx: Arc<Mutex<Option<Context>>>,
    code_index: Arc<Mutex<Option<IndexFacade>>>,
    code_enabled: bool,
    code_ignore_patterns: Vec<String>,
) -> crate::error::Result<()> {
    let config = WatcherConfig::default();
    let mut watcher = FileWatcher::new(config)?;

    // Open context to get collection paths
    let ctx_guard = ctx.lock().await;
    if ctx_guard.is_none() {
        drop(ctx_guard);
        return Ok(());
    }

    let collection_list = {
        let ctx_ref = ctx_guard.as_ref().unwrap();
        collections::list_collections(&ctx_ref.conn)?
    };
    drop(ctx_guard);

    // Resolve absolute collection paths for routing
    let collection_paths: Vec<PathBuf> = collection_list
        .iter()
        .map(|coll| root.join(&coll.path))
        .filter(|p| p.exists())
        .collect();

    // Build exclude matcher for code changes (node_modules, dist, target, etc.)
    let code_excludes = build_code_excludes(&root, &code_ignore_patterns);

    // Watch root recursively (covers both code and collections inside root)
    if code_enabled {
        if let Err(e) = watcher.watch(&root.to_path_buf()) {
            tracing::warn!("Failed to watch root: {}", e);
        } else {
            tracing::info!("Watching root for changes");
        }
    }

    // Only watch collection paths that are OUTSIDE the root (rare edge case)
    for (coll, abs_path) in collection_list.iter().zip(collection_paths.iter()) {
        if !abs_path.starts_with(&root) {
            if let Err(e) = watcher.watch(&abs_path.to_path_buf()) {
                tracing::warn!("Failed to watch {}: {}", abs_path.display(), e);
            } else {
                tracing::info!(
                    "Watching external collection '{}' at {}",
                    coll.name,
                    abs_path.display()
                );
            }
        }
    }

    // Process file changes with batching for code reindex
    let mut code_batch: Vec<PathBuf> = Vec::new();
    let mut needs_doc_update = false;

    loop {
        let change = if code_batch.is_empty() && !needs_doc_update {
            // No pending work — block until next event
            match watcher.recv().await {
                Some(c) => c,
                None => break,
            }
        } else {
            // Pending batch — wait for more events or flush after idle timeout
            match tokio::time::timeout(
                std::time::Duration::from_millis(CODE_BATCH_IDLE_MS),
                watcher.recv(),
            )
            .await
            {
                Ok(Some(c)) => c,
                Ok(None) => break, // channel closed
                Err(_) => {
                    // Timeout: flush pending batch
                    flush_code_batch(&code_index, &root, &mut code_batch).await;
                    flush_doc_update(&ctx, &root, &mut needs_doc_update).await;
                    continue;
                }
            }
        };

        tracing::debug!("File change detected: {:?}", change.path);

        let (is_code, is_doc) = classify_change(&change.path, &collection_paths, &code_excludes);

        if is_code {
            code_batch.push(change.path.clone());
        }
        if is_doc {
            needs_doc_update = true;
        }
        if !is_code && !is_doc {
            tracing::debug!("Ignoring non-code, non-doc change: {:?}", change.path);
        }
    }

    Ok(())
}

/// Flush accumulated code changes as a single incremental reindex.
async fn flush_code_batch(
    code_index: &Arc<Mutex<Option<IndexFacade>>>,
    root: &Path,
    batch: &mut Vec<PathBuf>,
) {
    if batch.is_empty() {
        return;
    }
    let paths = std::mem::take(batch);
    let mut idx_guard = code_index.lock().await;
    if let Some(facade) = idx_guard.as_mut() {
        match facade.reindex_files(root, &paths) {
            Ok(stats) => {
                if stats.symbols_indexed > 0 || stats.files_indexed > 0 {
                    tracing::info!(
                        "Code reindexed: {} files, {} symbols (from {} changes)",
                        stats.files_indexed,
                        stats.symbols_indexed,
                        paths.len(),
                    );
                }
            }
            Err(e) => tracing::error!("Code reindex failed: {}", e),
        }
        crate::llm::release_cached_service();
    }
}

/// Flush pending document update.
async fn flush_doc_update(ctx: &Arc<Mutex<Option<Context>>>, root: &Path, needs_update: &mut bool) {
    if !*needs_update {
        return;
    }
    *needs_update = false;
    let mut ctx_guard = ctx.lock().await;
    if let Some(ctx_ref) = ctx_guard.as_mut() {
        match handle_update(ctx_ref, root) {
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
            Err(e) => tracing::error!("Doc reindex failed: {}", e),
        }
    }
}

/// Base instructions explaining what mdkb is and how to use it.
///
/// These are always included in server instructions, regardless of whether
/// memory entries exist. They tell the LLM what mdkb does and how to interact.
const BASE_INSTRUCTIONS: &str = "\
# mdkb — Project Knowledge Base

## When to use mdkb vs Grep/Glob

Use Grep/Glob for exact string/pattern matching. Use mdkb for everything else:
- **Understanding code flow**: `search(scope=\"symbols\")` to find entry point → `code_graph(name)` to trace all callers/callees. This replaces iterative grep-read-grep chains.
- **Semantic code queries**: `search(query, scope=\"code\")` for natural language (\"where is auth handled?\")
- **Docs + decisions**: `search(query)` searches docs and memory together.
- **Impact analysis**: `code_graph(name, direction=\"callers\")` before modifying any function.

## Workflow

1. Before multi-step tasks: `search(query, scope=\"memory\")` for prior decisions.
2. To understand a function: `search(name, scope=\"symbols\")` → `code_graph(name)` for full call tree.
3. After solving problems: check duplicates, then `memory_write`. Batch 2+ with `memory_write_batch`.
4. Search returns IDs. Use `get(id)` for full content.

Memory entry_type: `problem`, `decision`, `topic`.

## Tools

| Need | Tool |
|---|---|
| Docs + memory search | `search(query)` |
| Semantic code search | `search(query, scope=\"code\")` |
| Find symbols by name | `search(query, scope=\"symbols\")` |
| **Trace call tree / impact** | **`code_graph(name)`** |
| Read full content | `get(id)` |
| Browse memories | `memory_list()` |
| Write memory | `memory_write(id, title, content)` |
| Batch write | `memory_write_batch(entries=[...])` |
| Remove entries | `memory_delete(id)` |
| Revision diffs | `get(id, format=\"history\")` |

Multi-repo: pass `root` to target a repo. `root=\"*\"` for cross-repo.
";

/// Select the base instructions variant based on `MDKB_INSTRUCTIONS_VARIANT` env var.
///
/// Currently only `default` exists. Unknown variants fall back to default with a warning.
/// Add new variants here for A/B testing, then validate with `e2e_search_behaviour` tests.
fn select_base_instructions() -> &'static str {
    static VARIANT: std::sync::OnceLock<&str> = std::sync::OnceLock::new();
    *VARIANT.get_or_init(
        || match std::env::var("MDKB_INSTRUCTIONS_VARIANT").as_deref() {
            Ok("default") | Err(_) => BASE_INSTRUCTIONS,
            Ok(variant) => {
                tracing::warn!(
                    variant,
                    "Unknown MDKB_INSTRUCTIONS_VARIANT, falling back to default"
                );
                BASE_INSTRUCTIONS
            }
        },
    )
}

/// Build server instructions combining base instructions with memory index.
///
/// Always includes base instructions explaining what mdkb is.
/// Appends memory warmup index when memory entries exist.
fn build_server_instructions(index: &[String]) -> String {
    let mut instructions = select_base_instructions().to_string();

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

/// Format a Unix timestamp as a compact relative time string (e.g., "3d ago", "2mo ago").
fn relative_time_ago(unix_ts: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let secs = (now - unix_ts).max(0);

    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 7 * 86400 {
        format!("{}d ago", secs / 86400)
    } else if secs < 30 * 86400 {
        format!("{}w ago", secs / (7 * 86400))
    } else if secs < 365 * 86400 {
        format!("{}mo ago", secs / (30 * 86400))
    } else {
        format!("{}y ago", secs / (365 * 86400))
    }
}

/// Truncate text to a maximum length with ellipsis.
fn truncate_text(text: &str, max_len: usize) -> String {
    let text = text.replace('\n', " ");
    if text.len() <= max_len {
        text
    } else {
        let mut cut = max_len.saturating_sub(3);
        while !text.is_char_boundary(cut) && cut > 0 {
            cut -= 1;
        }
        format!("{}...", &text[..cut])
    }
}

/// Format search results for output.
/// OOD (out-of-domain) threshold — normalized scores below this suggest weak relevance.
const OOD_SCORE_THRESHOLD: f64 = 0.3;

/// Returns an OOD hint when search results appear outside the indexed knowledge.
///
/// Returns `None` when results are strong enough to be useful.
/// Returns a hint string when results are absent or weak — to be appended to output.
fn ood_hint(result_count: usize, top_score: Option<f64>) -> Option<&'static str> {
    if result_count == 0 {
        return Some(
            "\n> No relevant knowledge found. \
             This query appears outside the scope of indexed content — \
             consider broadening terms or using Grep as fallback.",
        );
    }
    if top_score.is_some_and(|s| s < OOD_SCORE_THRESHOLD) {
        return Some(
            "\n> Low-confidence results — top match score is below threshold. \
             Indexed knowledge may not cover this topic well.",
        );
    }
    None
}

fn format_search_results(results: &[SearchResult], limit: usize) -> String {
    use crate::store::hybrid::lost_in_middle_reorder;

    let filtered: Vec<_> = results.iter().filter(|r| r.score != 0.0).collect();

    if filtered.is_empty() {
        return "No matching documents found.".to_string();
    }

    // Apply lost-in-the-middle reordering
    let mut ordered: Vec<_> = filtered;
    lost_in_middle_reorder(&mut ordered);

    let mut output = if ordered.len() >= limit {
        format!(
            "Showing {} results (limit reached, refine query for more precise results):\n",
            ordered.len()
        )
    } else {
        String::new()
    };

    for r in &ordered {
        let title = r.title.as_deref().unwrap_or("(untitled)");
        if let Some(ref root) = r.repo_root {
            output.push_str(&format!(
                "[{}] {} - {} (score: {:.2}, repo: {})\n",
                r.id, r.path, title, r.score, root
            ));
        } else {
            output.push_str(&format!(
                "[{}] {} - {} (score: {:.2})\n",
                r.id, r.path, title, r.score
            ));
        }
        for snippet in &r.snippets {
            output.push_str(&format!("  {}\n", snippet));
        }
    }

    // Hint: guide the model toward get() for retrieval
    let ids: Vec<_> = ordered.iter().map(|r| r.id.to_string()).collect();
    if ids.len() == 1 {
        output.push_str(&format!("\nUse get(\"{}\") to read.", ids[0]));
    } else {
        output.push_str(&format!(
            "\nUse get(\"{}\") to read one, or get(\"{}\") for all.",
            ids[0],
            ids.join(",")
        ));
    }

    output
}

/// Format memory search results for output.
fn format_memory_search_results(entries: &[memory::MemoryEntry]) -> String {
    use crate::store::hybrid::lost_in_middle_reorder;

    if entries.is_empty() {
        return "No matching memory entries found.".to_string();
    }

    // Apply lost-in-the-middle reordering
    let mut ordered: Vec<_> = entries.iter().collect();
    lost_in_middle_reorder(&mut ordered);

    let mut out = format!("Found {} memory entries:\n\n", entries.len());
    for entry in &ordered {
        out.push_str(&format!(
            "- [{}] {} ({}, conf:{:.2}, {}): {}\n",
            entry.id,
            entry.title,
            entry.entry_type,
            entry.confidence(),
            relative_time_ago(entry.updated_at),
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

    // --- OOD detection tests ---

    #[test]
    fn test_ood_hint_zero_results() {
        let hint = ood_hint(0, None);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("No relevant knowledge found"));
    }

    #[test]
    fn test_ood_hint_zero_results_with_score() {
        // score is irrelevant when count is 0
        let hint = ood_hint(0, Some(0.9));
        assert!(hint.is_some());
    }

    #[test]
    fn test_ood_hint_low_score() {
        let hint = ood_hint(3, Some(0.1));
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("Low-confidence"));
    }

    #[test]
    fn test_ood_hint_score_at_threshold_is_low() {
        // score exactly at threshold (< 0.3) → hint
        let hint = ood_hint(1, Some(0.29));
        assert!(hint.is_some());
    }

    #[test]
    fn test_ood_hint_score_above_threshold_no_hint() {
        let hint = ood_hint(3, Some(0.5));
        assert!(hint.is_none());
    }

    #[test]
    fn test_ood_hint_good_results_no_hint() {
        let hint = ood_hint(5, Some(0.85));
        assert!(hint.is_none());
    }

    #[test]
    fn test_ood_hint_results_no_score_no_hint() {
        // memory search passes None score — only triggers on zero count
        let hint = ood_hint(2, None);
        assert!(hint.is_none());
    }

    #[test]
    fn test_format_search_results_empty() {
        let results: Vec<SearchResult> = vec![];
        let output = format_search_results(&results, 10);
        assert!(
            output.starts_with("No matching documents"),
            "Should start with 'No matching documents', got: {}",
            output
        );
    }

    #[test]
    fn test_format_search_results_no_collection_prefix() {
        let results = vec![SearchResult {
            id: 1,
            collection: "docs".to_string(),
            path: "readme.md".to_string(),
            title: Some("README".to_string()),
            score: -5.5,
            snippets: vec!["...matching text...".to_string()],
            status: None,
            superseded_by: None,
            repo_root: None,
        }];
        let output = format_search_results(&results, 10);
        assert!(
            output.contains("[1] readme.md"),
            "Should show path without collection prefix, got: {output}"
        );
        assert!(
            !output.contains("docs:"),
            "Should not contain collection prefix, got: {output}"
        );
        assert!(output.contains("README"));
    }

    #[test]
    fn test_format_search_results_get_hint() {
        let results = vec![
            SearchResult {
                id: 10,
                collection: "docs".to_string(),
                path: "auth.md".to_string(),
                title: Some("Auth Guide".to_string()),
                score: 0.85,
                snippets: vec![],
                status: None,
                superseded_by: None,
                repo_root: None,
            },
            SearchResult {
                id: 20,
                collection: "wiki".to_string(),
                path: "security.md".to_string(),
                title: Some("Security".to_string()),
                score: 0.70,
                snippets: vec![],
                status: None,
                superseded_by: None,
                repo_root: None,
            },
        ];
        let output = format_search_results(&results, 10);
        assert!(
            output.contains("get(\"10\")"),
            "Should hint single get, got: {output}"
        );
        assert!(
            output.contains("get(\"10,20\")"),
            "Should hint batch get, got: {output}"
        );
    }

    #[test]
    fn test_format_search_results_filters_zero_score() {
        let results = vec![
            SearchResult {
                id: 1,
                collection: "docs".to_string(),
                path: "good.md".to_string(),
                title: Some("Good".to_string()),
                score: 0.85,
                snippets: vec![],
                status: None,
                superseded_by: None,
                repo_root: None,
            },
            SearchResult {
                id: 2,
                collection: "docs".to_string(),
                path: "zero.md".to_string(),
                title: Some("Zero".to_string()),
                score: 0.00,
                snippets: vec![],
                status: None,
                superseded_by: None,
                repo_root: None,
            },
        ];
        let output = format_search_results(&results, 10);
        assert!(
            output.contains("good.md"),
            "Should include result with positive score"
        );
        assert!(
            !output.contains("zero.md"),
            "Should filter out result with score 0.00, got: {output}"
        );
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
        assert!(result.contains("# mdkb"));
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
        assert!(result.contains("# mdkb"));
        assert!(result.contains("memory_write"));
        assert!(result.contains("search(query)"));

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
            index.push(format!(
                "entry-{i}: Some title for entry number {i} #tag1 #tag2"
            ));
        }
        let result = build_server_instructions(&index);
        let tokens = count_tokens(&result);

        // Should be under 2K tokens for 50 entries
        assert!(
            tokens < 2000,
            "Warmup exceeds token budget: {} tokens",
            tokens
        );
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
        use rmcp::model::EmptyObject;
        use std::time::Duration;

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
            let result =
                tokio::time::timeout(timeout_duration, server.status(Parameters(EmptyObject {})))
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
        assert!(
            result.is_ok(),
            "status() should auto-init and succeed, got: {:?}",
            result.err()
        );

        // Verify .mdkb/ was created
        assert!(
            root.join(".mdkb").exists(),
            ".mdkb/ should have been created"
        );
        assert!(
            root.join(".mdkb/index.sqlite").exists(),
            "database should exist"
        );
        assert!(
            root.join(".mdkb/config.toml").exists(),
            "config should exist"
        );
    }

    #[test]
    fn test_build_server_instructions_contains_base_instructions() {
        // Even with no memories, instructions should explain what mdkb is
        let index: Vec<String> = vec![];
        let result = build_server_instructions(&index);

        // Must contain base instructions explaining mdkb purpose
        assert!(result.contains("mdkb"), "Should mention mdkb");
        assert!(
            result.contains("# mdkb"),
            "Should explain what mdkb is"
        );
        assert!(
            result.contains("search(query)"),
            "Should mention search tool"
        );
        assert!(
            result.contains("memory_write"),
            "Should mention memory tools"
        );
        assert!(
            result.contains("memory_list"),
            "Should mention memory_list tool"
        );
    }

    #[test]
    fn test_build_server_instructions_includes_memory_when_present() {
        let index = vec!["auth-flow: OAuth2 implementation #auth".to_string()];
        let result = build_server_instructions(&index);

        // Should contain both base instructions and memory
        assert!(
            result.contains("# mdkb"),
            "Should have base instructions"
        );
        assert!(
            result.contains("auth-flow"),
            "Should include memory entries"
        );
    }

    /// Test that multiple different tool calls don't deadlock.
    #[tokio::test]
    async fn test_mcp_multiple_tools_no_deadlock() {
        use rmcp::model::EmptyObject;
        use std::time::Duration;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

        let server = McpServer::new(root);
        let timeout_duration = Duration::from_secs(5);

        // Call status tool multiple times to verify no deadlock
        let tools_to_test = vec!["status", "status", "status"];

        for (i, tool_name) in tools_to_test.iter().enumerate() {
            let result = match *tool_name {
                "status" => tokio::time::timeout(
                    timeout_duration,
                    server.status(Parameters(EmptyObject {})),
                )
                .await
                .map(|r| r.map(|_| ())),
                _ => unreachable!(),
            };

            match result {
                Ok(Ok(_)) => {} // Success
                Ok(Err(e)) => panic!("Tool {} ({}) failed: {:?}", tool_name, i, e),
                Err(_) => panic!("Tool {} ({}) timed out - likely deadlock!", tool_name, i),
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
        server
            .memory_write(Parameters(MemoryWriteParams {
                id: "test-delete-me".to_string(),
                title: "Deletable entry".to_string(),
                content: "This will be deleted.".to_string(),
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                root: None,
            }))
            .await
            .expect("Failed to write memory entry");

        // Delete it
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.memory_delete(Parameters(MemoryDeleteParams {
                id: "test-delete-me".to_string(),
                root: None,
            })),
        )
        .await;

        match result {
            Ok(Ok(r)) => {
                let text = extract_text(&r);
                assert!(
                    text.contains("Deleted memory entry 'test-delete-me'"),
                    "Got: {}",
                    text
                );
            }
            Ok(Err(e)) => panic!("memory_delete failed: {:?}", e),
            Err(_) => panic!("memory_delete timed out - likely deadlock!"),
        }

        // Verify it's gone - get should return not found
        let get_result = server
            .get(Parameters(GetParams {
                id: "test-delete-me".to_string(),
                lines: None,
                format: None,
                root: None,
            }))
            .await;
        assert!(
            get_result.is_err(),
            "get should fail for deleted memory entry"
        );

        // Deleting nonexistent entry should report not found
        let result = server
            .memory_delete(Parameters(MemoryDeleteParams {
                id: "nonexistent".to_string(),
                root: None,
            }))
            .await
            .expect("Should not error");
        let text = extract_text(&result);
        assert!(text.contains("not found"), "Got: {}", text);
    }

    /// Regression test: new tools don't deadlock when called sequentially.
    #[tokio::test]
    async fn test_mcp_new_tools_no_deadlock() {
        use rmcp::model::EmptyObject;
        use std::time::Duration;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

        let server = McpServer::new(root);
        let timeout = Duration::from_secs(5);

        // Sequence: status -> memory_write -> memory_delete -> status
        tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
            .await
            .expect("timeout")
            .expect("status failed");

        tokio::time::timeout(
            timeout,
            server.memory_write(Parameters(MemoryWriteParams {
                id: "deadlock-test".to_string(),
                title: "Test".to_string(),
                content: "Content".to_string(),
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                root: None,
            })),
        )
        .await
        .expect("timeout")
        .expect("memory_write failed");

        tokio::time::timeout(
            timeout,
            server.memory_delete(Parameters(MemoryDeleteParams {
                id: "deadlock-test".to_string(),
                root: None,
            })),
        )
        .await
        .expect("timeout")
        .expect("memory_delete failed");

        tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
            .await
            .expect("timeout")
            .expect("status failed");
    }

    /// Extract the error message from a failed MCP tool result.
    fn extract_error_msg(result: Result<CallToolResult, McpError>) -> String {
        match result {
            Err(e) => e.message.into_owned(),
            Ok(r) => panic!("Expected error, got success: {}", extract_text(&r)),
        }
    }

    // --- Error message tests ---
    //
    // Verify that error messages are concise and contain the relevant ID.

    #[tokio::test]
    async fn test_get_not_found_error() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .get(Parameters(GetParams {
                id: "nonexistent".to_string(),
                lines: None,
                format: None,
                root: None,
            }))
            .await;

        let msg = extract_error_msg(result);
        assert!(
            msg.contains("Not found"),
            "Error should indicate not found, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_get_numeric_id_not_found_error() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .get(Parameters(GetParams {
                id: "999999".to_string(),
                lines: None,
                format: None,
                root: None,
            }))
            .await;

        let msg = extract_error_msg(result);
        assert!(
            msg.contains("Not found"),
            "Error should indicate not found, got: {}",
            msg
        );
    }

    #[tokio::test]
    async fn test_search_no_results() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .search(Parameters(SearchParams {
                query: "zzzznonexistentquery99999".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: None,
                kind: None,
                file: None,
                threshold: 0.3,
                root: None,
            }))
            .await
            .expect("search should not error");

        let text = extract_text(&result);
        assert!(
            text.contains("No relevant knowledge found"),
            "Should indicate no relevant knowledge (OOD), got: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_get_glob_no_results() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .get(Parameters(GetParams {
                id: "nonexistent/**/*.md".to_string(),
                lines: None,
                format: None,
                root: None,
            }))
            .await
            .expect("get with glob should not error");

        let text = extract_text(&result);
        assert!(
            text.contains("No documents"),
            "Should indicate no documents, got: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_search_memory_scope_no_results() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .search(Parameters(SearchParams {
                query: "zzzznonexistentquery99999".to_string(),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: Some("memory".to_string()),
                kind: None,
                file: None,
                threshold: 0.3,
                root: None,
            }))
            .await
            .expect("search with memory scope should not error");

        let text = extract_text(&result);
        assert!(
            text.contains("No matching memory entries"),
            "Should indicate no memory entries, got: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_memory_delete_not_found_has_hint() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .memory_delete(Parameters(MemoryDeleteParams {
                id: "nonexistent-entry".to_string(),
                root: None,
            }))
            .await
            .expect("memory_delete should not error");

        let text = extract_text(&result);
        assert!(
            text.contains("not found"),
            "Should indicate entry not found, got: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_memory_list_empty() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .memory_list(Parameters(MemoryListParams {
                limit: 20,
                sort: "recent".to_string(),
                root: None,
            }))
            .await
            .expect("memory_list should not error");

        let text = extract_text(&result);
        assert!(
            text.contains("No memory entries"),
            "Should indicate no entries, got: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_memory_list_with_entries() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        // Write two entries
        server
            .memory_write(Parameters(MemoryWriteParams {
                id: "test-a".to_string(),
                title: "Test A".to_string(),
                content: "Content A".to_string(),
                entry_type: "topic".to_string(),
                tags: vec!["tag1".to_string()],
                source_type: "user_statement".to_string(),
                root: None,
            }))
            .await
            .expect("write A");

        server
            .memory_write(Parameters(MemoryWriteParams {
                id: "test-b".to_string(),
                title: "Test B".to_string(),
                content: "Content B".to_string(),
                entry_type: "problem".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                root: None,
            }))
            .await
            .expect("write B");

        let result = server
            .memory_list(Parameters(MemoryListParams {
                limit: 20,
                sort: "newest".to_string(),
                root: None,
            }))
            .await
            .expect("memory_list should not error");

        let text = extract_text(&result);
        assert!(
            text.contains("test-a"),
            "Should contain test-a, got: {}",
            text
        );
        assert!(
            text.contains("test-b"),
            "Should contain test-b, got: {}",
            text
        );
        assert!(
            text.contains("Found 2"),
            "Should indicate 2 entries, got: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_memory_list_invalid_sort() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .memory_list(Parameters(MemoryListParams {
                limit: 20,
                sort: "invalid".to_string(),
                root: None,
            }))
            .await;

        assert!(result.is_err(), "Should error on invalid sort");
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

            std::fs::write(
                src_dir.join("main.rs"),
                r#"
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
"#,
            )
            .unwrap();

            std::fs::write(
                src_dir.join("lib.rs"),
                r#"
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
"#,
            )
            .unwrap();

            // Create code index
            let index_path = root.join(".mdkb/code.sqlite");
            let mut facade = IndexFacade::create(&index_path).expect("Failed to create code index");
            facade
                .index_directory(&src_dir)
                .expect("Failed to index source files");

            let server = McpServer::new(root);
            (temp_dir, server)
        }

        #[tokio::test]
        async fn test_search_symbols_scope() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "main".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed");

            let text = extract_text(&result);
            assert!(text.contains("main"), "Should find main: {}", text);
            assert!(text.contains("sym#"), "Should include symbol ID: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_not_found() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "nonexistent_symbol_xyz".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed");

            let text = extract_text(&result);
            assert!(text.contains("No symbols found"), "Got: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_with_kind_filter() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "DataHelper".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: Some("struct".to_string()),
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols kind=struct failed");

            let text = extract_text(&result);
            assert!(
                text.contains("DataHelper"),
                "Should find DataHelper: {}",
                text
            );
            assert!(text.contains("Struct"), "Should be a struct: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_with_file_filter() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "utility".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: Some("lib.rs".to_string()),
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols file=lib.rs failed");

            let text = extract_text(&result);
            assert!(text.contains("utility"), "Should find utility: {}", text);
            assert!(text.contains("lib.rs"), "Should be in lib.rs: {}", text);
        }

        #[tokio::test]
        async fn test_search_symbols_invalid_kind() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "main".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: Some("invalid_kind".to_string()),
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout");

            assert!(result.is_err(), "Should error on invalid kind");
        }

        #[tokio::test]
        async fn test_search_symbols_fuzzy() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "process".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed");

            let text = extract_text(&result);
            assert!(
                text.contains("process_data"),
                "Should find process_data: {}",
                text
            );
        }

        #[tokio::test]
        async fn test_code_graph_calls() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "process_data".to_string(),
                    direction: "calls".to_string(),
                    symbol_id: None,
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("code_graph direction=calls failed");

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

            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "process_data".to_string(),
                    direction: "callers".to_string(),
                    symbol_id: None,
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("code_graph direction=callers failed");

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

            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "validate".to_string(),
                    direction: "impact".to_string(),
                    symbol_id: None,
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("code_graph direction=impact failed");

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
            tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "main".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed");

            let result = tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
                .await
                .expect("timeout")
                .expect("status failed");

            let text = extract_text(&result);
            assert!(
                text.contains("Code Index"),
                "Should include code index section: {}",
                text
            );
            assert!(text.contains("Symbols:"), "Should list symbols: {}", text);
            assert!(text.contains("Files:"), "Should list files: {}", text);
            assert!(
                text.contains("Relationships:"),
                "Should list relationships: {}",
                text
            );
        }

        #[tokio::test]
        async fn test_code_intel_tools_no_deadlock() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // Call multiple code-intel tools in sequence to verify no deadlock
            tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
                .await
                .expect("timeout")
                .expect("status failed");

            tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "main".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed");

            tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "data".to_string(),
                    limit: 5,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed (second call)");

            tokio::time::timeout(timeout, server.status(Parameters(EmptyObject {})))
                .await
                .expect("timeout")
                .expect("status failed (second call)");
        }

        #[tokio::test]
        async fn test_resolve_symbol_not_found() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // code_graph with nonexistent symbol should return an error
            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "nonexistent_fn".to_string(),
                    direction: "calls".to_string(),
                    symbol_id: None,
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout");

            assert!(result.is_err(), "Should error for nonexistent symbol");
        }

        #[tokio::test]
        async fn test_resolve_symbol_by_id() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // First find a symbol to get its ID via search scope=symbols
            let find_result = tokio::time::timeout(
                timeout,
                server.search(Parameters(SearchParams {
                    query: "main".to_string(),
                    limit: 10,
                    collection: None,
                    include_superseded: false,
                    scope: Some("symbols".to_string()),
                    kind: None,
                    file: None,
                    threshold: 0.3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed");

            let text = extract_text(&find_result);
            // Extract symbol ID from "sym#N"
            let sym_id: u32 = text
                .split("sym#")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse().ok())
                .expect("Should find sym# in output");

            // Now use that ID with code_graph
            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "main".to_string(),
                    direction: "calls".to_string(),
                    symbol_id: Some(sym_id),
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("code_graph with ID failed");

            let text = extract_text(&result);
            assert!(
                text.contains("main") || text.contains("does not call"),
                "Should work with symbol_id: {}",
                text
            );
        }
    }

    #[test]
    fn test_classify_change_rs_outside_collection() {
        let path = Path::new("/project/src/main.rs");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(
            classify_change(path, &collections, &excludes),
            (true, false)
        );
    }

    #[test]
    fn test_classify_change_md_in_collection() {
        let path = Path::new("/project/docs/readme.md");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(
            classify_change(path, &collections, &excludes),
            (false, true)
        );
    }

    #[test]
    fn test_classify_change_rs_in_collection() {
        let path = Path::new("/project/docs/example.rs");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(classify_change(path, &collections, &excludes), (true, true));
    }

    #[test]
    fn test_classify_change_irrelevant_file() {
        let path = Path::new("/project/data.json");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(
            classify_change(path, &collections, &excludes),
            (false, false)
        );
    }

    #[test]
    fn test_classify_change_excludes_node_modules() {
        let root = Path::new("/project");
        let collections = vec![PathBuf::from("/project/docs")];
        let patterns = vec!["**/node_modules/**".to_string()];
        let excludes = build_code_excludes(root, &patterns);

        // .js in node_modules should NOT be classified as code
        let path = Path::new("/project/node_modules/lodash/index.js");
        assert_eq!(
            classify_change(path, &collections, &excludes),
            (false, false)
        );

        // .js outside node_modules should still be code
        let path = Path::new("/project/src/app.js");
        assert_eq!(
            classify_change(path, &collections, &excludes),
            (true, false)
        );
    }

    #[test]
    fn test_classify_change_excludes_multiple_patterns() {
        let root = Path::new("/project");
        let collections: Vec<PathBuf> = vec![];
        let patterns = vec![
            "**/node_modules/**".to_string(),
            "**/dist/**".to_string(),
            "**/target/**".to_string(),
        ];
        let excludes = build_code_excludes(root, &patterns);

        assert_eq!(
            classify_change(
                Path::new("/project/node_modules/x/a.js"),
                &collections,
                &excludes
            ),
            (false, false)
        );
        assert_eq!(
            classify_change(
                Path::new("/project/dist/bundle.js"),
                &collections,
                &excludes
            ),
            (false, false)
        );
        assert_eq!(
            classify_change(
                Path::new("/project/target/debug/build.rs"),
                &collections,
                &excludes
            ),
            (false, false)
        );
        assert_eq!(
            classify_change(Path::new("/project/src/main.rs"), &collections, &excludes),
            (true, false)
        );
    }

    #[tokio::test]
    async fn test_memory_write_does_not_increment_access_count() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        // Create an entry
        server
            .memory_write(Parameters(MemoryWriteParams {
                id: "test-entry".to_string(),
                title: "Test Entry".to_string(),
                content: "Initial content".to_string(),
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                root: None,
            }))
            .await
            .expect("write should succeed");

        // Get access_count after creation
        let ctx_guard = server.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let entry_after_create =
            crate::store::memory::get_entry_without_tracking(&ctx.conn, "test-entry")
                .unwrap()
                .unwrap();
        let count_after_create = entry_after_create.access_count;
        drop(ctx_guard);

        // Update the entry (this should NOT increment access_count)
        server
            .memory_write(Parameters(MemoryWriteParams {
                id: "test-entry".to_string(),
                title: "Test Entry".to_string(),
                content: "Updated content".to_string(),
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                root: None,
            }))
            .await
            .expect("update should succeed");

        // access_count must not have changed
        let ctx_guard = server.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let entry_after_update =
            crate::store::memory::get_entry_without_tracking(&ctx.conn, "test-entry")
                .unwrap()
                .unwrap();
        assert_eq!(
            entry_after_update.access_count, count_after_create,
            "memory_write must not increment access_count (was {}, now {})",
            count_after_create, entry_after_update.access_count
        );
    }

    #[tokio::test]
    async fn test_ensure_context_returns_error_during_doc_reindex() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");

        let server = McpServer::new(root);

        // Initialize context normally first
        server
            .ensure_context()
            .await
            .expect("initial ensure_context should succeed");

        // Simulate startup taking ctx for reindex
        server.doc_reindex_active.store(true, Ordering::Relaxed);
        {
            let mut ctx_guard = server.ctx.lock().await;
            let _taken = ctx_guard.take(); // take ctx out
        }

        // ensure_context should return an error, not hang or create a new ctx
        let result = server.ensure_context().await;
        assert!(
            result.is_err(),
            "ensure_context should fail during doc reindex"
        );
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("initializing"),
            "error should mention initializing, got: {}",
            err_msg
        );

        // ctx should still be None (no new ctx created)
        let ctx_guard = server.ctx.lock().await;
        assert!(
            ctx_guard.is_none(),
            "ctx should remain None while reindex is active"
        );
    }

    #[test]
    fn test_uri_to_path_valid() {
        let path = uri_to_path("file:///Users/me/project");
        assert_eq!(path, Some(PathBuf::from("/Users/me/project")));
    }

    #[test]
    fn test_uri_to_path_no_scheme() {
        assert_eq!(uri_to_path("/Users/me/project"), None);
    }

    #[test]
    fn test_uri_to_path_http_scheme() {
        assert_eq!(uri_to_path("http://example.com"), None);
    }

    #[test]
    fn test_global_constructor() {
        let config = crate::daemon::config::DaemonConfig::default();
        let registry = std::sync::Arc::new(RepoRegistry::new(config));
        let server = McpServer::global(registry);
        assert!(server.is_global());
    }

    #[test]
    fn test_standalone_not_global() {
        let root = tempfile::TempDir::new().unwrap();
        let server = McpServer::new(root.path().to_path_buf());
        assert!(!server.is_global());
    }

    #[tokio::test]
    async fn test_resolve_handle_standalone_shares_arcs() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).unwrap();
        let server = McpServer::new(root);

        let handle = server.resolve_handle(None).await.unwrap();
        // The handle's ctx and code_index must be the SAME Arcs as the server's
        assert!(Arc::ptr_eq(&handle.ctx, &server.ctx));
        assert!(Arc::ptr_eq(&handle.code_index, &server.code_index));
        assert!(Arc::ptr_eq(
            &handle.doc_reindex_active,
            &server.doc_reindex_active
        ));
    }

    #[tokio::test]
    async fn test_resolve_handle_global_single_root() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();

        let config = crate::daemon::config::DaemonConfig::default();
        let registry = Arc::new(RepoRegistry::new(config));
        registry.get_or_open(&root).unwrap();

        let server = McpServer::global(Arc::clone(&registry));
        // No root param, 1 root registered → auto-selects
        let handle = server.resolve_handle(None).await.unwrap();
        assert_eq!(handle.root, root.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn test_resolve_handle_global_no_roots() {
        let config = crate::daemon::config::DaemonConfig::default();
        let registry = Arc::new(RepoRegistry::new(config));

        let server = McpServer::global(registry);
        let err = server.resolve_handle(None).await.unwrap_err();
        assert!(
            format!("{:?}", err).contains("No repos registered"),
            "Should error on empty registry: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_resolve_handle_global_multiple_roots_no_param() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp1.path().join(".mdkb")).unwrap();
        std::fs::create_dir_all(tmp2.path().join(".mdkb")).unwrap();

        let config = crate::daemon::config::DaemonConfig::default();
        let registry = Arc::new(RepoRegistry::new(config));
        registry.get_or_open(tmp1.path()).unwrap();
        registry.get_or_open(tmp2.path()).unwrap();

        let server = McpServer::global(registry);
        let err = server.resolve_handle(None).await.unwrap_err();
        assert!(
            format!("{:?}", err).contains("Multiple repos"),
            "Should error listing roots: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_resolve_handle_global_specific_root() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp1.path().join(".mdkb")).unwrap();
        std::fs::create_dir_all(tmp2.path().join(".mdkb")).unwrap();

        let config = crate::daemon::config::DaemonConfig::default();
        let registry = Arc::new(RepoRegistry::new(config));
        registry.get_or_open(tmp1.path()).unwrap();
        registry.get_or_open(tmp2.path()).unwrap();

        let server = McpServer::global(registry);
        let handle = server
            .resolve_handle(Some(tmp2.path().to_str().unwrap()))
            .await
            .unwrap();
        assert_eq!(handle.root, tmp2.path().canonicalize().unwrap());
    }

    #[tokio::test]
    async fn test_resolve_handle_cross_repo_not_implemented() {
        let config = crate::daemon::config::DaemonConfig::default();
        let registry = Arc::new(RepoRegistry::new(config));
        let server = McpServer::global(registry);

        let err = server.resolve_handle(Some("*")).await.unwrap_err();
        assert!(
            format!("{:?}", err).contains("Cross-repo"),
            "Should report cross-repo not implemented: {:?}",
            err
        );
    }

    #[tokio::test]
    async fn test_cross_repo_search_standalone_errors() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).unwrap();
        let server = McpServer::new(root);

        let result = server
            .search(Parameters(SearchParams {
                query: "test".to_string(),
                root: Some("*".to_string()),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: None,
                kind: None,
                threshold: 0.5,
                file: None,
            }))
            .await;

        assert!(
            result.is_err(),
            "Cross-repo search should fail in standalone mode"
        );
    }

    #[tokio::test]
    async fn test_cross_repo_search_two_repos() {
        use std::time::Duration;

        // Set up two repos with documents
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let root1 = tmp1.path().to_path_buf();
        let root2 = tmp2.path().to_path_buf();

        crate::cli::handlers::handle_init(&root1).unwrap();
        crate::cli::handlers::handle_init(&root2).unwrap();

        // Add a collection and document to each repo
        {
            let ctx1 = crate::cli::handlers::Context::open(&root1).unwrap();
            let docs_dir1 = root1.join("docs");
            std::fs::create_dir_all(&docs_dir1).unwrap();
            std::fs::write(
                docs_dir1.join("alpha.md"),
                "# Alpha\n\nAlpha content about widgets",
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            let coll1 = crate::domain::Collection {
                name: "docs".to_string(),
                path: "docs".to_string(),
                pattern: "**/*.md".to_string(),
                source: "manual".to_string(),
                created_at: now,
                updated_at: now,
            };
            crate::store::collections::add_collection(&ctx1.conn, &coll1).unwrap();
            crate::cli::handlers::handle_update(&ctx1, &root1).unwrap();
        }
        {
            let ctx2 = crate::cli::handlers::Context::open(&root2).unwrap();
            let docs_dir2 = root2.join("docs");
            std::fs::create_dir_all(&docs_dir2).unwrap();
            std::fs::write(
                docs_dir2.join("beta.md"),
                "# Beta\n\nBeta content about widgets",
            )
            .unwrap();
            let now = chrono::Utc::now().timestamp();
            let coll2 = crate::domain::Collection {
                name: "docs".to_string(),
                path: "docs".to_string(),
                pattern: "**/*.md".to_string(),
                source: "manual".to_string(),
                created_at: now,
                updated_at: now,
            };
            crate::store::collections::add_collection(&ctx2.conn, &coll2).unwrap();
            crate::cli::handlers::handle_update(&ctx2, &root2).unwrap();
        }

        // Create global server with both repos
        let config = crate::daemon::config::DaemonConfig::default();
        let registry = Arc::new(RepoRegistry::new(config));
        registry.get_or_open(&root1).unwrap();
        registry.get_or_open(&root2).unwrap();
        let server = McpServer::global(registry);

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.search(Parameters(SearchParams {
                query: "widgets".to_string(),
                root: Some("*".to_string()),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: None,
                kind: None,
                threshold: 0.5,
                file: None,
            })),
        )
        .await
        .expect("timeout")
        .expect("cross-repo search failed");

        let text = extract_text(&result);
        // Should find results from both repos
        assert!(
            text.contains("alpha.md") || text.contains("beta.md"),
            "Should find docs from at least one repo: {}",
            text
        );
        assert!(
            text.contains("repo:"),
            "Should include repo provenance: {}",
            text
        );
    }

    /// Backward compatibility: standalone mode (no --global) works exactly as before.
    #[tokio::test]
    async fn test_backward_compat_standalone_search() {
        use std::time::Duration;

        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).unwrap();

        // Add a doc
        let ctx = crate::cli::handlers::Context::open(&root).unwrap();
        let docs_dir = root.join("docs");
        std::fs::create_dir_all(&docs_dir).unwrap();
        std::fs::write(
            docs_dir.join("readme.md"),
            "# Hello\n\nBackward compat test",
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp();
        let coll = crate::domain::Collection {
            name: "docs".to_string(),
            path: "docs".to_string(),
            pattern: "**/*.md".to_string(),
            source: "manual".to_string(),
            created_at: now,
            updated_at: now,
        };
        crate::store::collections::add_collection(&ctx.conn, &coll).unwrap();
        drop(ctx);
        {
            let mut ctx = crate::cli::handlers::Context::open(&root).unwrap();
            crate::cli::handlers::handle_update(&mut ctx, &root).unwrap();
        }

        // Standalone server — no registry, no root param
        let server = McpServer::new(root);
        assert!(!server.is_global());

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.search(Parameters(SearchParams {
                query: "backward".to_string(),
                root: None, // no root param — standalone
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: None,
                kind: None,
                threshold: 0.5,
                file: None,
            })),
        )
        .await
        .expect("timeout")
        .expect("search failed");

        let text = extract_text(&result);
        assert!(
            text.contains("readme.md"),
            "Should find doc in standalone mode: {}",
            text
        );
    }

    /// Global mode with specific root param routes to the right repo.
    #[tokio::test]
    async fn test_global_mode_specific_root_search() {
        use std::time::Duration;

        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        let root1 = tmp1.path().to_path_buf();
        let root2 = tmp2.path().to_path_buf();

        crate::cli::handlers::handle_init(&root1).unwrap();
        crate::cli::handlers::handle_init(&root2).unwrap();

        // Add doc only to repo2
        {
            let ctx = crate::cli::handlers::Context::open(&root2).unwrap();
            let docs_dir = root2.join("docs");
            std::fs::create_dir_all(&docs_dir).unwrap();
            std::fs::write(docs_dir.join("unique.md"), "# Unique\n\nOnly in repo2").unwrap();
            let now = chrono::Utc::now().timestamp();
            let coll = crate::domain::Collection {
                name: "docs".to_string(),
                path: "docs".to_string(),
                pattern: "**/*.md".to_string(),
                source: "manual".to_string(),
                created_at: now,
                updated_at: now,
            };
            crate::store::collections::add_collection(&ctx.conn, &coll).unwrap();
            drop(ctx);
            let mut ctx = crate::cli::handlers::Context::open(&root2).unwrap();
            crate::cli::handlers::handle_update(&mut ctx, &root2).unwrap();
        }

        let config = crate::daemon::config::DaemonConfig::default();
        let registry = Arc::new(RepoRegistry::new(config));
        registry.get_or_open(&root1).unwrap();
        registry.get_or_open(&root2).unwrap();
        let server = McpServer::global(registry);

        // Search with root pointing to repo2
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.search(Parameters(SearchParams {
                query: "unique".to_string(),
                root: Some(root2.to_string_lossy().to_string()),
                limit: 10,
                collection: None,
                include_superseded: false,
                scope: None,
                kind: None,
                threshold: 0.5,
                file: None,
            })),
        )
        .await
        .expect("timeout")
        .expect("search with root failed");

        let text = extract_text(&result);
        assert!(
            text.contains("unique.md"),
            "Should find doc in targeted repo: {}",
            text
        );
    }

    #[tokio::test]
    async fn test_memory_write_batch_creates_multiple_entries() {
        use super::super::tools::MemoryWriteBatchEntry;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .memory_write_batch(Parameters(MemoryWriteBatchParams {
                entries: vec![
                    MemoryWriteBatchEntry {
                        id: "batch-a".to_string(),
                        title: "Batch A".to_string(),
                        content: "Content A".to_string(),
                        entry_type: "topic".to_string(),
                        tags: vec!["test".to_string()],
                        source_type: "user_statement".to_string(),
                    },
                    MemoryWriteBatchEntry {
                        id: "batch-b".to_string(),
                        title: "Batch B".to_string(),
                        content: "Content B".to_string(),
                        entry_type: "decision".to_string(),
                        tags: vec![],
                        source_type: "user_statement".to_string(),
                    },
                ],
                root: None,
            }))
            .await
            .expect("batch write should succeed");

        let text = extract_text(&result);
        assert!(text.contains("Created memory entry: batch-a"), "Should contain batch-a: {text}");
        assert!(text.contains("Created memory entry: batch-b"), "Should contain batch-b: {text}");

        // Verify entries exist in DB
        let ctx_guard = server.ctx.lock().await;
        let ctx = ctx_guard.as_ref().unwrap();
        let a = crate::store::memory::get_entry_without_tracking(&ctx.conn, "batch-a")
            .unwrap()
            .expect("batch-a should exist");
        let b = crate::store::memory::get_entry_without_tracking(&ctx.conn, "batch-b")
            .unwrap()
            .expect("batch-b should exist");
        assert_eq!(a.title, "Batch A");
        assert_eq!(b.title, "Batch B");
        assert_eq!(b.entry_type, crate::store::memory::EntryType::Decision);
    }

    #[tokio::test]
    async fn test_memory_write_batch_empty_fails() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let result = server
            .memory_write_batch(Parameters(MemoryWriteBatchParams {
                entries: vec![],
                root: None,
            }))
            .await;

        assert!(result.is_err(), "empty batch should fail");
    }

    #[tokio::test]
    async fn test_memory_write_batch_over_limit_fails() {
        use super::super::tools::MemoryWriteBatchEntry;

        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let root = temp_dir.path().to_path_buf();
        crate::cli::handlers::handle_init(&root).expect("Failed to init mdkb");
        let server = McpServer::new(root);

        let entries: Vec<MemoryWriteBatchEntry> = (0..21)
            .map(|i| MemoryWriteBatchEntry {
                id: format!("over-{i}"),
                title: format!("Over {i}"),
                content: format!("Content {i}"),
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
            })
            .collect();

        let result = server
            .memory_write_batch(Parameters(MemoryWriteBatchParams {
                entries,
                root: None,
            }))
            .await;

        assert!(result.is_err(), "over-limit batch should fail");
    }

    // --- relative_time_ago tests ---

    #[test]
    fn test_relative_time_ago_seconds() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time_ago(now - 30), "just now");
        assert_eq!(relative_time_ago(now), "just now");
        assert_eq!(relative_time_ago(now - 59), "just now");
    }

    #[test]
    fn test_relative_time_ago_minutes() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time_ago(now - 120), "2m ago");
        assert_eq!(relative_time_ago(now - 3500), "58m ago");
    }

    #[test]
    fn test_relative_time_ago_hours() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time_ago(now - 3600), "1h ago");
        assert_eq!(relative_time_ago(now - 7200), "2h ago");
        assert_eq!(relative_time_ago(now - 23 * 3600), "23h ago");
    }

    #[test]
    fn test_relative_time_ago_days() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time_ago(now - 86400), "1d ago");
        assert_eq!(relative_time_ago(now - 6 * 86400), "6d ago");
    }

    #[test]
    fn test_relative_time_ago_weeks() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time_ago(now - 14 * 86400), "2w ago");
        assert_eq!(relative_time_ago(now - 27 * 86400), "3w ago");
    }

    #[test]
    fn test_relative_time_ago_months() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time_ago(now - 45 * 86400), "1mo ago");
        assert_eq!(relative_time_ago(now - 180 * 86400), "6mo ago");
        assert_eq!(relative_time_ago(now - 364 * 86400), "12mo ago");
    }

    #[test]
    fn test_relative_time_ago_years() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time_ago(now - 366 * 86400), "1y ago");
        assert_eq!(relative_time_ago(now - 730 * 86400), "2y ago");
    }

    #[test]
    fn test_relative_time_ago_future() {
        let now = chrono::Utc::now().timestamp();
        // Future timestamps should still say "just now"
        assert_eq!(relative_time_ago(now + 1000), "just now");
    }
}
