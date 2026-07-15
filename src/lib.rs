//! mdkb - Local markdown knowledge base with semantic search.
//!
//! A Rust CLI tool and MCP server for indexing and searching markdown files.
//!
//! # Architecture
//!
//! The crate follows hexagonal architecture with three main layers:
//!
//! - **Domain**: Core business logic (entities, operations)
//! - **Store**: SQLite storage layer (rusqlite + FTS5)
//! - **CLI/MCP**: User interfaces (clap CLI, rmcp MCP server)
//!
pub mod cli;
pub mod code;
pub mod config;
pub mod daemon;
pub mod domain;
pub mod error;
pub mod eval;
pub mod git;
pub mod llm;
pub mod mcp;
pub mod metrics;
pub mod store;
pub mod watcher;

// Re-export main types
#[doc(inline)]
pub use config::Config;
#[doc(inline)]
pub use daemon::config::DaemonConfig;
#[doc(inline)]
pub use error::{Error, ErrorKind, Result};
