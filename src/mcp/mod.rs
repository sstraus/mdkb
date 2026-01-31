//! MCP (Model Context Protocol) server implementation.
//!
//! Exposes mdkb functionality as an MCP server with tools for:
//! - `mdkb_search` - Full-text BM25 search
//! - `mdkb_get` - Document retrieval
//! - `mdkb_status` - Index status
//! - `mdkb_update` - Trigger reindex

pub mod server;
pub mod tools;

pub use server::McpServer;
