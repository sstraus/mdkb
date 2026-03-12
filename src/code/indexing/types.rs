//! Intermediate types for the indexing pipeline stages.
//!
//! Data flows through stages with increasing enrichment:
//! `PathBuf` → [`FileContent`] → [`ParsedFile`] → [`IndexBatch`].

use std::path::PathBuf;

use crate::code::parsing::language::Language;
use crate::code::relationship::RelationKind;
use crate::code::symbol::{Symbol, Visibility, ScopeContext};
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
}

/// A relationship extracted from parsing, before symbol ID resolution.
#[derive(Debug)]
pub struct RawRelationship {
    pub from_name: Box<str>,
    pub from_range: Range,
    pub to_name: Box<str>,
    pub to_range: Range,
    pub kind: RelationKind,
}

/// All parse results for a single file.
#[derive(Debug)]
pub struct ParsedFile {
    pub path: PathBuf,
    pub content_hash: String,
    pub language: Language,
    pub raw_symbols: Vec<RawSymbol>,
    pub raw_relationships: Vec<RawRelationship>,
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
}

/// A relationship where the source symbol may be resolved but the target
/// is still a name string (resolved in a later phase).
#[derive(Debug)]
pub struct UnresolvedRelationship {
    pub from_id: Option<SymbolId>,
    pub from_name: Box<str>,
    pub to_name: Box<str>,
    pub file_id: FileId,
    pub kind: RelationKind,
    pub to_range: Option<Range>,
}

/// A batch of index data ready for SQLite writes.
#[derive(Debug)]
pub struct IndexBatch {
    pub symbols: Vec<(Symbol, PathBuf)>,
    pub unresolved_relationships: Vec<UnresolvedRelationship>,
    pub file_registrations: Vec<FileRegistration>,
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
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline results
// ---------------------------------------------------------------------------

/// Statistics from a pipeline run.
#[derive(Debug, Default)]
pub struct IndexStats {
    pub files_discovered: u32,
    pub files_indexed: u32,
    pub symbols_indexed: u32,
    pub relationships_collected: u32,
    pub files_skipped: u32,
    pub errors: u32,
}
