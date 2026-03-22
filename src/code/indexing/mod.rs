//! Multi-stage parallel indexing pipeline for code intelligence.
//!
//! Discovers, reads, parses, collects, and indexes source code files
//! using crossbeam channels for stage-to-stage communication.
//!
//! The main entry point is [`IndexFacade`], which wraps the pipeline and
//! provides query methods over the SQLite index.

pub mod hasher;
pub mod pipeline;
pub mod types;
pub mod walker;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::code::semantic::{SemanticSearch, format_symbol_text};
use crate::code::storage::CodeDb;
use crate::code::symbol::Symbol;
use crate::code::types::SymbolId;

use self::pipeline::PipelineConfig;
use self::types::IndexStats;

/// Facade over the indexing pipeline and SQLite queries.
///
/// Owns the code database and provides both mutation (indexing) and
/// query (search, graph traversal) operations.
#[derive(Debug)]
pub struct IndexFacade {
    db: CodeDb,
    config: PipelineConfig,
    semantic: Option<SemanticSearch>,
    /// false = not yet attempted to initialize semantic search.
    semantic_initialized: bool,
}

impl IndexFacade {
    /// Create a new facade with a database at the given path.
    ///
    /// The ONNX embedding model is NOT loaded until first use
    /// (lazy initialization to save ~300-800 MB per idle instance).
    pub fn create(db_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let db = CodeDb::create(db_path)?;
        Ok(Self {
            db,
            config: PipelineConfig::default(),
            semantic: None,
            semantic_initialized: false,
        })
    }

    /// Open an existing database, or create one if it doesn't exist.
    ///
    /// The ONNX embedding model is NOT loaded until first use.
    pub fn open_or_create(db_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let db = CodeDb::open_or_create(db_path)?;
        Ok(Self {
            db,
            config: PipelineConfig::default(),
            semantic: None,
            semantic_initialized: false,
        })
    }

    /// Override the default pipeline configuration.
    pub fn with_config(mut self, config: PipelineConfig) -> Self {
        self.config = config;
        self
    }

    /// Lazily initialize semantic search on first use.
    ///
    /// Loads the ONNX model (~300-800 MB RSS) only when actually needed.
    fn ensure_semantic(&mut self) -> Option<&SemanticSearch> {
        if !self.semantic_initialized {
            self.semantic_initialized = true;
            self.semantic = init_semantic(self.db.path());
        }
        self.semantic.as_ref()
    }

    /// Get a reference to the underlying database.
    pub fn db(&self) -> &CodeDb {
        &self.db
    }

    // -----------------------------------------------------------------------
    // Indexing operations
    // -----------------------------------------------------------------------

    /// Index all source files under the given directory.
    ///
    /// Uses content hashes to skip unchanged files and deletes stale entries
    /// for changed files before re-indexing, preventing duplicates.
    pub fn index_directory(&mut self, root: &Path) -> anyhow::Result<IndexStats> {
        let indexed_mtimes = self.db.get_file_mtimes()?;

        let stats = if indexed_mtimes.is_empty() {
            // Fresh index — no dedup needed
            pipeline::index_directory(root, &self.db, &self.config)?
        } else {
            // Incremental — discover files, filter by mtime, delete stale, re-index.
            // Uses mtime comparison (filesystem metadata) to avoid reading file contents.
            let discovered = walker::discover_files(root, &self.config.ignore_patterns);
            let mut changed = Vec::new();

            for path in &discovered {
                let rel_key = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                let current_mtime = hasher::file_mtime(path).unwrap_or(0);

                match indexed_mtimes.get(&rel_key) {
                    Some(&old) if old == current_mtime => {} // unchanged
                    _ => changed.push(path.clone()),
                }
            }

            if changed.is_empty() {
                return Ok(IndexStats {
                    files_discovered: discovered.len() as u32,
                    ..IndexStats::default()
                });
            }

            // Delete stale entries for changed files
            for path in &changed {
                self.delete_by_file(path, root)?;
            }

            pipeline::index_files(&changed, root, &self.db, &self.config)?
        };

        self.generate_symbol_embeddings();
        Ok(stats)
    }

