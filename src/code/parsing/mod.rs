//! Language parsing framework for code intelligence.
//!
//! Provides the [`LanguageParser`] trait and supporting types for extracting
//! symbols, relationships, and call graphs from source code via tree-sitter.

pub mod context;
pub mod import;
pub mod language;
pub mod method_call;
pub mod parser;
pub mod rust;
