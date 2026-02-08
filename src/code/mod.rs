//! Code intelligence for multi-language source analysis.
//!
//! Provides symbol extraction, relationship graphing, and semantic code search
//! via tree-sitter parsing and Tantivy indexing. Gated behind the `code-intel` feature.

pub mod indexing;
pub mod parsing;
pub mod project_resolver;
pub mod relationship;
pub mod storage;
pub mod symbol;
pub mod types;
