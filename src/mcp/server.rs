//! MCP server implementation.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

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

use crate::code::indexing::IndexFacade;
use crate::code::types::SymbolId;
use crate::config::McpConfig;
use crate::core::Context;
use crate::core::indexing::handle_update;
use crate::domain::SearchResult;
use crate::metrics::{UsageMetrics, count_tokens};
use crate::store::{collections, documents, memory, stats};
use crate::watcher::{FileWatcher, WatcherConfig};

use super::tools::{
    CodeGraphParams, GetParams, GraphParams, MemoryConfirmParams, MemoryDeleteParams,
    MemoryListParams, MemoryWriteBatchParams, MemoryWriteParams, SearchParams, UsageParams,
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
    /// Code intelligence index (SQLite-backed).
    code_index: Arc<Mutex<Option<IndexFacade>>>,
    /// Tool router.
    tool_router: ToolRouter<Self>,
    /// Usage metrics tracker (in-memory).
    metrics: Arc<UsageMetrics>,
    /// MCP configuration. Retained for the constructor surface; tool impls
    /// now read settings from `full_config.mcp` via the per-repo handle.
    #[allow(dead_code)]
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
    /// Persistent tool call counter — drives drift-gated `PRAGMA optimize`.
    persistent_call_count: Arc<AtomicU64>,
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
            persistent_call_count: Arc::new(AtomicU64::new(0)),
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
            persistent_call_count: Arc::new(AtomicU64::new(0)),
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
            persistent_call_count: Arc::new(AtomicU64::new(0)),
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
        let (output, result_count) =
            super::dispatch::cross_repo_search_impl(&handles, params).await?;

        let tokens = count_tokens(&output);
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
    /// - `root` = "*": rejected here; only `search` handles cross-repo fan-out.
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
                        "root=\"*\" is supported only by search. Pass the exact repository root for get and other tools.",
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

    /// Query MCP roots from the client peer and register them in the registry.
    /// Takes `peer` by value so this can run inside a detached task without
    /// blocking the server's notification loop on a slow client reply.
    async fn sync_roots_from_peer(peer: &rmcp::Peer<RoleServer>, registry: &RepoRegistry) {
        match peer.list_roots().await {
            Ok(result) => {
                for root in &result.roots {
                    if let Some(path) = uri_to_path(&root.uri) {
                        // Anchor the client-provided root the same way hooks do
                        // (nearest existing store → git root → launch dir) so
                        // MCP and hooks converge on one store per project even
                        // when the client launched in a sub-directory.
                        let Some(anchor) = crate::git::resolve_project_root(&path, None) else {
                            tracing::warn!(
                                "Ignoring root {}: it holds git repositories (or is $HOME), so a \
                                 store there would anchor every repo underneath it",
                                path.display()
                            );
                            continue;
                        };
                        match registry.get_or_open(&anchor) {
                            Ok(_) => {
                                tracing::info!(
                                    "Registered root from MCP client: {}",
                                    anchor.display()
                                );
                            }
                            Err(e) => {
                                tracing::warn!("Failed to register root {}: {e}", anchor.display());
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

    /// Initialize context, set `doc_reindex_active`, and take ctx out of the
    /// lock — all in one lock acquisition. This eliminates a race where tool
    /// calls could see ctx=Some with doc_reindex_active=false between
    /// ensure_context() and the flag set, causing flaky test failures on CI.
    async fn init_and_take_for_reindex(&self) -> Result<Option<Context>, McpError> {
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
            let _writer_guard =
                crate::store::mutation_lock::acquire_writer(&ctx.db_path, "server context setup")
                    .map_err(|e| mcp_error(format!("Failed to acquire writer lock: {e}")))?;
            stats::init_stats_schema(&ctx.conn)
                .map_err(|e| mcp_error(format!("Failed to init stats schema: {}", e)))?;
            self.apply_conventions(&ctx)
                .map_err(|e| mcp_error(format!("Failed to apply conventions: {}", e)))?;
            if self.session_id.load(Ordering::Relaxed) == 0 {
                let session_id = stats::create_session(&ctx.conn)
                    .map_err(|e| mcp_error(format!("Failed to create session: {}", e)))?;
                self.session_id.store(session_id, Ordering::Relaxed);
                tracing::info!("Started stats session {}", session_id);
            }
            *ctx_guard = Some(ctx);
        }
        self.doc_reindex_active.store(true, Ordering::Relaxed);
        Ok(ctx_guard.take())
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

            let _writer_guard =
                crate::store::mutation_lock::acquire_writer(&ctx.db_path, "server context setup")
                    .map_err(|e| mcp_error(format!("Failed to acquire writer lock: {e}")))?;

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
            let pipeline_config = crate::code::indexing::pipeline::PipelineConfig {
                ignore_patterns: self.code_ignore_patterns.clone(),
                respect_gitignore: self.full_config.code.indexing.respect_gitignore,
                ..Default::default()
            };
            facade = facade.with_config(pipeline_config);
            *idx_guard = Some(facade);
        }
        Ok(idx_guard)
    }

    /// Resolve a symbol by ID or name, returning an error for disambiguation.
    ///
    /// If `symbol_id` is provided, looks up by ID directly.
    /// If only `name` is provided, finds all matches. Returns an error with
    /// a disambiguation list if multiple symbols share the name.
    pub(super) fn resolve_symbol(
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
            0 => {
                // Fuzzy fallback: try FTS trigram search (needs >= 3 chars)
                if name.len() >= 3 {
                    let fuzzy = facade.search_symbols(name, 10);
                    match fuzzy.len() {
                        0 => {}
                        1 => return Ok(fuzzy.into_iter().next().unwrap()),
                        _ => return Err(Self::disambiguation_error(name, &fuzzy)),
                    }
                }
                Err(mcp_error(format!("No symbol found: '{name}'.")))
            }
            1 => Ok(matches.into_iter().next().unwrap()),
            _ => Err(Self::disambiguation_error(name, &matches)),
        }
    }

    /// Build a disambiguation error listing candidate symbols.
    fn disambiguation_error(name: &str, candidates: &[crate::code::symbol::Symbol]) -> McpError {
        let mut msg = format!("Multiple symbols match '{}'. Pass symbol_id:\n", name);
        for sym in candidates {
            let scope = match &sym.scope_context {
                Some(crate::code::symbol::ScopeContext::ClassMember {
                    class_name: Some(cn),
                }) => format!(" [in {cn}]"),
                Some(crate::code::symbol::ScopeContext::Local {
                    parent_name: Some(pn),
                    ..
                }) => format!(" [in {pn}]"),
                _ => String::new(),
            };
            let sig = sym
                .signature
                .as_ref()
                .map(|s| format!(" `{}`", truncate_text(s.trim(), 60)))
                .unwrap_or_default();
            msg.push_str(&format!(
                "  sym#{} - {:?} {} in {} ({}){}{}\n",
                sym.id.value(),
                sym.kind,
                sym.name,
                sym.file_path,
                sym.range,
                scope,
                sig,
            ));
        }
        mcp_error(msg)
    }

    /// Record a tool call to persistent stats.
    /// The current session id as a provenance string, or `None` before a session
    /// is established (session_id == 0).
    fn session_provenance(&self) -> Option<String> {
        let id = self.session_id.load(Ordering::Relaxed);
        (id > 0).then(|| id.to_string())
    }

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

        let mut ctx_guard = self.ctx.lock().await;
        if ctx_guard.is_none() {
            return;
        }
        let call_count = self.persistent_call_count.fetch_add(1, Ordering::Relaxed) + 1;
        let interval = self.full_config.db.optimize_interval_calls;
        let outcome =
            crate::core::run_guarded_write(&mut ctx_guard, "persistent call telemetry", |ctx| {
                stats::record_call(&ctx.conn, session_id, tool_name, tokens, results, truncated)?;
                if crate::store::maintenance::should_optimize(call_count, interval) {
                    crate::store::maintenance::run_optimize(&ctx.conn)?;
                }
                Ok(())
            });
        if let Some(Err(error)) = outcome {
            tracing::warn!("Failed to record call stats: {error}");
        }
    }

    /// Search documents using hybrid search (BM25 + semantic with RRF fusion).
    #[tool(
        description = "Semantic search (fuzzy, not literal). Searches docs+memory (default), code symbols (scope=\"symbols\"), or semantic code (scope=\"code\"). For exact string/regex matching, use Grep instead."
    )]
    pub async fn search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        if params.root.as_deref() == Some("*") {
            return self.cross_repo_search(&params).await;
        }

        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let (output, result_count) = super::dispatch::search_impl(&handle, &params).await?;

        let tokens = count_tokens(&output);
        self.metrics.record_search(tokens, result_count);
        self.record_persistent_call("search", tokens, result_count, false)
            .await;
        tracing::debug!("mdkb_search: {} tokens, {} results", tokens, result_count);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Retrieve a document by ID or path, with optional line range.
    /// Also accepts memory slugs, glob patterns, and comma-separated lists.
    #[tool(
        description = "Retrieve a document by ID, path, or memory slug, with optional line range. In multi-repo mode, pass the exact repository root; root=\"*\" is supported only by search."
    )]
    pub async fn get(
        &self,
        Parameters(params): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let (output, count, truncated) = super::dispatch::get_impl(&handle, &params).await?;
        let tokens = count_tokens(&output);
        self.metrics.record_get(tokens);
        self.record_persistent_call("get", tokens, count, truncated)
            .await;
        tracing::debug!(
            "mdkb_get: {} tokens, {} results, truncated={}",
            tokens,
            count,
            truncated
        );
        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Get index status including documents, collections, and code index.
    #[tool(description = "Get index status: collections, documents, code index stats.")]
    pub async fn status(
        &self,
        Parameters(_): Parameters<EmptyObject>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(None).await?;
        let output = super::dispatch::status_impl(&handle).await?;

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
        let outcome =
            super::dispatch::update_impl(&handle, &crate::core::indexing::UpdateRequest::default())
                .await?;
        let output = super::dispatch::render_update_outcome(&outcome);
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
    pub async fn memory_write(
        &self,
        Parameters(params): Parameters<MemoryWriteParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let entry = super::tools::MemoryWriteBatchEntry {
            id: params.id,
            title: params.title,
            content: params.content,
            source_file: params.source_file,
            entry_type: params.entry_type,
            tags: params.tags,
            source_type: params.source_type,
            ttl: params.ttl,
            due_in: params.due_in,
            relates: params.relates,
            agent: params.agent,
            on_conflict: params.on_conflict,
        };
        let session = self.session_provenance();
        let output =
            super::dispatch::memory_write_impl(&handle, &entry, session.as_deref(), params.dry_run)
                .await?;

        let tokens = count_tokens(&output);
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
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let session = self.session_provenance();
        let (output, count) = super::dispatch::memory_write_batch_impl(
            &handle,
            &params.entries,
            session.as_deref(),
            params.dry_run,
        )
        .await?;

        let tokens = count_tokens(&output);
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
        let output =
            super::dispatch::memory_delete_impl(&handle, &params.id, params.dry_run).await?;

        let tokens = count_tokens(&output);
        self.record_persistent_call("memory_delete", tokens, 1, false)
            .await;
        tracing::debug!("mdkb_memory_delete: {}", output);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Record a Bayesian confirmation signal for a memory entry.
    #[tool(
        description = "Record outcome=\"confirmed\"|\"refuted\" against a memory entry. Atomic: increments or decrements confirmations (floor 0) and advances last_confirmed_at."
    )]
    pub async fn memory_confirm(
        &self,
        Parameters(params): Parameters<MemoryConfirmParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let output =
            super::dispatch::memory_confirm_impl(&handle, &params.id, &params.outcome).await?;

        let tokens = count_tokens(&output);
        self.record_persistent_call("memory_confirm", tokens, 1, false)
            .await;
        tracing::debug!("mdkb_memory_confirm: {}", output);

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// List memory entries with configurable sort order.
    #[tool(description = "List memory entries sorted by recency, popularity, or creation date.")]
    async fn memory_list(
        &self,
        Parameters(params): Parameters<MemoryListParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let (output, count) =
            super::dispatch::memory_list_impl(&handle, params.limit, &params.sort).await?;

        let tokens = count_tokens(&output);
        self.metrics.record_search(tokens, count);
        self.record_persistent_call("memory_list", tokens, count, false)
            .await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    // -----------------------------------------------------------------------
    // Code intelligence tools
    // -----------------------------------------------------------------------

    /// Query the code call graph: outgoing calls, incoming callers, or impact radius.
    #[tool(
        description = "Query code call graph. Resolves fuzzy/partial names. Directions: calls (default), callers, impact."
    )]
    async fn code_graph(
        &self,
        Parameters(params): Parameters<CodeGraphParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let output = super::dispatch::code_graph_impl(&handle, &params).await?;
        let tokens = count_tokens(&output);
        self.record_persistent_call("code_graph", tokens, 1, false)
            .await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Query the knowledge graph: links, backlinks, neighbors, or shortest path.
    #[tool(
        description = "Query the knowledge graph (frontmatter + wikilink edges). Directions: links (default, outgoing), backlinks (incoming), neighbors (adjacent), path (shortest path to `to`)."
    )]
    async fn graph(
        &self,
        Parameters(params): Parameters<GraphParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let output = super::dispatch::graph_impl(&handle, &params).await?;
        let tokens = count_tokens(&output);
        self.record_persistent_call("graph", tokens, 1, false).await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }

    /// Audit token economy: session/lifetime token counts, per-tool usage, top-5 most-called.
    #[tool(
        description = "Audit token economy: session tokens, per-tool counts, top-5 most-called, lifetime totals."
    )]
    pub async fn usage(
        &self,
        Parameters(params): Parameters<UsageParams>,
    ) -> Result<CallToolResult, McpError> {
        let handle = self.resolve_handle(params.root.as_deref()).await?;
        let session_id = self.session_id.load(Ordering::Relaxed);
        let output = super::dispatch::usage_impl(&handle, &params, session_id).await?;
        let tokens = count_tokens(&output);
        self.record_persistent_call("usage", tokens, 1, false).await;

        Ok(CallToolResult::success(vec![Content::text(output)]))
    }
}

