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

#[cfg(any(feature = "http-server", feature = "https-server"))]
pub mod common;

#[cfg(feature = "http-server")]
pub mod http_server;

#[cfg(feature = "https-server")]
pub mod https_server;

#[doc(inline)]
pub use server::McpServer;
