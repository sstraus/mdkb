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

#[cfg(any(feature = "http-server", feature = "https-server"))]
pub mod common;

#[cfg(feature = "http-server")]
pub mod http_server;

#[cfg(feature = "https-server")]
pub mod https_server;

#[doc(inline)]
pub use server::McpServer;