/// Format a code symbol for MCP output.
pub(super) fn format_symbol(sym: &crate::code::symbol::Symbol) -> String {
    format_symbol_with_file_tokens(sym, None)
}

/// Format a code symbol, optionally annotating the containing file's token estimate.
pub(super) fn format_symbol_with_file_tokens(
    sym: &crate::code::symbol::Symbol,
    file_tokens: Option<u32>,
) -> String {
    let suffix = match file_tokens {
        Some(n) => format!(" (file: ~{}tok)", n),
        None => String::new(),
    };
    let mut out = format!(
        "  sym#{} {:?} {} in {}:{}{}\n",
        sym.id.value(),
        sym.kind,
        sym.name,
        sym.file_path,
        sym.range.start_line,
        suffix,
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

pub(super) fn resolve_document(
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
            // The surface map always ships; warmup content, when present,
            // follows it. An agent holding an MCP tool name otherwise has no
            // way to reach the CLI spelling of the same capability without
            // leaving MCP (story 024-0c7e).
            instructions: Some(match &self.warmup_instructions {
                Some(warmup) => format!("{}\n{warmup}", surface_instructions()),
                None => surface_instructions(),
            }),
        }
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Some(registry) = &self.registry {
            // Detach so the server's notification loop is never blocked by a
            // slow / non-responsive client. Critical for transports where the
            // peer's `roots/list` reply is not guaranteed (tests, hooks).
            let registry = Arc::clone(registry);
            let peer = context.peer.clone();
            tokio::spawn(async move {
                Self::sync_roots_from_peer(&peer, &registry).await;
            });
        }
    }

    async fn on_roots_list_changed(&self, context: NotificationContext<RoleServer>) {
        if let Some(registry) = &self.registry {
            let registry = Arc::clone(registry);
            let peer = context.peer.clone();
            tokio::spawn(async move {
                Self::sync_roots_from_peer(&peer, &registry).await;
            });
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
            // Initialize context and take it for reindex in one lock acquisition.
            // This prevents a race where tool calls see ctx=Some before the
            // doc_reindex_active flag is set.
            let taken_ctx = match startup_server.init_and_take_for_reindex().await {
                Ok(ctx) => ctx,
                Err(e) => {
                    tracing::error!("Startup auto-init failed: {:?}", e);
                    return;
                }
            };

            // Doc reindex phase. init_and_take_for_reindex already set
            // doc_reindex_active; the RAII guard clears it on ANY exit (including
            // a panic in handle_update/session_index), scoped so it clears before
            // the code phase. catch_unwind restores ctx to the mutex even on panic
            // — a lost ctx would wedge every tool call (ARCH-A1).
            {
                let _doc_guard = super::dispatch::ActiveFlagGuard::from_armed(
                    startup_server.doc_reindex_active.clone(),
                );
                if let Some(ctx) = taken_ctx {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _writer_guard = match crate::store::mutation_lock::acquire_writer(
                            &ctx.db_path,
                            "startup indexing",
                        ) {
                            Ok(guard) => guard,
                            Err(error) => {
                                tracing::error!("Startup writer admission failed: {error}");
                                return false;
                            }
                        };
                        crate::store::heal::invalidate_marker(&ctx.db_path);
                        let mut corruption_observed = false;
                        match handle_update(&ctx, &startup_root) {
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
                            Err(e) => {
                                corruption_observed |= e.is_index_corrupt();
                                tracing::error!("Startup doc reindex failed: {}", e);
                            }
                        }

                        let sessions_base =
                            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                                .join(".claude/projects");
                        let project_root = startup_root.to_string_lossy().to_string();
                        match crate::core::sessions::handle_session_index(
                            &ctx,
                            &sessions_base,
                            &project_root,
                        ) {
                            Ok(sr) if sr.added > 0 || sr.updated > 0 => {
                                tracing::info!(
                                    "Startup session index: {} added, {} updated",
                                    sr.added,
                                    sr.updated
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                corruption_observed |= e.is_index_corrupt();
                                tracing::warn!("Session indexing failed: {}", e);
                            }
                        }
                        if let Err(error) =
                            crate::store::heal::verify_and_mark_throttled(&ctx.db_path)
                        {
                            corruption_observed = true;
                            tracing::error!(
                                "Startup index verification failed; closing context for automatic recovery: {error}"
                            );
                        }
                        corruption_observed
                    }));
                    let restore_context = match outcome {
                        Ok(false) => true,
                        Ok(true) => {
                            tracing::error!(
                                "Startup indexing observed corruption; leaving context closed for automatic recovery"
                            );
                            false
                        }
                        Err(_) => {
                            tracing::error!("Startup doc reindex panicked; verifying context");
                            let verification = crate::store::mutation_lock::acquire_writer(
                                &ctx.db_path,
                                "startup panic verification",
                            )
                            .and_then(|_guard| {
                                crate::store::heal::verify_and_mark_throttled(&ctx.db_path)
                            });
                            match verification {
                                Ok(()) => true,
                                Err(error) => {
                                    tracing::error!(
                                        "Startup panic verification failed; leaving context closed for automatic recovery: {error}"
                                    );
                                    false
                                }
                            }
                        }
                    };

                    if restore_context {
                        let mut ctx_guard = startup_server.ctx.lock().await;
                        *ctx_guard = Some(ctx);
                    }
                }
                // _doc_guard drops here → doc_reindex_active cleared.
            }

            // Run code reindex (incremental if index already exists).
            // Respects code.enabled — skip entirely when the user disabled code intelligence.
            if !startup_server.full_config.code.enabled {
                return;
            }
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

            // Guard clears code_reindex_active on ANY exit (incl. panic).
            let _code_guard = super::dispatch::ActiveFlagGuard::from_armed(
                startup_server.code_reindex_active.clone(),
            );
            if let Some(mut facade) = taken_facade {
                // Content-hash incremental refresh (full build only on empty index).
                // catch_unwind so the facade is always restored, never left None.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    facade.update(&startup_root)
                }));
                crate::llm::release_cached_service();
                match outcome {
                    Ok(Ok(stats)) => {
                        if stats.symbols_indexed > 0 || stats.files_indexed > 0 {
                            tracing::info!(
                                "Startup index: {} files, {} symbols",
                                stats.files_indexed,
                                stats.symbols_indexed
                            );
                        }
                    }
                    Ok(Err(e)) => tracing::error!("Startup code reindex failed: {}", e),
                    Err(_) => {
                        tracing::error!("Startup code reindex panicked; restoring index handle");
                    }
                }

                // Put the facade back (also on the panic path).
                let mut idx_guard = startup_server.code_index.lock().await;
                *idx_guard = Some(facade);
            }
            // _code_guard drops here → code_reindex_active cleared.
        });
    }

    // NOTE: standalone mode does not spawn a file watcher. The watcher lives
    // only inside the daemon (`McpServer::global` / `RepoRegistry`), so N
    // client sessions share exactly one notify backend and one reindex stream.
    // Story 017-91cb / plan AC: "File watcher runs only in daemon mode".

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

