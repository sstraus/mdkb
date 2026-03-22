//! Language parsing framework for code intelligence.
//!
//! Provides the [`LanguageParser`] trait and supporting types for extracting
//! symbols, relationships, and call graphs from source code via tree-sitter.

pub mod c_lang;
pub mod caching_parser;
pub mod context;
pub mod cpp;
pub mod csharp;
pub mod gdscript;
pub mod go;
pub mod import;
pub mod java;
pub mod kotlin;
pub mod language;
pub mod lua;
pub mod method_call;
pub mod parser;
pub mod php;
pub mod python;
pub mod rust;
pub mod swift;
pub mod typescript;
