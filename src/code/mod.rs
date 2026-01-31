//! Code intelligence for multi-language source analysis.
//!
//! Provides symbol extraction, relationship graphing, and semantic code search
//! via tree-sitter parsing and SQLite indexing.

pub mod indexing;
pub mod parsing;
pub mod relationship;
pub mod semantic;
pub mod storage;
pub mod symbol;
pub mod types;