    /// Re-index a directory (full reindex, discarding previous data).
    pub fn reindex(&mut self, root: &Path) -> anyhow::Result<IndexStats> {
        // Roll back any dangling transaction from a previous failed reindex.
        let _ = self.db.conn().execute_batch("ROLLBACK");
        self.db.clear()?;
        // Only clear semantic if already initialized (don't trigger lazy load for a clear)
        if let Some(ref semantic) = self.semantic {
            if let Err(e) = semantic.clear() {
                tracing::error!(
                    "Failed to clear semantic index: {e}. Impact: old embeddings may persist."
                );
            }
        }

        self.index_directory(root)
    }

    /// Index specific files (not a full directory walk).
    ///
    /// Like `index_directory` but takes explicit file paths instead of walking.
    pub fn index_files(&mut self, root: &Path, paths: &[PathBuf]) -> anyhow::Result<IndexStats> {
        let stats = pipeline::index_files(paths, root, &self.db, &self.config)?;
        self.generate_symbol_embeddings_for_files(paths, root);
        Ok(stats)
    }

    /// Get a map of relative file path → content hash for all indexed files.
    pub fn get_indexed_file_hashes(&self) -> HashMap<String, String> {
        self.db.get_file_hashes().unwrap_or_else(|e| {
            tracing::error!("Failed to read file hashes: {e}. All files will be treated as new.");
            HashMap::new()
        })
    }

    /// Delete a file and all its symbols/relationships from the index.
    ///
    /// `file_path` is an absolute path; `root` is used to derive the relative
    /// path key stored in the database.
    pub fn delete_by_file(&mut self, file_path: &Path, root: &Path) -> anyhow::Result<()> {
        let rel_path = file_path
            .strip_prefix(root)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string();

        // Collect symbol IDs before deleting (for embedding cleanup)
        let symbol_ids = self.get_symbol_ids_for_path(&rel_path);

        self.db.delete_by_file(&rel_path)?;

        // Only remove embeddings if semantic is already initialized;
        // avoid triggering lazy model load (~300-800 MB) for a delete operation.
        if let Some(ref semantic) = self.semantic {
            if let Err(e) = semantic.remove_embeddings(&symbol_ids) {
                tracing::error!(
                    error = %e,
                    symbol_count = symbol_ids.len(),
                    "Failed to remove embeddings for deleted symbols — orphaned embeddings remain in vector store"
                );
            }
        }

        Ok(())
    }

    /// Get symbol IDs for all symbols in a file (by relative path key).
    fn get_symbol_ids_for_path(&self, rel_path: &str) -> HashSet<u32> {
        match self.db.get_symbol_ids_for_file(rel_path) {
            Ok(ids) => ids.into_iter().collect(),
            Err(e) => {
                tracing::error!(
                    "Failed to load symbol IDs for {rel_path}: {e}. Embeddings may be orphaned.",
                );
                HashSet::new()
            }
        }
    }

    /// Incrementally reindex only changed files.
    ///
    /// Compares content hashes of the given paths against what's already indexed.
    /// Only re-parses and re-indexes files whose content hash has changed.
    /// Files that no longer exist on disk are removed from the index.
    pub fn reindex_files(&mut self, root: &Path, paths: &[PathBuf]) -> anyhow::Result<IndexStats> {
        let indexed_hashes = self.db.get_file_hashes()?;

        let mut changed: Vec<PathBuf> = Vec::new();
        let mut deleted: Vec<PathBuf> = Vec::new();

        for path in paths {
            let rel_key = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            if !path.exists() {
                // File deleted from disk
                if indexed_hashes.contains_key(&rel_key) {
                    deleted.push(path.clone());
                }
                continue;
            }

            let content = match std::fs::read_to_string(path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to read {}: {e}", path.display());
                    continue;
                }
            };
            let current_hash = hasher::content_hash(&content);

            match indexed_hashes.get(&rel_key) {
                Some(old_hash) if *old_hash == current_hash => {
                    // Unchanged — skip
                }
                _ => {
                    changed.push(path.clone());
                }
            }
        }

