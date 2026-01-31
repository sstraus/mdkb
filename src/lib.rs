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
//! # Features
//!
//! - `llm`: Enable local LLM inference for semantic search (Phase 3+)

pub mod cli;
pub mod config;
pub mod domain;
pub mod error;
pub mod llm;
pub mod mcp;
pub mod store;
pub mod watcher;

// Re-export main types
pub use config::Config;
pub use error::{Error, Result};
