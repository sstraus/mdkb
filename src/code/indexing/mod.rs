//! Multi-stage parallel indexing pipeline for code intelligence.
//!
//! Discovers, reads, parses, collects, and indexes source code files
//! using crossbeam channels for stage-to-stage communication.
//!
//! The main entry point is [`IndexFacade`], which wraps the pipeline and
//! provides query methods over the Tantivy index.

pub mod hasher;
pub mod pipeline;
pub mod types;
pub mod walker;

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::Term;

use crate::code::relationship::RelationKind;
use crate::code::semantic::{SemanticSearch, format_symbol_text};
use crate::code::storage::CodeIndex;
use crate::code::symbol::{Symbol, Visibility};
use crate::code::types::{FileId, Range, SymbolId, SymbolKind};

use self::pipeline::PipelineConfig;
use self::types::{IndexStats, UnresolvedRelationship};

/// Facade over the indexing pipeline and Tantivy queries.
///
/// Owns the code index and provides both mutation (indexing) and
/// query (search, graph traversal) operations.
#[derive(Debug)]
pub struct IndexFacade {
    index: CodeIndex,
    config: PipelineConfig,
    unresolved: Vec<UnresolvedRelationship>,
    semantic: Option<SemanticSearch>,
}

impl IndexFacade {
    /// Create a new facade with an index at the given path.
    pub fn create(index_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let index = CodeIndex::create(index_path)?;
        let semantic = init_semantic(index.path());
        Ok(Self {
            index,
            config: PipelineConfig::default(),
            unresolved: Vec::new(),
            semantic,
        })
    }

    /// Open an existing index, or create one if it doesn't exist.
    pub fn open_or_create(index_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let index = CodeIndex::open_or_create(index_path)?;
        let semantic = init_semantic(index.path());
        Ok(Self {
            index,
            config: PipelineConfig::default(),
            unresolved: Vec::new(),
            semantic,
        })
    }

    /// Override the default pipeline configuration.
    pub fn with_config(mut self, config: PipelineConfig) -> Self {
        self.config = config;
        self
    }

    // -----------------------------------------------------------------------
    // Indexing operations
    // -----------------------------------------------------------------------

    /// Index all source files under the given directory.
    pub fn index_directory(&mut self, root: &Path) -> anyhow::Result<IndexStats> {
        let (stats, unresolved) = pipeline::index_directory(root, &self.index, &self.config)?;
        self.unresolved.extend(unresolved);
        self.generate_symbol_embeddings();
        Ok(stats)
    }

    /// Re-index a directory (full reindex, discarding previous data).
    pub fn reindex(&mut self, root: &Path) -> anyhow::Result<IndexStats> {
        // Clear existing data by deleting all documents.
        // Scope the writer so it's dropped before index_directory creates a new one.
        {
            let mut writer = self.index.writer()?;
            writer.delete_all_documents()?;
            writer.commit()?;
        }
        self.unresolved.clear();
        if let Some(ref semantic) = self.semantic {
            if let Err(e) = semantic.clear() {
                tracing::error!("Failed to clear semantic index: {e}. Impact: old embeddings may persist.");
            }
        }
        self.index.reload()?;

        self.index_directory(root)
    }

    /// Access unresolved relationships (for future resolution phases).
    pub fn unresolved_relationships(&self) -> &[UnresolvedRelationship] {
        &self.unresolved
    }

    // -----------------------------------------------------------------------
    // Query operations
    // -----------------------------------------------------------------------

