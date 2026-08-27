//! Intermediate types for the indexing pipeline stages.
//!
//! Data flows through stages with increasing enrichment:
//! `PathBuf` → [`FileContent`] → [`ParsedFile`] → [`IndexBatch`].

use std::path::PathBuf;

use crate::code::parsing::language::Language;
use crate::code::relationship::RelationKind;
use crate::code::symbol::{ScopeContext, Symbol, Visibility};
use crate::code::types::{CompactString, FileId, Range, SymbolId, SymbolKind};

// ---------------------------------------------------------------------------
// READ stage output
// ---------------------------------------------------------------------------

/// File content read from disk with its content hash.
#[derive(Debug)]
pub struct FileContent {
    pub path: PathBuf,
    pub content: String,
    pub hash: String,
    /// Approximate LLM token count (cl100k_base) for the file's text.
    pub token_estimate: u32,
}

// ---------------------------------------------------------------------------
// PARSE stage output (no IDs yet)
// ---------------------------------------------------------------------------

/// A symbol extracted from parsing, before ID assignment.
#[derive(Debug)]
pub struct RawSymbol {
    pub name: CompactString,
    pub kind: SymbolKind,
    pub range: Range,
    pub signature: Option<Box<str>>,
    pub doc_comment: Option<Box<str>>,
    pub visibility: Visibility,
    pub scope_context: Option<ScopeContext>,
    /// The namespace the parser read out of the source, empty when the language
    /// declares none. COLLECT combines it with the address derived from the
    /// file's path — see [`module_path_for`](super::module_path::module_path_for).
    pub module_path: Box<str>,
}

/// A relationship extracted from parsing, before symbol ID resolution.
#[derive(Debug)]
pub struct RawRelationship {
    pub from_name: Box<str>,
    pub from_range: Range,
    pub to_name: Box<str>,
    /// What the call site wrote before the target's last segment, `None` for a
    /// bare name. It narrows the target and never widens it: a qualifier that
    /// names nothing indexed makes the target external rather than letting it
    /// match a same-named local symbol.
    pub to_qualifier: Option<Box<str>>,
    pub to_range: Range,
    pub kind: RelationKind,
}

/// An import extracted from parsing, before file ID assignment.
#[derive(Debug)]
pub struct RawImport {
    pub path: Box<str>,
    pub alias: Option<Box<str>>,
    pub is_glob: bool,
    pub is_type_only: bool,
}

/// All parse results for a single file.
#[derive(Debug)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub content_hash: String,
    pub language: Language,
    pub token_estimate: u32,
    pub raw_symbols: Vec<RawSymbol>,
    pub raw_relationships: Vec<RawRelationship>,
    pub raw_imports: Vec<RawImport>,
}

// ---------------------------------------------------------------------------
// COLLECT stage output (IDs assigned)
// ---------------------------------------------------------------------------

/// Metadata for a file registered in the index.
#[derive(Debug)]
pub struct FileRegistration {
    pub path: PathBuf,
    /// Path relative to the indexed root directory (e.g. `src/main.rs`).
    pub rel_path: Box<str>,
    pub file_id: FileId,
    pub content_hash: String,
    pub language: Language,
    pub mtime: u64,
    /// Approximate LLM token count (cl100k_base) for the file's text.
    pub token_estimate: u32,
}

/// A relationship where the source symbol may be resolved but the target
/// is still a name string (resolved in a later phase).
#[derive(Debug)]
pub struct CollectedRelationship {
    pub from_id: Option<SymbolId>,
    pub from_name: Box<str>,
    pub to_name: Box<str>,
    pub to_qualifier: Option<Box<str>>,
    pub file_id: FileId,
    pub kind: RelationKind,
    pub to_range: Option<Range>,
}

/// An import with the file it was found in resolved to a pipeline ID.
#[derive(Debug)]
pub struct CollectedImport {
    pub file_id: FileId,
    pub import: RawImport,
}

/// A batch of index data ready for SQLite writes.
#[derive(Debug)]
pub struct IndexBatch {
    pub symbols: Vec<(Symbol, PathBuf)>,
    pub unresolved_relationships: Vec<CollectedRelationship>,
    pub file_registrations: Vec<FileRegistration>,
    pub imports: Vec<CollectedImport>,
}

impl IndexBatch {
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }
}

impl Default for IndexBatch {
    fn default() -> Self {
        Self {
            symbols: Vec::new(),
            unresolved_relationships: Vec::new(),
            file_registrations: Vec::new(),
            imports: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline results
// ---------------------------------------------------------------------------

/// Statistics from a pipeline run.
///
/// Serializable because a routed `mdkb update` runs in the daemon and is
/// reported by the CLI: the numbers have to cross a socket to reach the process
/// that prints them.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexStats {
    pub files_discovered: u32,
    pub files_indexed: u32,
    /// Files dropped from the index because they no longer exist on disk. Only a
    /// caller that walked the whole tree can know this, so it is zero on the
    /// explicit-path paths (`index_files`, `reindex_files`).
    pub files_removed: u32,
    pub symbols_indexed: u32,
    pub relationships_collected: u32,
    /// Files the PARSE stage could not handle: unsupported/unknown language or a
    /// parser that failed to construct. Surfaced so a partial index is visible
    /// rather than silently reported as a clean success.
    pub parse_errors: u32,
}
