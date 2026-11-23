//! MCP (Model Context Protocol) server implementation.
//!
//! Exposes mdkb functionality as an MCP server with tools for:
//! - `mdkb_search` - Full-text BM25 search
//! - `mdkb_get` - Document retrieval
//! - `mdkb_status` - Index status
//! - `mdkb_update` - Trigger reindex

pub mod dispatch;
pub mod server;
pub mod tools;

use std::borrow::Cow;

use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;

/// Create an `INTERNAL_ERROR` MCP error from any message.
pub(super) fn mcp_error(message: impl Into<Cow<'static, str>>) -> McpError {
    McpError {
        code: ErrorCode::INTERNAL_ERROR,
        message: message.into(),
        data: None,
    }
}

/// Wait for a shutdown signal: SIGTERM or SIGINT on Unix, Ctrl-C elsewhere.
/// Returns the signal number that arrived (`SIGTERM` = 15, `SIGINT` = 2) so a
/// caller that must exit with the code a shell expects (`128 + signum`) does
/// not have to invent one.
///
/// Shared by the daemon (`main.rs`) and the HTTP MCP transport
/// (`http_server.rs`) so there is exactly one signal-installation path — an
/// HTTP server that only watched Ctrl-C ignored SIGTERM, the signal `docker
/// stop` and systemd both send.
///
/// May be awaited repeatedly to catch a second signal: each call installs a
/// fresh listener, and Unix notifies every listener registered for a signal
/// kind, so a call made after a previous one resolved still sees the next
/// occurrence.
pub async fn wait_for_shutdown_signal() -> crate::error::Result<i32> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())
            .map_err(|e| crate::error::Error::mcp(format!("sigterm handler: {e}")))?;
        let mut int = signal(SignalKind::interrupt())
            .map_err(|e| crate::error::Error::mcp(format!("sigint handler: {e}")))?;
        Ok(tokio::select! {
            _ = term.recv() => 15,
            _ = int.recv() => 2,
        })
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| crate::error::Error::mcp(format!("ctrl-c handler: {e}")))?;
        Ok(2)
    }
}

#[cfg(any(feature = "http-server", feature = "https-server"))]
pub mod common;

#[cfg(feature = "http-server")]
pub mod http_server;

#[cfg(feature = "https-server")]
pub mod https_server;

#[doc(inline)]
pub use server::McpServer;
