//! MCP server implementation.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use rmcp::handler::server::tool::{Parameters, ToolRouter};
use rmcp::handler::server::ServerHandler;
use rmcp::model::{CallToolResult, Content, ErrorCode, Implementation, ServerCapabilities, ServerInfo};
use rmcp::ServiceExt;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError};
use tokio::sync::Mutex;

use crate::cli::handlers::{handle_mget, handle_update, Context};
use crate::domain::{SearchQuery, SearchResult};
use crate::store::{collections, documents, search};
use crate::watcher::{FileWatcher, WatcherConfig};

use super::tools::{GetParams, MultiGetParams, SearchParams};

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
}

#[tool_router]
impl McpServer {
    /// Create a new MCP server.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            ctx: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// Initialize the database connection.
    async fn ensure_context(&self) -> Result<(), McpError> {
        let mut ctx_guard = self.ctx.lock().await;
        if ctx_guard.is_none() {
            let ctx = Context::open(&self.root)
                .map_err(|e| mcp_error(format!("Failed to open database: {}", e)))?;
            *ctx_guard = Some(ctx);
        }
        Ok(())
    }

    /// Search documents using BM25 full-text search.
    #[tool(description = "Search markdown documents using BM25 full-text search")]
    async fn mdkb_search(
        &self,
        Parameters(params): Parameters<SearchParams>,
    ) -> Result<CallToolResult, McpError> {
        self.ensure_context().await?;

        let ctx_guard = self.ctx.lock().await;
        let ctx = ctx_guard
            .as_ref()
            .ok_or_else(|| mcp_error("Database not initialized"))?;

        let query = SearchQuery {
            text: params.query,
            limit: params.limit,
            collection: params.collection,
            tags: vec![],
        };

        let results = search::search(&ctx.conn, &query)
            .map_err(|e| mcp_error(format!("Search failed: {}", e)))?;

        let output = format_search_results(&results);
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
        for (doc, content) in &results {
            let title = doc.title.as_deref().unwrap_or("(untitled)");
            output.push_str(&format!(
                "=== [{}] {} - {} ===\n{}\n\n",
                doc.id, doc.relative_path, title, content
            ));
        }

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
    let server = McpServer::new(root.clone());
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
        .map_err(|e| crate::error::Error::Mcp(format!("Failed to start server: {}", e)))?;

    service
        .waiting()
        .await
        .map_err(|e| crate::error::Error::Mcp(format!("Server error: {}", e)))?;

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

/// Format search results for output.
fn format_search_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }

    let mut output = String::new();
    for r in results {
        let title = r.title.as_deref().unwrap_or("(untitled)");
        output.push_str(&format!(
            "[{}] {} - {} (score: {:.2})\n",
            r.id, r.path, title, r.score
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
            path: "readme.md".to_string(),
            title: Some("README".to_string()),
            score: -5.5,
            snippets: vec!["...matching text...".to_string()],
        }];
        let output = format_search_results(&results);
        assert!(output.contains("[1]"));
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