/// Which sinks a changed path belongs to. A path can belong to several.
#[derive(Debug, Clone, Copy, Default)]
struct ChangeRoutes {
    code: bool,
    doc: bool,
    /// The git-tracked memory entry projection, `.mdkb/memory/entries/*.md`.
    memory: bool,
}

impl ChangeRoutes {
    fn any(self) -> bool {
        self.code || self.doc || self.memory
    }
}

fn classify_change(
    path: &Path,
    collection_paths: &[PathBuf],
    code_excludes: &globset::GlobSet,
    memory_entries_dir: &Path,
) -> ChangeRoutes {
    use crate::code::parsing::language::Language;
    ChangeRoutes {
        code: Language::from_path(path).is_some() && !code_excludes.is_match(path),
        doc: collection_paths.iter().any(|cp| path.starts_with(cp)),
        memory: path.starts_with(memory_entries_dir)
            && path.extension().and_then(|e| e.to_str()) == Some("md"),
    }
}

/// How long the watcher waits for context initialization before giving up.
const CTX_WAIT_SECS: u64 = 60;

/// Number of times `run_file_watcher` has been entered across the process.
/// Observable by tests to assert that the standalone path never spawns a
/// watcher and that the daemon spawns exactly one per registered repo.
pub static WATCHER_SPAWN_COUNT: AtomicU64 = AtomicU64::new(0);

/// Number of completed doc reindex flushes (observable by integration tests).
pub static DOC_REINDEX_COUNT: AtomicU64 = AtomicU64::new(0);

/// Number of completed code reindex flushes (observable by integration tests).
pub static CODE_REINDEX_COUNT: AtomicU64 = AtomicU64::new(0);

/// Run the file watcher and trigger reindex on changes.
///
/// Spawned only from `RepoRegistry::get_or_open` when a new `RepoHandle` is
/// created (daemon mode). Never spawned in standalone MCP stdio sessions.
#[allow(clippy::too_many_arguments)]
pub async fn run_file_watcher(
    root: PathBuf,
    ctx: Arc<Mutex<Option<Context>>>,
    code_index: Arc<Mutex<Option<IndexFacade>>>,
    code_enabled: bool,
    code_ignore_patterns: Vec<String>,
    respect_gitignore: bool,
    debounce_ms: u64,
    batch_idle_ms: u64,
    reindex_rx: Option<tokio::sync::mpsc::Receiver<PathBuf>>,
) -> crate::error::Result<()> {
    run_file_watcher_inner(
        root,
        ctx,
        code_index,
        code_enabled,
        code_ignore_patterns,
        respect_gitignore,
        debounce_ms,
        batch_idle_ms,
        None,
        reindex_rx,
    )
    .await
}