    /// Find a symbol by exact name. Returns the first match.
    pub fn get_symbol_by_name(&self, name: &str) -> Option<Symbol> {
        let searcher = self.index.reader().searcher();
        let schema = self.index.schema();

        let name_term = Term::from_field_text(schema.name, name);
        let type_term = Term::from_field_text(schema.doc_type, "symbol");

        let query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(TermQuery::new(name_term, IndexRecordOption::Basic))),
            (Occur::Must, Box::new(TermQuery::new(type_term, IndexRecordOption::Basic))),
        ]);

        let top = searcher.search(&query, &TopDocs::with_limit(1)).ok()?;
        let (_score, addr) = top.into_iter().next()?;
        let doc = searcher.doc::<tantivy::TantivyDocument>(addr).ok()?;
        doc_to_symbol(&doc, schema)
    }

    /// Find a symbol by its ID.
    pub fn get_symbol(&self, id: SymbolId) -> Option<Symbol> {
        let searcher = self.index.reader().searcher();
        let schema = self.index.schema();

        let id_term = Term::from_field_u64(schema.symbol_id, u64::from(id.value()));
        let type_term = Term::from_field_text(schema.doc_type, "symbol");

        let query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(TermQuery::new(id_term, IndexRecordOption::Basic))),
            (Occur::Must, Box::new(TermQuery::new(type_term, IndexRecordOption::Basic))),
        ]);

        let top = searcher.search(&query, &TopDocs::with_limit(1)).ok()?;
        let (_score, addr) = top.into_iter().next()?;
        let doc = searcher.doc::<tantivy::TantivyDocument>(addr).ok()?;
        doc_to_symbol(&doc, schema)
    }

    /// Find all symbols matching a name (may return multiple across files).
    pub fn find_symbols_by_name(&self, name: &str) -> Vec<Symbol> {
        let searcher = self.index.reader().searcher();
        let schema = self.index.schema();

        let name_term = Term::from_field_text(schema.name, name);
        let type_term = Term::from_field_text(schema.doc_type, "symbol");

        let query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(TermQuery::new(name_term, IndexRecordOption::Basic))),
            (Occur::Must, Box::new(TermQuery::new(type_term, IndexRecordOption::Basic))),
        ]);

        let Ok(top) = searcher.search(&query, &TopDocs::with_limit(100)) else {
            return Vec::new();
        };

        top.into_iter()
            .filter_map(|(_score, addr)| {
                let doc = searcher.doc::<tantivy::TantivyDocument>(addr).ok()?;
                doc_to_symbol(&doc, schema)
            })
            .collect()
    }

    /// Search symbols by a query string (uses ngram tokenizer for partial matching).
    pub fn search_symbols(&self, query: &str, limit: usize) -> Vec<Symbol> {
        let searcher = self.index.reader().searcher();
        let schema = self.index.schema();

        let parser = QueryParser::for_index(
            self.index.index(),
            vec![schema.name_text, schema.doc_comment, schema.signature],
        );

        let Ok(parsed) = parser.parse_query(query) else {
            return Vec::new();
        };

        // Combine with doc_type filter
        let type_term = Term::from_field_text(schema.doc_type, "symbol");
        let combined = BooleanQuery::new(vec![
            (Occur::Must, parsed),
            (Occur::Must, Box::new(TermQuery::new(type_term, IndexRecordOption::Basic))),
        ]);

        let Ok(top) = searcher.search(&combined, &TopDocs::with_limit(limit)) else {
            return Vec::new();
        };

        top.into_iter()
            .filter_map(|(_score, addr)| {
                let doc = searcher.doc::<tantivy::TantivyDocument>(addr).ok()?;
                doc_to_symbol(&doc, schema)
            })
            .collect()
    }

    /// Get functions/methods called by the given symbol (via unresolved relationships).
    pub fn get_called_functions(&self, symbol_id: SymbolId) -> Vec<Symbol> {
        self.resolve_relationship_targets(symbol_id, RelationKind::Calls)
    }

    /// Get functions/methods that call the given symbol.
    pub fn get_calling_functions(&self, symbol_id: SymbolId) -> Vec<Symbol> {
        // Look for relationships where to_name matches our symbol
        let Some(symbol) = self.get_symbol(symbol_id) else {
            return Vec::new();
        };

        self.unresolved
            .iter()
            .filter(|r| r.kind == RelationKind::Calls && &*r.to_name == symbol.as_name())
            .filter_map(|r| r.from_id)
            .filter_map(|id| self.get_symbol(id))
            .collect()
    }

    /// Compute the impact radius: all symbols reachable from the given one
    /// within `max_depth` hops via any relationship.
    pub fn get_impact_radius(&self, start: SymbolId, max_depth: usize) -> Vec<SymbolId> {
        // Build adjacency list once to avoid N+1 Tantivy queries during graph walk
        let adjacency = self.build_adjacency_list();

        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        visited.insert(start);
        queue.push_back((start, 0usize));

        while let Some((current, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }

            // Use hash lookup instead of querying Tantivy for each edge
            if let Some(targets) = adjacency.get(&current) {
                for &target in targets {
                    if visited.insert(target) {
                        queue.push_back((target, depth + 1));
                    }
                }
            }
        }

        visited.into_iter().filter(|&id| id != start).collect()
    }

    /// Build an adjacency list from unresolved relationships for efficient graph traversal.
    ///
    /// Returns a HashMap<SymbolId, Vec<SymbolId>> mapping source symbols to target symbols.
    /// Resolves target names to IDs via batch Tantivy queries.
    fn build_adjacency_list(&self) -> std::collections::HashMap<SymbolId, Vec<SymbolId>> {
        let mut adjacency: std::collections::HashMap<SymbolId, Vec<SymbolId>> =
            std::collections::HashMap::new();

        // Build a cache of name -> Vec<SymbolId> to minimize Tantivy queries
        let mut name_cache: std::collections::HashMap<&str, Vec<SymbolId>> =
            std::collections::HashMap::new();

        for rel in &self.unresolved {
            let Some(from_id) = rel.from_id else {
                continue;
            };

            // Look up target IDs, caching results
            let target_ids = name_cache.entry(&rel.to_name).or_insert_with(|| {
                self.find_symbols_by_name(&rel.to_name)
                    .into_iter()
                    .map(|s| s.id)
                    .collect()
            });

            adjacency
                .entry(from_id)
                .or_default()
                .extend(target_ids.iter().copied());
        }

        adjacency
    }

    // -----------------------------------------------------------------------
    // Semantic search
    // -----------------------------------------------------------------------

    /// Search symbols by semantic similarity to a natural language query.
    ///
    /// Returns `(Symbol, score)` pairs sorted by descending similarity.
    pub fn semantic_search(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
    ) -> anyhow::Result<Vec<(Symbol, f32)>> {
        let Some(ref semantic) = self.semantic else {
            return Ok(Vec::new());
        };

        let matches = semantic.search(query, limit, threshold)?;

        let results: Vec<(Symbol, f32)> = matches
            .into_iter()
            .filter_map(|m| {
                let id = SymbolId::new(m.symbol_id)?;
                let symbol = self.get_symbol(id)?;
                Some((symbol, m.score))
            })
            .collect();

        Ok(results)
    }

    /// Pre-initialize the semantic search model on a blocking thread.
    ///
    /// Call from async context to avoid blocking the tokio executor during
    /// model download. No-op if semantic search is not configured or model
    /// is already loaded.
    pub async fn init_semantic_model(&self) -> anyhow::Result<()> {
        if let Some(ref semantic) = self.semantic {
            semantic.init_model_async().await?;
        }
        Ok(())
    }

    /// Number of stored semantic embeddings.
    pub fn semantic_count(&self) -> usize {
        self.semantic
            .as_ref()
            .and_then(|s| s.count().ok())
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Statistics
    // -----------------------------------------------------------------------

    /// Total number of indexed symbols.
    pub fn symbol_count(&self) -> u64 {
        count_by_type(&self.index, "symbol")
    }

    /// Total number of indexed files.
    pub fn file_count(&self) -> u64 {
        count_by_type(&self.index, "file")
    }

    /// Total number of collected relationships (unresolved).
    pub fn relationship_count(&self) -> usize {
        self.unresolved.len()
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Generate embeddings for all indexed symbols and write to the vector store.
    fn generate_symbol_embeddings(&self) {
        let Some(ref semantic) = self.semantic else {
            return;
        };

        let symbols = self.all_symbols(100_000);
        if symbols.is_empty() {
            return;
        }

        let embed_inputs: Vec<(u32, String)> = symbols
            .iter()
            .map(|sym| {
                let text = format_symbol_text(
                    sym.kind,
                    sym.as_name(),
                    sym.as_signature(),
                    sym.as_doc_comment(),
                );
                (sym.id.value(), text)
            })
            .collect();

        tracing::info!("Generating semantic embeddings for {} symbols...", embed_inputs.len());
        if let Err(e) = semantic.generate_embeddings(&embed_inputs) {
            tracing::error!(
                "Failed to generate semantic embeddings: {e}. Impact: {} symbols will not be searchable via semantic_search().",
                embed_inputs.len()
            );
        }
    }

    /// Query all symbol documents from the Tantivy index.
    fn all_symbols(&self, limit: usize) -> Vec<Symbol> {
        let searcher = self.index.reader().searcher();
        let schema = self.index.schema();

        let type_term = Term::from_field_text(schema.doc_type, "symbol");
        let query = TermQuery::new(type_term, IndexRecordOption::Basic);

        let Ok(top) = searcher.search(&query, &TopDocs::with_limit(limit)) else {
            return Vec::new();
        };

        top.into_iter()
            .filter_map(|(_score, addr)| {
                let doc = searcher.doc::<tantivy::TantivyDocument>(addr).ok()?;
                doc_to_symbol(&doc, schema)
            })
            .collect()
    }

    /// Resolve relationship targets for a given source symbol and relation kind.
    fn resolve_relationship_targets(&self, source: SymbolId, kind: RelationKind) -> Vec<Symbol> {
        self.unresolved
            .iter()
            .filter(|r| r.from_id == Some(source) && r.kind == kind)
            .filter_map(|r| {
                let targets = self.find_symbols_by_name(&r.to_name);
                targets.into_iter().next()
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Semantic search initialization
// ---------------------------------------------------------------------------

/// Try to initialize `SemanticSearch` at `{index_path}/vectors.bin`.
///
/// Returns `None` if initialization fails (logged as error with impact).
fn init_semantic(index_path: &Path) -> Option<SemanticSearch> {
    let vectors_path = index_path.join("vectors.bin");
    match SemanticSearch::new(&vectors_path) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::error!(
                "Failed to initialize semantic search at {}: {e}. Impact: semantic_search() will return empty results.",
                vectors_path.display()
            );
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Document → Symbol conversion
// ---------------------------------------------------------------------------

/// Extract a Symbol from a Tantivy document.
fn doc_to_symbol(
    doc: &tantivy::TantivyDocument,
    schema: &crate::code::storage::IndexSchema,
) -> Option<Symbol> {
    use tantivy::schema::OwnedValue;

    let get_text = |field| -> Option<String> {
        let owned: OwnedValue = doc.get_first(field)?.into();
        match owned {
            OwnedValue::Str(s) => Some(s),
            _ => None,
        }
    };

    let get_u64 = |field| -> Option<u64> {
        let owned: OwnedValue = doc.get_first(field)?.into();
        match owned {
            OwnedValue::U64(n) => Some(n),
            _ => None,
        }
    };

    let symbol_id = SymbolId::new(get_u64(schema.symbol_id)? as u32)?;
    let name = get_text(schema.name)?;
    let kind_str = get_text(schema.kind)?;
    let kind: SymbolKind = kind_str.parse().ok()?;
    let file_id = FileId::new(get_u64(schema.file_id)? as u32)?;
    let file_path = get_text(schema.file_path)?;

    let start_line = get_u64(schema.line_number).unwrap_or(0) as u32;
    let start_col = get_u64(schema.column).unwrap_or(0) as u16;
    let end_line = get_u64(schema.end_line).unwrap_or(0) as u32;
    let end_col = get_u64(schema.end_column).unwrap_or(0) as u16;

    let visibility_num = get_u64(schema.visibility).unwrap_or(0);
    let visibility = match visibility_num {
        0 => Visibility::Public,
        1 => Visibility::Crate,
        2 => Visibility::Module,
        _ => Visibility::Private,
    };

    let mut symbol = Symbol {
        id: symbol_id,
        name: name.into(),
        kind,
        file_id,
        range: Range::new(start_line, start_col, end_line, end_col),
        file_path: file_path.into(),
        signature: None,
        doc_comment: None,
        module_path: None,
        visibility,
        scope_context: None,
    };

    if let Some(sig) = get_text(schema.signature) {
        symbol.signature = Some(sig.into());
    }
    if let Some(doc) = get_text(schema.doc_comment) {
        symbol.doc_comment = Some(doc.into());
    }
    if let Some(module) = get_text(schema.module_path) {
        symbol.module_path = Some(module.into());
    }

    Some(symbol)
}

/// Count documents of a given type in the index.
fn count_by_type(index: &CodeIndex, doc_type: &str) -> u64 {
    let searcher = index.reader().searcher();
    let schema = index.schema();
    let term = Term::from_field_text(schema.doc_type, doc_type);
    let query = TermQuery::new(term, IndexRecordOption::Basic);

    match searcher.search(&query, &tantivy::collector::Count) {
        Ok(count) => count as u64,
        Err(_) => 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_facade_create_and_index() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(
            src_dir.path().join("main.rs"),
            r#"
pub fn hello() -> String {
    String::from("hello")
}

pub fn world() {
    hello();
}
"#,
        )
        .unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(idx_dir.path().join("idx")).unwrap();
        let stats = facade.index_directory(src_dir.path()).unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.symbols_indexed >= 2);
        assert!(facade.symbol_count() >= 2);
        assert_eq!(facade.file_count(), 1);
    }

    #[test]
    fn test_facade_get_symbol_by_name() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("lib.rs"), "pub fn unique_symbol_name() {}").unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(idx_dir.path().join("idx")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let sym = facade.get_symbol_by_name("unique_symbol_name");
        assert!(sym.is_some(), "expected to find unique_symbol_name");
        assert_eq!(sym.unwrap().as_name(), "unique_symbol_name");
    }

    #[test]
    fn test_facade_get_symbol_by_id() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("lib.rs"), "pub fn my_func() {}").unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(idx_dir.path().join("idx")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let sym = facade.get_symbol_by_name("my_func").unwrap();
        let by_id = facade.get_symbol(sym.id);
        assert!(by_id.is_some());
        assert_eq!(by_id.unwrap().as_name(), "my_func");
    }

    #[test]
    fn test_facade_find_symbols_by_name() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn shared_name() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn shared_name() {}").unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(idx_dir.path().join("idx")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let syms = facade.find_symbols_by_name("shared_name");
        assert_eq!(syms.len(), 2, "expected 2 symbols named shared_name");
    }

    #[test]
    fn test_facade_search_symbols() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(
            src_dir.path().join("lib.rs"),
            "pub fn calculate_total() {}\npub fn process_order() {}",
        )
        .unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(idx_dir.path().join("idx")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let results = facade.search_symbols("calculate", 10);
        assert!(!results.is_empty(), "expected search results for 'calculate'");
        assert!(results.iter().any(|s| s.as_name() == "calculate_total"));
    }

    #[test]
    fn test_facade_reindex() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("lib.rs"), "fn original() {}").unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(idx_dir.path().join("idx")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        assert!(facade.get_symbol_by_name("original").is_some());

        // Modify the file and reindex
        fs::write(src_dir.path().join("lib.rs"), "fn replacement() {}").unwrap();
        facade.reindex(src_dir.path()).unwrap();

        assert!(
            facade.get_symbol_by_name("original").is_none(),
            "original should be gone after reindex"
        );
        assert!(facade.get_symbol_by_name("replacement").is_some());
    }

    #[test]
    fn test_facade_relationship_counts() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(
            src_dir.path().join("main.rs"),
            "fn caller() { callee(); }\nfn callee() {}",
        )
        .unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(idx_dir.path().join("idx")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        assert!(facade.relationship_count() > 0);
    }
}