        if changed.is_empty() && deleted.is_empty() {
            return Ok(IndexStats::default());
        }

        tracing::info!(
            "Incremental reindex: {} changed, {} deleted",
            changed.len(),
            deleted.len()
        );

        // Delete old data for changed and deleted files
        for path in deleted.iter().chain(changed.iter()) {
            self.delete_by_file(path, root)?;
        }

        // Re-index changed files
        if changed.is_empty() {
            return Ok(IndexStats::default());
        }

        self.index_files(root, &changed)
    }

    // -----------------------------------------------------------------------
    // Query operations
    // -----------------------------------------------------------------------

    /// Find a symbol by exact name. Returns the first match.
    pub fn get_symbol_by_name(&self, name: &str) -> Option<Symbol> {
        self.db.find_symbols_by_name(name).ok()?.into_iter().next()
    }

    /// Find a symbol by its ID.
    pub fn get_symbol(&self, id: SymbolId) -> Option<Symbol> {
        self.db.get_symbol(i64::from(id.value())).ok()?
    }

    /// Fetch multiple symbols by ID in a single query.
    ///
    /// Returns a map of SymbolId → Symbol. IDs not found are silently omitted.
    pub fn get_symbols_batch(&self, ids: &[SymbolId]) -> HashMap<SymbolId, Symbol> {
        if ids.is_empty() {
            return HashMap::new();
        }
        let db_ids: Vec<i64> = ids.iter().map(|id| i64::from(id.value())).collect();
        let db_map = self.db.get_symbols_batch(&db_ids).unwrap_or_else(|e| {
            tracing::error!("Failed to batch-fetch {} symbols: {e}", db_ids.len());
            HashMap::new()
        });

        db_map
            .into_iter()
            .filter_map(|(db_id, sym)| {
                let sid = SymbolId::new(db_id as u32)?;
                Some((sid, sym))
            })
            .collect()
    }

    /// Find all symbols matching a name (may return multiple across files).
    pub fn find_symbols_by_name(&self, name: &str) -> Vec<Symbol> {
        self.db.find_symbols_by_name(name).unwrap_or_else(|e| {
            tracing::error!("DB error in find_symbols_by_name('{name}'): {e}");
            Vec::new()
        })
    }

    /// Search symbols by a query string (uses FTS5 trigram for partial matching).
    pub fn search_symbols(&self, query: &str, limit: usize) -> Vec<Symbol> {
        self.db.search_symbols(query, limit).unwrap_or_else(|e| {
            tracing::error!("DB error in search_symbols('{query}'): {e}");
            Vec::new()
        })
    }

    /// Get functions/methods called by the given symbol.
    pub fn get_called_functions(&self, symbol_id: SymbolId) -> Vec<Symbol> {
        self.db
            .get_called_functions(i64::from(symbol_id.value()))
            .unwrap_or_else(|e| {
                tracing::error!("DB error in get_called_functions({symbol_id:?}): {e}");
                Vec::new()
            })
    }

    /// Get functions/methods that call the given symbol.
    pub fn get_calling_functions(&self, symbol_id: SymbolId) -> Vec<Symbol> {
        let Some(symbol) = self.get_symbol(symbol_id) else {
            return Vec::new();
        };
        self.db
            .get_calling_functions(symbol.as_name())
            .unwrap_or_else(|e| {
                tracing::error!("DB error in get_calling_functions({symbol_id:?}): {e}");
                Vec::new()
            })
    }

    /// Compute the impact radius: all symbols reachable from the given one
    /// within `max_depth` hops via call relationships.
    pub fn get_impact_radius(&self, start: SymbolId, max_depth: usize) -> Vec<SymbolId> {
        let Some(symbol) = self.get_symbol(start) else {
            return Vec::new();
        };
        let impact = self
            .db
            .get_impact_radius(symbol.as_name(), max_depth as u32)
            .unwrap_or_else(|e| {
                tracing::error!("DB error in get_impact_radius({start:?}): {e}");
                Vec::new()
            });
        impact
            .into_iter()
            .map(|s| s.id)
            .filter(|&id| id != start)
            .collect()
    }

    // -----------------------------------------------------------------------
    // Semantic search
    // -----------------------------------------------------------------------

    /// Search symbols by semantic similarity to a natural language query.
    ///
    /// Returns `(Symbol, score)` pairs sorted by descending similarity.
    /// Triggers lazy initialization of the ONNX model on first call.
    pub fn semantic_search(
        &mut self,
        query: &str,
        limit: usize,
        threshold: f32,
    ) -> anyhow::Result<Vec<(Symbol, f32)>> {
        if self.ensure_semantic().is_none() {
            return Ok(Vec::new());
        }
        let semantic = self.semantic.as_ref().unwrap();

        let matches = semantic.search(query, limit, threshold)?;

        // Batch-fetch all symbols
        let ids: Vec<SymbolId> = matches
            .iter()
            .filter_map(|m| SymbolId::new(m.symbol_id))
            .collect();
        let symbol_map = self.get_symbols_batch(&ids);

        let results: Vec<(Symbol, f32)> = matches
            .into_iter()
            .filter_map(|m| {
                let id = SymbolId::new(m.symbol_id)?;
                let symbol = symbol_map.get(&id)?.clone();
                Some((symbol, m.score))
            })
            .collect();

        Ok(results)
    }

    /// Check if semantic search is available.
    pub fn has_semantic_search(&mut self) -> bool {
        self.ensure_semantic().is_some()
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
        self.db.symbol_count().unwrap_or(0)
    }

    /// Total number of indexed files.
    pub fn file_count(&self) -> u64 {
        self.db.file_count().unwrap_or(0)
    }

    /// Total number of persisted relationships.
    pub fn relationship_count(&self) -> usize {
        self.db.relationship_count().unwrap_or(0) as usize
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Generate embeddings only for symbols in the given files.
    fn generate_symbol_embeddings_for_files(&mut self, paths: &[PathBuf], root: &Path) {
        if self.ensure_semantic().is_none() {
            return;
        }
        let semantic = self.semantic.as_ref().unwrap();

        let rel_paths: HashSet<String> = paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(root)
                    .ok()
                    .map(|r| r.to_string_lossy().to_string())
            })
            .collect();

        let symbols: Vec<Symbol> = self
            .db
            .all_symbols()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| rel_paths.contains(&*s.file_path))
            .collect();

        if symbols.is_empty() {
            return;
        }

        // Load existing embeddings, filter out the ones we're regenerating,
        // then append new ones
        let changed_ids: HashSet<u32> = symbols.iter().map(|s| s.id.value()).collect();
        let existing = semantic.store_load_filtered(|id| !changed_ids.contains(&id));

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

        tracing::info!(
            "Generating semantic embeddings for {} symbols (incremental)...",
            embed_inputs.len()
        );

        if let Err(e) = semantic.generate_embeddings_incremental(&existing, &embed_inputs) {
            tracing::error!(
                "Failed to generate incremental embeddings: {e}. Impact: {} symbols may not be searchable.",
                embed_inputs.len()
            );
        }
    }

    /// Generate embeddings for all indexed symbols and write to the vector store.
    fn generate_symbol_embeddings(&mut self) {
        if self.ensure_semantic().is_none() {
            return;
        }
        let semantic = self.semantic.as_ref().unwrap();

        let symbols = self.db.all_symbols().unwrap_or_default();
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

        tracing::info!(
            "Generating semantic embeddings for {} symbols...",
            embed_inputs.len()
        );
        if let Err(e) = semantic.generate_embeddings(&embed_inputs) {
            tracing::error!(
                "Failed to generate semantic embeddings: {e}. Impact: {} symbols will not be searchable via semantic_search().",
                embed_inputs.len()
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Semantic search initialization
// ---------------------------------------------------------------------------

/// Try to initialize `SemanticSearch` at `{db_path}/../vectors.bin`.
///
/// Returns `None` if initialization fails (logged as error with impact).
/// Does NOT load the ONNX model — that happens lazily on first use.
fn init_semantic(db_path: &Path) -> Option<SemanticSearch> {
    // vectors.bin lives next to code.sqlite in the .mdkb directory
    let vectors_path = db_path.parent()?.join("vectors.bin");
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_facade_create_does_not_load_model() {
        let dir = tempfile::tempdir().unwrap();
        let facade = IndexFacade::create(dir.path().join("code.sqlite")).unwrap();

        // Semantic should NOT be initialized on construction (lazy)
        assert!(
            !facade.semantic_initialized,
            "semantic should not be initialized on create"
        );
        assert!(
            facade.semantic.is_none(),
            "semantic should be None on create"
        );
    }

    #[test]
    fn test_facade_open_or_create_does_not_load_model() {
        let dir = tempfile::tempdir().unwrap();
        let facade = IndexFacade::open_or_create(dir.path().join("code.sqlite")).unwrap();

        assert!(
            !facade.semantic_initialized,
            "semantic should not be initialized on open_or_create"
        );
        assert!(
            facade.semantic.is_none(),
            "semantic should be None on open_or_create"
        );
    }

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

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        let stats = facade.index_directory(src_dir.path()).unwrap();

        assert_eq!(stats.files_indexed, 1);
        assert!(stats.symbols_indexed >= 2);
        assert!(facade.symbol_count() >= 2);
        assert_eq!(facade.file_count(), 1);
    }

    #[test]
    fn test_facade_get_symbol_by_name() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(
            src_dir.path().join("lib.rs"),
            "pub fn unique_symbol_name() {}",
        )
        .unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let sym = facade.get_symbol_by_name("unique_symbol_name");
        assert!(sym.is_some(), "expected to find unique_symbol_name");
        assert_eq!(sym.unwrap().as_name(), "unique_symbol_name");
    }

    #[test]
    fn test_facade_get_symbol_by_id() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("lib.rs"), "pub fn my_func() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let sym = facade.get_symbol_by_name("my_func").unwrap();
        let by_id = facade.get_symbol(sym.id);
        assert!(by_id.is_some());
        assert_eq!(by_id.unwrap().as_name(), "my_func");
    }

    #[test]
    fn test_facade_get_symbols_batch() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(
            src_dir.path().join("lib.rs"),
            "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}",
        )
        .unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let a = facade.get_symbol_by_name("alpha").unwrap();
        let b = facade.get_symbol_by_name("beta").unwrap();
        let g = facade.get_symbol_by_name("gamma").unwrap();

        let batch = facade.get_symbols_batch(&[a.id, b.id, g.id]);
        assert_eq!(batch.len(), 3);
        assert_eq!(batch.get(&a.id).unwrap().as_name(), "alpha");
        assert_eq!(batch.get(&b.id).unwrap().as_name(), "beta");
        assert_eq!(batch.get(&g.id).unwrap().as_name(), "gamma");

        // Empty input returns empty map
        let empty = facade.get_symbols_batch(&[]);
        assert!(empty.is_empty());

        // Non-existent ID returns partial results
        let fake_id = SymbolId::new(99999).unwrap();
        let partial = facade.get_symbols_batch(&[a.id, fake_id]);
        assert_eq!(partial.len(), 1);
        assert_eq!(partial.get(&a.id).unwrap().as_name(), "alpha");
    }

    #[test]
    fn test_facade_find_symbols_by_name() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn shared_name() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn shared_name() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
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

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let results = facade.search_symbols("calculate", 10);
        assert!(
            !results.is_empty(),
            "expected search results for 'calculate'"
        );
        assert!(results.iter().any(|s| s.as_name() == "calculate_total"));
    }

    #[test]
    fn test_facade_reindex() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("lib.rs"), "fn original() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
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

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        assert!(
            facade.relationship_count() > 0,
            "relationships should be persisted"
        );
    }

    #[test]
    fn test_get_indexed_file_hashes() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();
        fs::write(src_dir.path().join("c.rs"), "pub fn ccc() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let hashes = facade.get_indexed_file_hashes();
        assert_eq!(
            hashes.len(),
            3,
            "expected 3 file entries, got {}",
            hashes.len()
        );

        // All values should be valid SHA-256 hex strings (64 chars)
        for (_path, hash) in &hashes {
            assert_eq!(hash.len(), 64, "hash should be 64 hex chars");
            assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn test_delete_by_file() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("keep.rs"), "pub fn keep_me() {}").unwrap();
        fs::write(src_dir.path().join("remove.rs"), "pub fn remove_me() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        assert!(facade.get_symbol_by_name("keep_me").is_some());
        assert!(facade.get_symbol_by_name("remove_me").is_some());
        assert_eq!(facade.file_count(), 2);

        // Delete the remove.rs file from the index
        let remove_path = src_dir.path().join("remove.rs");
        facade.delete_by_file(&remove_path, src_dir.path()).unwrap();

        assert!(
            facade.get_symbol_by_name("keep_me").is_some(),
            "keep_me should survive"
        );
        assert!(
            facade.get_symbol_by_name("remove_me").is_none(),
            "remove_me should be deleted"
        );
        assert_eq!(facade.file_count(), 1);
    }

    #[test]
    fn test_reindex_files_updates_modified() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("lib.rs"), "pub fn foo() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        assert!(facade.get_symbol_by_name("foo").is_some());

        // Modify the file
        fs::write(src_dir.path().join("lib.rs"), "pub fn bar() {}").unwrap();

        let paths = vec![src_dir.path().join("lib.rs")];
        let stats = facade.reindex_files(src_dir.path(), &paths).unwrap();
        assert_eq!(
            stats.files_indexed, 1,
            "should have reindexed 1 changed file"
        );

        assert!(
            facade.get_symbol_by_name("foo").is_none(),
            "foo should be gone"
        );
        assert!(
            facade.get_symbol_by_name("bar").is_some(),
            "bar should be present"
        );
    }

    #[test]
    fn test_reindex_files_skips_unchanged() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        // Reindex without changing anything
        let paths = vec![src_dir.path().join("a.rs"), src_dir.path().join("b.rs")];
        let stats = facade.reindex_files(src_dir.path(), &paths).unwrap();
        assert_eq!(
            stats.files_indexed, 0,
            "no files should be reindexed when unchanged"
        );

        // Symbols should still be queryable
        assert!(facade.get_symbol_by_name("aaa").is_some());
        assert!(facade.get_symbol_by_name("bbb").is_some());
    }

    #[test]
    fn test_reindex_files_handles_deleted() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        assert!(facade.get_symbol_by_name("aaa").is_some());
        assert!(facade.get_symbol_by_name("bbb").is_some());

        // Delete a.rs from disk, then reindex its path
        fs::remove_file(src_dir.path().join("a.rs")).unwrap();
        let paths = vec![src_dir.path().join("a.rs")];
        facade.reindex_files(src_dir.path(), &paths).unwrap();

        assert!(
            facade.get_symbol_by_name("aaa").is_none(),
            "aaa should be removed"
        );
        assert!(
            facade.get_symbol_by_name("bbb").is_some(),
            "bbb should survive"
        );
        assert_eq!(facade.file_count(), 1);
    }

    #[test]
    fn test_index_directory_twice_no_duplicates() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(
            src_dir.path().join("lib.rs"),
            "pub fn foo() {}\npub fn bar() {}",
        )
        .unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();

        let stats1 = facade.index_directory(src_dir.path()).unwrap();
        assert_eq!(stats1.symbols_indexed, 2);

        // Index again without clearing — should not duplicate
        let _stats2 = facade.index_directory(src_dir.path()).unwrap();

        // Count total symbols
        assert_eq!(
            facade.symbol_count(),
            2,
            "Should have exactly 2 symbols after double index"
        );
    }
}