/// Configurable debounce + batch idle + optional ready signal for tests.
#[allow(clippy::too_many_arguments)]
pub async fn run_file_watcher_inner(
    root: PathBuf,
    ctx: Arc<Mutex<Option<Context>>>,
    code_index: Arc<Mutex<Option<IndexFacade>>>,
    code_enabled: bool,
    code_ignore_patterns: Vec<String>,
    respect_gitignore: bool,
    debounce_ms: u64,
    batch_idle_ms: u64,
    ready: Option<Arc<tokio::sync::Notify>>,
    mut reindex_rx: Option<tokio::sync::mpsc::Receiver<PathBuf>>,
) -> crate::error::Result<()> {
    WATCHER_SPAWN_COUNT.fetch_add(1, Ordering::Relaxed);
    let mut watcher = FileWatcher::new(WatcherConfig { debounce_ms })?;

    // Wait for context initialization (driven by first client request via
    // ensure_handle_context). This MUST NOT exit on a timeout: returning here
    // drops `reindex_rx`, after which every post_tool_use path injection fails
    // with "channel closed" forever (the watcher never respawns). The task is
    // aborted when the handle is dropped (RepoHandle::Drop), so waiting
    // indefinitely is bounded by handle lifetime and cannot leak. We only warn
    // once, actionably, if ctx is unusually slow to appear.
    let collection_list = {
        let warn_after =
            tokio::time::Instant::now() + std::time::Duration::from_secs(CTX_WAIT_SECS);
        let mut warned = false;
        loop {
            let mut ctx_guard = ctx.lock().await;
            if ctx_guard.is_some() {
                let outcome = crate::core::run_guarded_read(
                    &mut ctx_guard,
                    "watcher collection discovery",
                    |ctx_ref| collections::list_collections(&ctx_ref.conn),
                );
                match outcome {
                    Some(Ok(collections)) => break collections,
                    Some(Err(error)) => return Err(error),
                    None => {}
                }
            }
            drop(ctx_guard);
            if !warned && tokio::time::Instant::now() >= warn_after {
                warned = true;
                tracing::warn!(
                    "Watcher: context still uninitialized after {CTX_WAIT_SECS}s; \
                     continuing to wait. Reindex is on hold until the first client \
                     request initializes the repo context."
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };

    // Resolve absolute collection paths for routing
    let collection_paths: Vec<PathBuf> = collection_list
        .iter()
        .map(|coll| root.join(&coll.path))
        .filter(|p| p.exists())
        .collect();
    // Build exclude matcher for code changes (node_modules, dist, target, etc.)
    let code_excludes = build_code_excludes(&root, &code_ignore_patterns);

    // The memory projection this watcher reconciles, `.mdkb/memory/entries/`.
    let memory_entries_dir = root.join(".mdkb/memory/entries");

    // Watch root recursively — it covers code, collections inside root, AND the
    // memory entry projection. This registration must NOT be gated on any one
    // sink: it used to sit behind `if code_enabled`, so turning code indexing off
    // left the daemon watching nothing at all while every log line still said it
    // was running. Whether a given change is acted on is a per-sink routing
    // decision, which is where `code_enabled` belongs (see `classify_change` and
    // the flush calls below).
    if let Err(e) = watcher.watch(&root.to_path_buf()) {
        tracing::warn!("Failed to watch root: {}", e);
    } else {
        tracing::info!("Watching root for changes");
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

    // Bootstrap code index if empty (mirrors standalone startup task behavior).
    // Runs once before entering the event loop — holds the lock briefly to avoid
    // racing with acquire_code_index(), then releases before the (slow) reindex.
    if code_enabled {
        let needs_bootstrap = {
            let idx_guard = code_index.lock().await;
            match idx_guard.as_ref() {
                Some(f) => f.file_count() == 0,
                None => true,
            }
        };
        if needs_bootstrap {
            let mut idx_guard = code_index.lock().await;
            if idx_guard.is_none() {
                let index_path = root.join(".mdkb/code.sqlite");
                match IndexFacade::open_or_create(&index_path) {
                    Ok(facade) => {
                        let pipeline_config = crate::code::indexing::pipeline::PipelineConfig {
                            ignore_patterns: code_ignore_patterns.clone(),
                            respect_gitignore,
                            ..Default::default()
                        };
                        *idx_guard = Some(facade.with_config(pipeline_config));
                    }
                    Err(e) => {
                        tracing::error!("Watcher bootstrap: failed to open code index: {e}");
                    }
                }
            }
            if let Some(facade) = idx_guard.as_mut() {
                if facade.file_count() == 0 {
                    match facade.index_directory(&root) {
                        Ok(stats) if stats.symbols_indexed > 0 || stats.files_indexed > 0 => {
                            tracing::info!(
                                "Watcher startup: indexed {} files, {} symbols",
                                stats.files_indexed,
                                stats.symbols_indexed,
                            );
                        }
                        Ok(_) => {}
                        Err(e) => tracing::error!("Watcher startup reindex failed: {e}"),
                    }
                    crate::llm::release_cached_service();
                }
            }
        }
    }

    if let Some(notify) = ready {
        notify.notify_one();
    }

    // Process file changes with batching for code reindex.
    // Two event sources: FSEvents watcher and injected paths from post-tool-use IPC.
    let mut code_batch: Vec<PathBuf> = Vec::new();
    let mut needs_doc_update = false;
    let mut needs_memory_sync = false;

    loop {
        // Helper: receive from the optional injected-path channel, or block forever if absent.
        macro_rules! recv_injected {
            () => {
                async {
                    match reindex_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<PathBuf>>().await,
                    }
                }
            };
        }

        if code_batch.is_empty() && !needs_doc_update && !needs_memory_sync {
            // No pending work — block until next event from either source.
            tokio::select! {
                change = watcher.recv() => {
                    let Some(change) = change else {
                        tracing::error!(
                            "Watcher: FSEvents stream closed; file-change watching stopped \
                             for this repo (restart the daemon to restore it)."
                        );
                        break;
                    };
                    tracing::debug!("File change detected: {:?}", change.path);
                    let routes = classify_change(&change.path, &collection_paths, &code_excludes, &memory_entries_dir);
                    if routes.code { code_batch.push(change.path.clone()); }
                    if routes.doc { needs_doc_update = true; }
                    if routes.memory { needs_memory_sync = true; }
                    if !routes.any() {
                        tracing::debug!("Ignoring unrouted change: {:?}", change.path);
                    }
                }
                path = recv_injected!() => {
                    if let Some(p) = path {
                        // Directory = post-heal full-rebuild signal; file = code reindex.
                        if p.is_dir() {
                            full_rebuild_from_heal(&ctx, &code_index, &root).await;
                        } else {
                            code_batch.push(p);
                        }
                    }
                }
            }
        } else {
            // Pending batch — accumulate more events or flush after idle timeout.
            tokio::select! {
                change = watcher.recv() => {
                    if let Some(change) = change {
                        tracing::debug!("File change detected: {:?}", change.path);
                        let routes = classify_change(&change.path, &collection_paths, &code_excludes, &memory_entries_dir);
                        if routes.code { code_batch.push(change.path.clone()); }
                        if routes.doc { needs_doc_update = true; }
                        if routes.memory { needs_memory_sync = true; }
                        if !routes.any() {
                            tracing::debug!("Ignoring unrouted change: {:?}", change.path);
                        }
                    } else {
                        tracing::error!(
                            "Watcher: FSEvents stream closed; file-change watching stopped \
                             for this repo (restart the daemon to restore it)."
                        );
                        break;
                    }
                }
                path = recv_injected!() => {
                    if let Some(p) = path {
                        // A directory is the post-heal full-rebuild signal (the repo
                        // root); a file is a targeted code reindex.
                        if p.is_dir() {
                            full_rebuild_from_heal(&ctx, &code_index, &root).await;
                        } else {
                            code_batch.push(p);
                        }
                    }
                }
                () = tokio::time::sleep(std::time::Duration::from_millis(batch_idle_ms)) => {
                    if watcher.take_missed_events() {
                        // The watcher dropped events under backpressure; the batch
                        // is an incomplete view, so rescan everything instead.
                        tracing::warn!(
                            "Watcher: recovering dropped change events with a full rescan"
                        );
                        code_batch.clear();
                        full_code_rescan(&code_index, &root).await;
                        needs_doc_update = true;
                        needs_memory_sync = true;
                    } else {
                        flush_code_batch(&code_index, &root, &mut code_batch).await;
                    }
                    // Order matters: `handle_update` already runs the memory
                    // reconciliation, so a pending doc update subsumes a pending
                    // memory sync. Flushing docs first lets the memory flush see
                    // the flag cleared and skip a redundant second pass.
                    flush_doc_update(&ctx, &root, &mut needs_doc_update, &mut needs_memory_sync).await;
                    flush_memory_sync(&ctx, &mut needs_memory_sync).await;
                }
            }
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
    let outcome =
        crate::code::indexing::run_code_mutation(&mut idx_guard, "code reindex", |facade| {
            facade.reindex_files(root, &paths)
        });
    if let Some(result) = outcome {
        match result {
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
        CODE_REINDEX_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Full code rescan (content-hash diff over all files), used to recover after
/// the watcher dropped change events under backpressure — the specific dropped
/// paths are unknown, so `update` re-checks everything.
async fn full_code_rescan(code_index: &Arc<Mutex<Option<IndexFacade>>>, root: &Path) {
    let mut idx_guard = code_index.lock().await;
    let outcome = crate::code::indexing::run_code_mutation(
        &mut idx_guard,
        "watcher recovery rescan",
        |facade| facade.update(root),
    );
    if let Some(result) = outcome {
        match result {
            Ok(stats) => tracing::info!(
                "Watcher recovery rescan: {} files, {} symbols",
                stats.files_indexed,
                stats.symbols_indexed
            ),
            Err(e) => tracing::error!("Watcher recovery rescan failed: {}", e),
        }
        crate::llm::release_cached_service();
        CODE_REINDEX_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Full rebuild after an autoheal quarantine rebuilt an empty index.
///
/// The injected signal is the repo root (a directory), distinct from the file
/// paths post_tool_use injects. A quarantine wipes documents AND sessions from
/// the index (they live only there), so both must be re-derived from source —
/// not just code. Best-effort; every phase logs and continues on failure.
async fn full_rebuild_from_heal(
    ctx: &Arc<Mutex<Option<Context>>>,
    code_index: &Arc<Mutex<Option<IndexFacade>>>,
    root: &Path,
) {
    tracing::warn!("post-heal: rebuilding docs + sessions + code from source");
    // A quarantine rebuilds `memory_entries` empty, so the projection on disk is
    // the only surviving copy — the doc update's reconciliation pass is what
    // re-imports it. Both flags are set for that reason.
    let mut needs_docs = true;
    let mut needs_memory = true;
    flush_doc_update(ctx, root, &mut needs_docs, &mut needs_memory).await;
    flush_memory_sync(ctx, &mut needs_memory).await;
    full_code_rescan(code_index, root).await;

    match crate::daemon::config::home_dir() {
        Err(e) => tracing::warn!("post-heal session reindex skipped: {e}"),
        Ok(home) => {
            let sessions_base = home.join(".claude/projects");
            let project_root = root.to_string_lossy().to_string();
            let ctx = Arc::clone(ctx);
            let indexed = tokio::task::spawn_blocking(move || {
                let mut guard = ctx.blocking_lock();
                crate::core::run_mutation(&mut guard, "post-heal session index", |c| {
                    crate::core::sessions::handle_session_index(c, &sessions_base, &project_root)
                })
            })
            .await;
            if let Ok(Some(Err(e))) = indexed {
                tracing::warn!("post-heal session reindex failed: {e}");
            }
        }
    }
}

/// Reconcile the memory entry projection after a change under
/// `.mdkb/memory/entries/` — typically a `git pull` landing a colleague's entry,
/// or a hand edit.
///
/// The watcher delivers per-file events, but the bulk-loss circuit breaker and
/// the git intentional/suspect discriminator are **set-level** decisions: they
/// need to see every vanished file at once to tell a committed bulk deletion
/// from a broken checkout. So an event is only ever a *trigger* — the debounced
/// flush runs the same whole-directory pass as `mdkb update`, and the changed
/// path is deliberately not passed in. A per-file archive decision would make
/// twelve deletions twelve independent choices, each below the cap, and the
/// breaker would never fire.
///
/// Reconciliation writes into the directory it watches, which re-triggers this
/// flush once. That pass finds every recorded hash already matching, writes
/// nothing, and the loop closes.
async fn flush_memory_sync(ctx: &Arc<Mutex<Option<Context>>>, needs_sync: &mut bool) {
    if !*needs_sync {
        return;
    }
    *needs_sync = false;
    // Synchronous SQLite + filesystem work, like `handle_update`: run it on a
    // blocking thread so it never stalls a tokio worker (PERF-1).
    let ctx = Arc::clone(ctx);
    let outcome = tokio::task::spawn_blocking(move || {
        let mut guard = ctx.blocking_lock();
        crate::core::run_mutation(&mut guard, "memory sync", |ctx_ref| {
            crate::core::memory_sync::sync_memory_files(ctx_ref)
        })
    })
    .await;
    match outcome {
        Ok(Some(Ok(s))) => {
            if s.imported > 0 || s.adopted > 0 || s.conflicts > 0 || s.revived > 0 {
                tracing::info!(
                    "Memory reconciled: {} imported, {} adopted, {} conflicts, {} revived",
                    s.imported,
                    s.adopted,
                    s.conflicts,
                    s.revived
                );
            }
            if s.conflicts > 0 {
                tracing::warn!(
                    "Memory sync resolved {} conflict(s) by newest edit; the superseded \
                     versions are kept — inspect with `mdkb memory history <id>`.",
                    s.conflicts
                );
            }
            if s.quarantined > 0 {
                tracing::warn!(
                    "Memory sync skipped {} unreadable file(s) (merge markers, bad \
                     frontmatter, or id/filename mismatch).",
                    s.quarantined
                );
            }
        }
        Ok(Some(Err(e))) => tracing::error!("Memory sync failed: {}", e),
        Ok(None) => {} // ctx not initialized — nothing to do
        Err(e) => tracing::error!("Memory sync task panicked: {}", e),
    }
}

/// Flush pending document update.
///
/// Clears `needs_memory_sync` too: `handle_update` runs the memory
/// reconciliation itself, so letting it stand would cost a redundant second
/// whole-directory pass on every doc change.
async fn flush_doc_update(
    ctx: &Arc<Mutex<Option<Context>>>,
    root: &Path,
    needs_update: &mut bool,
    needs_memory_sync: &mut bool,
) {
    if !*needs_update {
        return;
    }
    *needs_update = false;
    *needs_memory_sync = false;
    // `handle_update` is fully synchronous (SQLite writes, filesystem walks, and —
    // via auto-embed/memory-backfill — ONNX inference). Run it on a blocking thread
    // and take the lock there (`blocking_lock`), so it never blocks the tokio worker
    // or holds an async guard across seconds of work (PERF-1).
    let ctx = Arc::clone(ctx);
    let root = root.to_path_buf();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut guard = ctx.blocking_lock();
        crate::core::run_mutation(&mut guard, "doc reindex", |ctx_ref| {
            handle_update(ctx_ref, &root)
        })
    })
    .await;
    match outcome {
        Ok(Some(Ok(result))) => {
            DOC_REINDEX_COUNT.fetch_add(1, Ordering::Relaxed);
            if result.added > 0 || result.updated > 0 || result.removed > 0 {
                tracing::info!(
                    "Reindexed: {} added, {} updated, {} removed",
                    result.added,
                    result.updated,
                    result.removed
                );
            }
        }
        Ok(Some(Err(e))) => tracing::error!("Doc reindex failed: {}", e),
        Ok(None) => {} // ctx not initialized — nothing to do
        Err(e) => tracing::error!("Doc reindex task panicked: {}", e),
    }
}

/// Base instructions explaining what mdkb is and how to use it.
///
/// These are always included in server instructions, regardless of whether
/// memory entries exist. They tell the LLM what mdkb does and how to interact.
const BASE_INSTRUCTIONS: &str = "\
# mdkb — Project Knowledge Base

mdkb is a **semantic** search engine (fuzzy, concept-based). It does NOT match literal strings or regex — for those use Grep, not mdkb.

## Code

- `code_graph(name)` — callers, callees, or impact radius in one call (`direction=\"callers\"|\"callees\"|\"impact\"`). Replaces multi-file Grep for \"who calls X\".
- `search(query, scope=\"symbols\")` — find a symbol by name. `scope=\"code\"` — semantic code query.
- Architecture/decisions: `search(query)` → `get(id)`.
- Literal string or regex → Grep.

## Memory

- `search(query, scope=\"memory\")` — check before writing duplicates.
- `memory_write` / `memory_write_batch` — persist after solving problems.
- `memory_confirm(id, outcome=\"confirmed\"|\"refuted\")` — adjust belief (+/-1, floor 0) instead of rewriting.
- `memory_delete` — remove stale entries.

`search` returns IDs → `get(id)` for full content. Use `root=\"*\"` only for cross-repo search. Then call `get` with the exact `repo` shown on the selected result; `get` does not accept `root=\"*\"`.

### Reminders

Create: `memory_write(id, title, content, entry_type=\"reminder\", due_in=<seconds>)`.
A due reminder appears as `[reminder:DUE] {id}: {title}` once `due_in` elapses.

When you see one:
1. Ask if done. End your turn — do NOT delete yet.
2. Delete only on unambiguous affirmative. Ambiguous = re-ask.
3. Confirmed → `memory_delete(id)`. \"not found\" = already removed.
4. Snooze → `get(id)`, then `memory_write(id, title, <content>, entry_type=\"reminder\", due_in=<new_seconds>)`.
";

/// Select the base instructions variant based on `MDKB_INSTRUCTIONS_VARIANT` env var.
///
/// Currently only `default` exists. Unknown variants fall back to default with a warning.
/// Add new variants here for A/B testing, then validate with `e2e_search_behaviour` tests.
fn select_base_instructions() -> &'static str {
    static VARIANT: std::sync::OnceLock<&str> = std::sync::OnceLock::new();
    VARIANT.get_or_init(
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
        .and_then(|ctx| memory::get_warmup_index(&ctx.conn, limit).ok())
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
pub(super) fn relative_time_ago(unix_ts: i64) -> String {
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
pub(super) fn truncate_text(text: &str, max_len: usize) -> String {
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
pub(super) fn ood_hint(result_count: usize, top_score: Option<f64>) -> Option<&'static str> {
    if result_count == 0 {
        return Some(
            "\n> No results. mdkb is semantic search — it won't match literal strings. \
             Use Grep for exact string/regex matching in source files.",
        );
    }
    if top_score.is_some_and(|s| s < OOD_SCORE_THRESHOLD) {
        return Some(
            "\n> Low-confidence results. If searching for a literal string or pattern, \
             use Grep instead — mdkb only does semantic/fuzzy matching.",
        );
    }
    None
}

pub(super) fn format_search_results(results: &[SearchResult], limit: usize) -> String {
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
    let retrieval_ids: Vec<_> = ordered
        .iter()
        .map(|r| {
            if r.collection == "memory" && !r.path.is_empty() {
                r.path.clone()
            } else {
                r.id.to_string()
            }
        })
        .collect();
    let repo_roots: Vec<_> = ordered
        .iter()
        .filter_map(|r| r.repo_root.as_deref())
        .collect();

    if let Some(root) = repo_roots.first() {
        let id = serde_json::to_string(&retrieval_ids[0]).expect("string serialization");
        let root = serde_json::to_string(root).expect("string serialization");
        output.push_str(&format!("\nUse get({id}, root={root}) to read one."));
        if retrieval_ids.len() > 1 {
            output.push_str(" For another result, pass its listed repo as root.");
        }
        output.push_str(" root=\"*\" is search-only.");
    } else if retrieval_ids.len() == 1 {
        output.push_str(&format!("\nUse get(\"{}\") to read.", retrieval_ids[0]));
    } else {
        output.push_str(&format!(
            "\nUse get(\"{}\") to read one, or get(\"{}\") for all.",
            retrieval_ids[0],
            retrieval_ids.join(",")
        ));
    }

    output
}

/// Format memory search results for output.
pub(super) fn format_memory_search_results(entries: &[memory::MemoryEntry]) -> String {
    use crate::store::hybrid::lost_in_middle_reorder;

    if entries.is_empty() {
        return "No matching memory entries found.".to_string();
    }

    // Apply lost-in-the-middle reordering
    let mut ordered: Vec<_> = entries.iter().collect();
    lost_in_middle_reorder(&mut ordered);

    let mut out = format!("Found {} memory entries:\n\n", entries.len());
    for entry in &ordered {
        let ttl_info = format_ttl_info(entry.expires_at);
        let confirmed_info = format_confirmed_info(entry.last_confirmed_at);
        out.push_str(&format!(
            "- [{}] {} ({}, conf:{:.2}, confirms:{}, access:{}{}, {}{}): {}\n",
            entry.id,
            entry.title,
            entry.entry_type,
            entry.confidence(),
            entry.confirmations,
            entry.access_count,
            confirmed_info,
            relative_time_ago(entry.updated_at),
            ttl_info,
            truncate_text(&entry.content, 100)
        ));
    }
    out
}

/// Format last_confirmed_at for display. Returns empty string when never confirmed.
fn format_confirmed_info(last_confirmed_at: Option<i64>) -> String {
    match last_confirmed_at {
        Some(ts) => format!(", confirmed:{}", relative_time_ago(ts)),
        None => String::new(),
    }
}

/// Drop memory entries whose confidence() falls below `min`.
/// Omitted filter or `0.0` is a no-op (current behavior).
pub(super) fn apply_min_confidence(
    entries: Vec<memory::MemoryEntry>,
    min: Option<f64>,
) -> Vec<memory::MemoryEntry> {
    match min {
        Some(threshold) if threshold > 0.0 => entries
            .into_iter()
            .filter(|e| e.confidence() >= threshold)
            .collect(),
        _ => entries,
    }
}

/// Format TTL info for display. Returns empty string for permanent entries.
pub(super) fn format_ttl_info(expires_at: Option<i64>) -> String {
    match expires_at {
        Some(ts) => {
            let now = chrono::Utc::now().timestamp();
            if ts <= now {
                ", EXPIRED".to_string()
            } else {
                let dt = chrono::DateTime::from_timestamp(ts, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| ts.to_string());
                format!(", expires:{dt}")
            }
        }
        None => String::new(),
    }
}

/// Apply line range to content.
pub(super) fn apply_line_range(content: &str, range: &str) -> Result<String, McpError> {
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

/// The tool names this server advertises to a client.
///
/// Read from the generated tool router rather than a list maintained by hand,
/// so `tests/surface_parity.rs` compares the MCP surface against the CLI one
/// using what the server actually publishes. A hand-written list would drift
/// from the router and the parity check would then be asserting agreement
/// between two pieces of prose (story 024-0c7e).
pub fn advertised_tool_names() -> Vec<String> {
    McpServer::tool_router()
        .list_all()
        .into_iter()
        .map(|t| t.name.to_string())
        .collect()
}

/// The MCP-tool-to-CLI-command map, rendered for the server instructions.
///
/// An agent holding an MCP tool name has no way to discover the CLI spelling
/// without leaving MCP — `memory_write` and `mdkb memory add` are the same
/// capability under two names, and nothing said so (story 024-0c7e). Included in
/// the instructions because that is the one place an MCP client always reads.
///
/// Deliberately compact: only the pairs, and only the notes that state a real
/// difference. Every token here is charged on every turn of every conversation,
/// so a pair whose two names already imply each other earns none.
pub fn surface_instructions() -> String {
    let mut out = String::from("Equivalent CLI commands (same capability, different name):\n");
    for e in crate::core::surface::SURFACE_MAP {
        let Some(cli) = e.cli_command else { continue };
        out.push_str(&format!("  {} = mdkb {}\n", e.mcp_tool, cli));
    }
    out.push_str("Full inventory with the differences that matter: `mdkb surface`.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Daemon config whitelisting the system temp dir, so global-mode tests that
    /// open repos under a `TempDir` (outside home) pass the default-deny
    /// whitelist (SEC-3, story 070). These tests exercise global routing, not the
    /// whitelist — which has dedicated tests in `daemon::config`.
    fn global_test_config() -> crate::daemon::config::DaemonConfig {
        crate::daemon::config::DaemonConfig {
            whitelist_dirs: vec![std::env::temp_dir().to_string_lossy().to_string()],
            ..crate::daemon::config::DaemonConfig::default()
        }
    }

    // --- OOD detection tests ---

    #[test]
    fn test_ood_hint_zero_results() {
        let hint = ood_hint(0, None);
        assert!(hint.is_some());
        assert!(hint.unwrap().contains("No results"));
    }

    #[test]
    fn disambiguation_error_truncates_multibyte_signature_without_panic() {
        use crate::code::symbol::Symbol;
        use crate::code::types::{FileId, Range, SymbolId, SymbolKind};

        // Signature whose byte 57 lands inside a multi-byte char: raw `&s[..57]`
        // slicing would panic here. Prefix pads past the 60-byte truncation
        // threshold so truncation actually engages on the multi-byte tail.
        let sig = format!("fn f(x: {}) -> ()", "パラメータ".repeat(8));
        assert!(
            !sig.is_char_boundary(57),
            "test setup: byte 57 must split a char"
        );

        let sym = Symbol::new(
            SymbolId::new(1).unwrap(),
            "f",
            SymbolKind::Function,
            FileId::new(1).unwrap(),
            Range::new(1, 0, 1, 0),
        )
        .with_signature(sig);

        // Must not panic; the truncated signature is char-boundary safe.
        let err = McpServer::disambiguation_error("f", std::slice::from_ref(&sym));
        assert!(err.message.contains("Multiple symbols match 'f'"));
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
    fn test_format_cross_repo_results_get_hint_uses_exact_root() {
        let results = vec![SearchResult {
            id: 10,
            collection: "docs".to_string(),
            path: "auth.md".to_string(),
            title: Some("Auth Guide".to_string()),
            score: 0.85,
            snippets: vec![],
            status: None,
            superseded_by: None,
            repo_root: Some("/repos/example".to_string()),
        }];

        let output = format_search_results(&results, 10);
        assert!(
            output.contains("get(\"10\", root=\"/repos/example\")"),
            "Should include the exact repo in the get hint, got: {output}"
        );
        assert!(
            output.contains("root=\"*\" is search-only"),
            "Should explain wildcard scope, got: {output}"
        );
    }

    #[test]
    fn test_format_cross_repo_memory_hint_uses_slug_and_exact_root() {
        let results = vec![SearchResult {
            id: 0,
            collection: "memory".to_string(),
            path: "parity-backlog-checkpoint-2026-07-23".to_string(),
            title: Some("Parity backlog checkpoint".to_string()),
            score: 1.0,
            snippets: vec![],
            status: None,
            superseded_by: None,
            repo_root: Some("/repos/example".to_string()),
        }];

        let output = format_search_results(&results, 10);
        assert!(
            output
                .contains("get(\"parity-backlog-checkpoint-2026-07-23\", root=\"/repos/example\")"),
            "Should use the memory slug and exact repo, got: {output}"
        );
        assert!(
            !output.contains("get(\"0\""),
            "Should not expose the synthetic aggregation ID, got: {output}"
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
        assert!(!result.contains("## Available Memories"));
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

    #[test]
    fn test_base_instructions_token_budget() {
        // Base instructions are sent on every MCP session — guard against prose bloat.
        let result = build_server_instructions(&[]);
        let tokens = count_tokens(&result);
        assert!(
            tokens < 600,
            "BASE_INSTRUCTIONS exceeds 600-token budget: {} tokens",
            tokens
        );
    }

    #[test]
    fn test_base_instructions_contains_reminder_protocol() {
        // Guard against accidental removal of the Reminders confirmation flow.
        // If someone deletes the Reminders section, this fails fast.
        let result = build_server_instructions(&[]);
        assert!(
            result.contains("[reminder:DUE]"),
            "Reminder format marker missing from instructions"
        );
        assert!(
            result.contains("### Reminders"),
            "Reminders section heading missing"
        );
        assert!(
            result.contains("memory_delete"),
            "Reminder deletion step missing"
        );
        assert!(
            result.contains("get(id)"),
            "Snooze must instruct to fetch content via get(id) before rewrite"
        );
        // Numbered confirmation-flow steps (1. through 4.)
        for step in ["1.", "2.", "3.", "4."] {
            assert!(
                result.contains(step),
                "Reminder flow step `{}` missing",
                step
            );
        }
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
                Err(error) => panic!(
                    "Tool call {} timed out after {:?} - likely deadlock: {error}",
                    i, timeout_duration,
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
        assert!(result.contains("# mdkb"), "Should explain what mdkb is");
        assert!(
            result.contains("search(query)"),
            "Should mention search tool"
        );
        assert!(
            result.contains("memory_write"),
            "Should mention memory tools"
        );
        // memory_list is discoverable via tool schema; no need to mention in instructions
    }

    #[test]
    fn test_build_server_instructions_includes_memory_when_present() {
        let index = vec!["auth-flow: OAuth2 implementation #auth".to_string()];
        let result = build_server_instructions(&index);

        // Should contain both base instructions and memory
        assert!(result.contains("# mdkb"), "Should have base instructions");
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
        let tools_to_test = ["status", "status", "status"];

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
                Ok(Ok(())) => {} // Success
                Ok(Err(e)) => panic!("Tool {} ({}) failed: {:?}", tool_name, i, e),
                Err(error) => panic!(
                    "Tool {} ({}) timed out - likely deadlock: {error}",
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
        server
            .memory_write(Parameters(MemoryWriteParams {
                id: "test-delete-me".to_string(),
                title: "Deletable entry".to_string(),
                content: "This will be deleted.".to_string(),
                source_file: None,
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                ttl: None,
                due_in: None,
                relates: vec![],
                agent: None,
                on_conflict: None,
                root: None,
                dry_run: false,
            }))
            .await
            .expect("Failed to write memory entry");

        // Delete it
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            server.memory_delete(Parameters(MemoryDeleteParams {
                id: "test-delete-me".to_string(),
                root: None,
                dry_run: false,
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
            Err(error) => panic!("memory_delete timed out - likely deadlock: {error}"),
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
                dry_run: false,
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
                source_file: None,
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                ttl: None,
                due_in: None,
                relates: vec![],
                agent: None,
                on_conflict: None,
                root: None,
                dry_run: false,
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
                dry_run: false,
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
                min_confidence: None,
                threshold: None,
                root: None,
            }))
            .await
            .expect("search should not error");

        let text = extract_text(&result);
        assert!(
            text.contains("No results"),
            "Should indicate no results with Grep hint (OOD), got: {}",
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
                min_confidence: None,
                threshold: None,
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
                dry_run: false,
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
                source_file: None,
                entry_type: "topic".to_string(),
                tags: vec!["tag1".to_string()],
                source_type: "user_statement".to_string(),
                ttl: None,
                due_in: None,
                relates: vec![],
                agent: None,
                on_conflict: None,
                root: None,
                dry_run: false,
            }))
            .await
            .expect("write A");

        server
            .memory_write(Parameters(MemoryWriteParams {
                id: "test-b".to_string(),
                title: "Test B".to_string(),
                content: "Content B".to_string(),
                source_file: None,
                entry_type: "problem".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                ttl: None,
                due_in: None,
                relates: vec![],
                agent: None,
                on_conflict: None,
                root: None,
                dry_run: false,
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

            // Create a JS file with top-level calls (CommonJS hook pattern)
            std::fs::write(
                src_dir.join("hook.js"),
                r#"
const { validate } = require('./lib/utils');

function processHook(data) {
    return validate(data);
}

if (require.main === module) {
    processHook('test');
    validate('direct');
}
"#,
            )
            .unwrap();

            // Create/reuse code index (init auto-bootstraps it)
            let index_path = root.join(".mdkb/code.sqlite");
            let mut facade =
                IndexFacade::open_or_create(&index_path).expect("Failed to open code index");
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
                    min_confidence: None,
                    threshold: None,
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
                    min_confidence: None,
                    threshold: None,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("search scope=symbols failed");

            let text = extract_text(&result);
            assert!(text.contains("0 matches"), "Got: {}", text);
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
                    min_confidence: None,
                    threshold: None,
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
                    threshold: None,
                    root: None,
                    min_confidence: None,
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
                    min_confidence: None,
                    threshold: None,
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
                    min_confidence: None,
                    threshold: None,
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
                    min_confidence: None,
                    threshold: None,
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
                    min_confidence: None,
                    threshold: None,
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
                    min_confidence: None,
                    threshold: None,
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
                    min_confidence: None,
                    threshold: None,
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

        #[tokio::test]
        async fn test_resolve_symbol_fuzzy_fallback() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // "process_data" exists as exact name. But "process" alone should
            // fuzzy-match and return a result (not an error) since there's only
            // one symbol containing "process".
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
            .expect("code_graph should succeed with exact name");

            let text = extract_text(&result);
            assert!(
                text.contains("process_data"),
                "Should resolve process_data: {}",
                text
            );
        }

        #[tokio::test]
        async fn test_resolve_symbol_fuzzy_partial_name() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // "validate" exists. Passing "validat" (partial) should fuzzy-resolve
            // to a single match and succeed.
            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "validat".to_string(),
                    direction: "callers".to_string(),
                    symbol_id: None,
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("code_graph should fuzzy-resolve 'validat' to 'validate'");

            let text = extract_text(&result);
            assert!(
                text.contains("validate"),
                "Should resolve to validate: {}",
                text
            );
        }

        #[tokio::test]
        async fn test_resolve_symbol_fuzzy_multiple_candidates() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // "new" and "transform" both exist in DataHelper.
            // Searching "data" should match multiple symbols (DataHelper, process_data)
            // and return a disambiguation list, not an error.
            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "data".to_string(),
                    direction: "calls".to_string(),
                    symbol_id: None,
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout");

            // Should be an error with disambiguation list
            assert!(
                result.is_err(),
                "Multiple fuzzy matches should require disambiguation"
            );
            let err_msg = format!("{}", result.unwrap_err());
            assert!(
                err_msg.contains("sym#"),
                "Error should list candidates with sym# IDs: {}",
                err_msg
            );
        }

        #[tokio::test]
        async fn test_code_graph_top_level_callers() {
            let (_dir, server) = setup_indexed_server();
            let timeout = Duration::from_secs(5);

            // "processHook" is called at top-level in hook.js via
            // `if (require.main === module) { processHook('test'); }`
            // The <module> synthetic symbol should appear as a caller.
            let result = tokio::time::timeout(
                timeout,
                server.code_graph(Parameters(CodeGraphParams {
                    name: "processHook".to_string(),
                    direction: "callers".to_string(),
                    symbol_id: None,
                    max_depth: 3,
                    root: None,
                })),
            )
            .await
            .expect("timeout")
            .expect("code_graph callers for processHook failed");

            let text = extract_text(&result);
            assert!(
                text.contains("<module>"),
                "Top-level caller should show as <module>: {}",
                text
            );
        }
    }

    /// `(code, doc)` for the pre-existing routing tests, which predate the
    /// memory route and assert nothing about it. The memory directory is set to
    /// a path none of them touch.
    fn code_doc(path: &Path, collections: &[PathBuf], excludes: &globset::GlobSet) -> (bool, bool) {
        let r = classify_change(
            path,
            collections,
            excludes,
            Path::new("/project/.mdkb/memory/entries"),
        );
        (r.code, r.doc)
    }

    #[test]
    fn memory_entry_files_route_to_reconciliation() {
        let entries = Path::new("/project/.mdkb/memory/entries");
        let collections: Vec<PathBuf> = vec![];
        let excludes = build_code_excludes(Path::new("/project"), &[]);

        let r = classify_change(
            &entries.join("auth-oauth2.md"),
            &collections,
            &excludes,
            entries,
        );
        assert!(
            r.memory,
            "a projected entry file must trigger reconciliation"
        );
        assert!(!r.code && !r.doc, "and nothing else");

        // The store is full of churn that must NOT trigger a pass: the sqlite
        // index and its WAL are rewritten constantly, and index.json is
        // regenerated by reconciliation itself — routing it would be a loop.
        for noisy in [
            Path::new("/project/.mdkb/index.sqlite-wal"),
            Path::new("/project/.mdkb/memory/index.json"),
            Path::new("/project/.mdkb/memory/archive/old.md"),
        ] {
            let r = classify_change(noisy, &collections, &excludes, entries);
            assert!(
                !r.memory,
                "{} must not trigger reconciliation",
                noisy.display()
            );
        }
    }

    #[test]
    fn test_classify_change_rs_outside_collection() {
        let path = Path::new("/project/src/main.rs");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(code_doc(path, &collections, &excludes), (true, false));
    }

    #[test]
    fn test_classify_change_md_in_collection() {
        let path = Path::new("/project/docs/readme.md");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(code_doc(path, &collections, &excludes), (false, true));
    }

    #[test]
    fn test_classify_change_rs_in_collection() {
        let path = Path::new("/project/docs/example.rs");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(code_doc(path, &collections, &excludes), (true, true));
    }

    #[test]
    fn test_classify_change_irrelevant_file() {
        let path = Path::new("/project/data.json");
        let collections = vec![PathBuf::from("/project/docs")];
        let excludes = build_code_excludes(Path::new("/project"), &[]);
        assert_eq!(code_doc(path, &collections, &excludes), (false, false));
    }

    #[test]
    fn test_classify_change_excludes_node_modules() {
        let root = Path::new("/project");
        let collections = vec![PathBuf::from("/project/docs")];
        let patterns = vec!["**/node_modules/**".to_string()];
        let excludes = build_code_excludes(root, &patterns);

        // .js in node_modules should NOT be classified as code
        let path = Path::new("/project/node_modules/lodash/index.js");
        assert_eq!(code_doc(path, &collections, &excludes), (false, false));

        // .js outside node_modules should still be code
        let path = Path::new("/project/src/app.js");
        assert_eq!(code_doc(path, &collections, &excludes), (true, false));
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
            code_doc(
                Path::new("/project/node_modules/x/a.js"),
                &collections,
                &excludes
            ),
            (false, false)
        );
        assert_eq!(
            code_doc(
                Path::new("/project/dist/bundle.js"),
                &collections,
                &excludes
            ),
            (false, false)
        );
        assert_eq!(
            code_doc(
                Path::new("/project/target/debug/build.rs"),
                &collections,
                &excludes
            ),
            (false, false)
        );
        assert_eq!(
            code_doc(Path::new("/project/src/main.rs"), &collections, &excludes),
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
                source_file: None,
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                ttl: None,
                due_in: None,
                relates: vec![],
                agent: None,
                on_conflict: None,
                root: None,
                dry_run: false,
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
                source_file: None,
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                ttl: None,
                due_in: None,
                relates: vec![],
                agent: None,
                on_conflict: None,
                root: None,
                dry_run: false,
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
        let config = global_test_config();
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

        let config = global_test_config();
        let registry = Arc::new(RepoRegistry::new(config));
        registry.get_or_open(&root).unwrap();

        let server = McpServer::global(Arc::clone(&registry));
        // No root param, 1 root registered → auto-selects
        let handle = server.resolve_handle(None).await.unwrap();
        assert_eq!(handle.root, root.canonicalize().unwrap());
    }

    #[tokio::test]
    async fn test_resolve_handle_global_no_roots() {
        let config = global_test_config();
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

        let config = global_test_config();
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

        let config = global_test_config();
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
    async fn test_resolve_handle_rejects_wildcard_outside_search() {
        let config = global_test_config();
        let registry = Arc::new(RepoRegistry::new(config));
        let server = McpServer::global(registry);

        let err = server.resolve_handle(Some("*")).await.unwrap_err();
        assert!(
            format!("{:?}", err).contains("supported only by search"),
            "Should explain that wildcard roots are search-only: {:?}",
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
                threshold: None,
                file: None,
                min_confidence: None,
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
            let ctx1 = crate::core::Context::open(&root1).unwrap();
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
            crate::core::indexing::handle_update(&ctx1, &root1).unwrap();
        }
        {
            let ctx2 = crate::core::Context::open(&root2).unwrap();
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
            crate::core::indexing::handle_update(&ctx2, &root2).unwrap();
        }

        // Create global server with both repos
        let config = global_test_config();
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
                threshold: None,
                file: None,
                min_confidence: None,
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
        let ctx = crate::core::Context::open(&root).unwrap();
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
            let ctx = crate::core::Context::open(&root).unwrap();
            crate::core::indexing::handle_update(&ctx, &root).unwrap();
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
                threshold: None,
                file: None,
                min_confidence: None,
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
            let ctx = crate::core::Context::open(&root2).unwrap();
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
            let ctx = crate::core::Context::open(&root2).unwrap();
            crate::core::indexing::handle_update(&ctx, &root2).unwrap();
        }

        let config = global_test_config();
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
                threshold: None,
                file: None,
                min_confidence: None,
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
                        source_file: None,
                        entry_type: "topic".to_string(),
                        tags: vec!["test".to_string()],
                        source_type: "user_statement".to_string(),
                        ttl: None,
                        due_in: None,
                        relates: vec![],
                        agent: None,
                        on_conflict: None,
                    },
                    MemoryWriteBatchEntry {
                        id: "batch-b".to_string(),
                        title: "Batch B".to_string(),
                        content: "Content B".to_string(),
                        source_file: None,
                        entry_type: "decision".to_string(),
                        tags: vec![],
                        source_type: "user_statement".to_string(),
                        ttl: None,
                        due_in: None,
                        relates: vec![],
                        agent: None,
                        on_conflict: None,
                    },
                ],
                root: None,
                dry_run: false,
            }))
            .await
            .expect("batch write should succeed");

        let text = extract_text(&result);
        assert!(
            text.contains("Created memory entry: batch-a"),
            "Should contain batch-a: {text}"
        );
        assert!(
            text.contains("Created memory entry: batch-b"),
            "Should contain batch-b: {text}"
        );

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
                dry_run: false,
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
                source_file: None,
                entry_type: "topic".to_string(),
                tags: vec![],
                source_type: "user_statement".to_string(),
                ttl: None,
                due_in: None,
                relates: vec![],
                agent: None,
                on_conflict: None,
            })
            .collect();

        let result = server
            .memory_write_batch(Parameters(MemoryWriteBatchParams {
                entries,
                root: None,
                dry_run: false,
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
