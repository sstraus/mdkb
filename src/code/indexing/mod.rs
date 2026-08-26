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
use std::sync::OnceLock;

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
    semantic: OnceLock<Option<SemanticSearch>>,
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
            semantic: OnceLock::new(),
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
            semantic: OnceLock::new(),
        })
    }

    /// Open an existing index with a SQLite-enforced read-only connection.
    ///
    /// Unlike [`Self::open_or_create`], this never initializes, repairs,
    /// quarantines, migrates, or creates the database.
    pub fn open_read_only(db_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let db = CodeDb::open_read_only(db_path)?;
        Ok(Self {
            db,
            config: PipelineConfig::default(),
            semantic: OnceLock::new(),
        })
    }

    /// Override the default pipeline configuration.
    #[must_use]
    pub fn with_config(mut self, config: PipelineConfig) -> Self {
        self.config = config;
        self
    }

    /// Lazily initialize semantic search on first use.
    ///
    /// Loads the ONNX model (~300-800 MB RSS) only when actually needed.
    fn ensure_semantic(&self) -> Option<&SemanticSearch> {
        self.semantic
            .get_or_init(|| init_semantic(self.db.path()))
            .as_ref()
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
        self.index_scope(root, root)
    }

    /// Index the files under `scope`, keying them relative to `root`.
    ///
    /// `mdkb code index src/` must not change what a file is called in the
    /// database. Walking and keying are therefore separate: `scope` bounds the
    /// walk, `root` is the prefix every stored path is relative to. Collapse the
    /// two and the same file lands under `lib.rs` from one command and
    /// `src/lib.rs` from another, which `insert_file` cannot tell apart.
    pub fn index_scope(&mut self, root: &Path, scope: &Path) -> anyhow::Result<IndexStats> {
        let files_removed = self.prune_vanished_files(root)?;
        let indexed_mtimes = self.db.get_file_mtimes()?;

        let stats = if indexed_mtimes.is_empty() {
            // Fresh index — no dedup needed. Embed the whole symbol table.
            let stats = pipeline::index_scope(root, scope, &self.db, &self.config)?;
            self.generate_symbol_embeddings();
            stats
        } else {
            // Incremental — discover files, filter by mtime, delete stale, re-index.
            // Uses mtime comparison (filesystem metadata) to avoid reading file contents.
            let discovered = walker::discover_files(
                scope,
                &self.config.ignore_patterns,
                self.config.respect_gitignore,
            );
            let mut changed = Vec::new();

            for path in &discovered {
                let current_mtime = hasher::file_mtime(path).unwrap_or(0);

                match indexed_mtimes.get(&rel_key(path, root)) {
                    Some(&old) if old == current_mtime => {} // unchanged
                    _ => changed.push(path.clone()),
                }
            }

            // No walk-based prune pass here: `scope` may be a subdirectory, so
            // "indexed but absent from the walk" would also cover the rest of the
            // project. Deletion detection lives in `update`, whose walk is always
            // the whole tree, and in `prune_vanished_files` above, which asks the
            // filesystem instead of the walk.
            if changed.is_empty() {
                self.db.mark_index_scan_completed()?;
                return Ok(IndexStats {
                    files_discovered: discovered.len() as u32,
                    files_removed,
                    ..IndexStats::default()
                });
            }

            // Delete stale entries for changed files
            for path in &changed {
                self.delete_by_file(path, root)?;
            }

            let mut stats = pipeline::index_files(&changed, root, &self.db, &self.config)?;
            // `index_files` scopes the DISCOVER stage to `changed`, so the pipeline
            // reports files_discovered == files_indexed here. Override with the
            // true repo-wide walk count so "discovered" means the same thing on
            // every path (matches the no-change branch above).
            stats.files_discovered = discovered.len() as u32;
            // Re-embed ONLY the changed files' symbols, reusing existing vectors
            // for everything else — a 1-file change must not re-run ONNX over the
            // entire symbol table (PERF-D3).
            self.generate_symbol_embeddings_for_files(&changed, root);
            stats
        };

        self.db.mark_index_scan_completed()?;
        self.verify_sound()?;
        Ok(IndexStats {
            files_removed: stats.files_removed + files_removed,
            ..stats
        })
    }

    /// Drop every indexed file that is no longer on disk under `root`.
    ///
    /// Unlike [`Self::prune_missing_files`] this asks the filesystem rather than
    /// a walk, so it is safe when only a subdirectory is being indexed. It is
    /// also the repair for indexes written before paths were keyed from the
    /// project root: a row stored as `lib.rs` for a file living at `src/lib.rs`
    /// resolves to nothing and is dropped, instead of lingering as a duplicate
    /// of the row this run is about to write.
    fn prune_vanished_files(&mut self, root: &Path) -> anyhow::Result<u32> {
        let vanished: Vec<String> = self
            .db
            .get_file_mtimes()?
            .into_keys()
            .filter(|rel| !root.join(rel).exists())
            .collect();
        for rel in &vanished {
            self.delete_by_rel_path(rel)?;
        }
        Ok(vanished.len() as u32)
    }

    /// Probe the index for structural damage, throttled to one scan per
    /// [`crate::store::heal::CHECK_INTERVAL`].
    ///
    /// Without this nothing ever notices: this connection answers reads from its
    /// page cache and appends writes to the WAL, so a torn main file can stay
    /// invisible for as long as the process lives — and the daemon's process
    /// lives for days.
    fn verify_sound(&self) -> anyhow::Result<()> {
        crate::store::heal::verify_and_mark_throttled(self.db.path()).map_err(anyhow::Error::from)
    }

    /// Incrementally refresh the whole index for `root`.
    ///
    /// Content-hash diff over all discovered files: only changed/new files are
    /// re-parsed and re-embedded and deleted files are dropped — the index is
    /// NOT wiped. On an empty index it falls through to a full build. This is the
    /// cheap path for the `update` command: a no-change refresh reindexes zero
    /// files instead of clearing and re-parsing everything. Mirrors the
    /// SessionStart code-refresh idiom.
    pub fn update(&mut self, root: &Path) -> anyhow::Result<IndexStats> {
        // Use the Result-returning count directly: a transient DB error must NOT
        // be treated as "empty", or reindex() below would clear() a populated
        // index (DATA-D1 silent wipe). Propagate the error instead.
        if self.db.file_count()? == 0 {
            return self.reindex(root);
        }
        let all_files = walker::discover_files(
            root,
            &self.config.ignore_patterns,
            self.config.respect_gitignore,
        );
        // `reindex_files` derives deletions from the paths it is handed, and a
        // file removed from disk is absent from the walk — so only this caller,
        // which knows `all_files` is the complete tree, can drop it.
        let files_removed = self.prune_missing_files(root, &all_files)?;
        let mut stats = self.reindex_files(root, &all_files)?;
        stats.files_removed = files_removed;
        self.verify_sound()?;
        Ok(stats)
    }

    /// Re-index a directory (full reindex, discarding previous data).
    pub fn reindex(&mut self, root: &Path) -> anyhow::Result<IndexStats> {
        self.reindex_scope(root, root)
    }

    /// Full reindex of `scope`, keyed relative to `root`. See [`Self::index_scope`].
    pub fn reindex_scope(&mut self, root: &Path, scope: &Path) -> anyhow::Result<IndexStats> {
        // Roll back any dangling transaction from a previous failed reindex.
        let _ = self.db.conn().execute_batch("ROLLBACK");
        self.db.clear()?;
        // Only clear semantic if already initialized (don't trigger lazy load for a clear)
        if let Some(Some(semantic)) = self.semantic.get() {
            if let Err(e) = semantic.clear() {
                tracing::error!(
                    "Failed to clear semantic index: {e}. Impact: old embeddings may persist."
                );
            }
        }

        self.index_scope(root, scope)
    }

    /// Index specific files (not a full directory walk).
    ///
    /// Like `index_directory` but takes explicit file paths instead of walking.
    pub fn index_files(&mut self, root: &Path, paths: &[PathBuf]) -> anyhow::Result<IndexStats> {
        let stats = pipeline::index_files(paths, root, &self.db, &self.config)?;
        self.generate_symbol_embeddings_for_files(paths, root);
        self.db.mark_index_scan_completed()?;
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
        self.delete_by_rel_path(&rel_key(file_path, root))
    }

    /// Delete an indexed file by the relative path key stored in the database.
    fn delete_by_rel_path(&mut self, rel_path: &str) -> anyhow::Result<()> {
        // Collect symbol IDs before deleting (for embedding cleanup)
        let symbol_ids = self.get_symbol_ids_for_path(rel_path);

        self.db.delete_by_file(rel_path)?;

        // Only remove embeddings if semantic is already initialized;
        // avoid triggering lazy model load (~300-800 MB) for a delete operation.
        if let Some(Some(semantic)) = self.semantic.get() {
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

    /// Drop every indexed file that is absent from a COMPLETE walk of `root`.
    ///
    /// Returns the number of files removed. Deletion detection cannot live in
    /// [`Self::reindex_files`]: that function inspects only the paths it is
    /// given, and a file deleted from disk never appears in a filesystem walk,
    /// so its symbols and relationships survived every incremental update and
    /// kept answering searches until a full reindex. Only a caller holding the
    /// whole walk can tell "deleted" from "not in this batch", so `walk` must be
    /// the full discovery result — passing a subset deletes the rest of the index.
    fn prune_missing_files(&mut self, root: &Path, walk: &[PathBuf]) -> anyhow::Result<u32> {
        let present: HashSet<String> = walk.iter().map(|p| rel_key(p, root)).collect();
        let stale: Vec<String> = self
            .db
            .get_file_hashes()?
            .into_keys()
            .filter(|indexed| !present.contains(indexed))
            .collect();

        for rel_path in &stale {
            self.delete_by_rel_path(rel_path)?;
        }

        if !stale.is_empty() {
            tracing::info!("Pruned {} file(s) deleted from disk", stale.len());
        }
        Ok(stale.len() as u32)
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
            let rel_key = rel_key(path, root);

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
            self.db.mark_index_scan_completed()?;
            return Ok(IndexStats {
                files_discovered: paths.len() as u32,
                ..IndexStats::default()
            });
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
            self.db.mark_index_scan_completed()?;
            return Ok(IndexStats {
                files_discovered: paths.len() as u32,
                ..IndexStats::default()
            });
        }

        let mut stats = self.index_files(root, &changed)?;
        // `index_files` scopes DISCOVER to `changed`, so it reports
        // files_discovered == files_indexed. Override with the true repo-wide
        // count (`paths`, the full walk `update()` passed in) so "discovered"
        // means the same thing regardless of how much of the repo changed.
        stats.files_discovered = paths.len() as u32;
        // The watcher drives this path constantly; the probe's own throttle is
        // what keeps that affordable.
        self.verify_sound()?;
        Ok(stats)
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

    /// Find all symbols in files matching a path substring.
    pub fn find_symbols_by_file(&self, file_pattern: &str, limit: usize) -> Vec<Symbol> {
        self.db
            .find_symbols_by_file(file_pattern, limit)
            .unwrap_or_else(|e| {
                tracing::error!("DB error in find_symbols_by_file('{file_pattern}'): {e}");
                Vec::new()
            })
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

    /// Symbol lookup with name, kind and file filters applied in SQL.
    ///
    /// Returns the capped rows and the total number of matches before the cap.
    pub fn query_symbols(
        &self,
        name: crate::code::storage::NameMatch<'_>,
        kind: Option<&str>,
        file_pattern: Option<&str>,
        limit: usize,
    ) -> (Vec<Symbol>, usize) {
        self.db
            .query_symbols(name, kind, file_pattern, limit)
            .unwrap_or_else(|e| {
                tracing::error!("DB error in query_symbols: {e}");
                (Vec::new(), 0)
            })
    }

    /// Look up file token estimates for a set of relative paths.
    pub fn get_file_token_estimates(&self, rel_paths: &[String]) -> HashMap<String, u32> {
        self.db
            .get_file_token_estimates(rel_paths)
            .unwrap_or_else(|e| {
                tracing::error!("DB error in get_file_token_estimates: {e}");
                HashMap::new()
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
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
    ) -> anyhow::Result<Vec<(Symbol, f32)>> {
        let Some(semantic) = self.ensure_semantic() else {
            return Ok(Vec::new());
        };

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
    pub fn has_semantic_search(&self) -> bool {
        self.ensure_semantic().is_some()
    }

    /// Number of stored semantic embeddings.
    pub fn semantic_count(&self) -> usize {
        self.semantic
            .get()
            .and_then(|opt| opt.as_ref())
            .and_then(|s| s.count().ok())
            .unwrap_or(0)
    }

    // -----------------------------------------------------------------------
    // Statistics
    // -----------------------------------------------------------------------

    /// Total number of indexed symbols. Logs and reports 0 on a DB error, so a
    /// query failure is visible rather than silently indistinguishable from an
    /// empty index (callers that must not conflate the two use `db` directly).
    pub fn symbol_count(&self) -> u64 {
        self.db.symbol_count().unwrap_or_else(|e| {
            tracing::error!("symbol_count query failed: {e}; reporting 0");
            0
        })
    }

    /// Total number of indexed files. Logs and reports 0 on a DB error (see
    /// `symbol_count`); `update()` uses `db.file_count()` directly so it never
    /// mistakes a failed count for an empty index.
    pub fn file_count(&self) -> u64 {
        self.db.file_count().unwrap_or_else(|e| {
            tracing::error!("file_count query failed: {e}; reporting 0");
            0
        })
    }

    /// Total number of persisted relationships. Logs and reports 0 on a DB error.
    pub fn relationship_count(&self) -> usize {
        self.db.relationship_count().unwrap_or_else(|e| {
            tracing::error!("relationship_count query failed: {e}; reporting 0");
            0
        }) as usize
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Generate embeddings only for symbols in the given files.
    fn generate_symbol_embeddings_for_files(&self, paths: &[PathBuf], root: &Path) {
        let Some(semantic) = self.ensure_semantic() else {
            return;
        };

        let rel_paths: HashSet<String> = paths
            .iter()
            .filter_map(|p| {
                p.strip_prefix(root)
                    .ok()
                    .map(|r| r.to_string_lossy().to_string())
            })
            .collect();

        let rel_path_strs: Vec<&str> = rel_paths.iter().map(|s| s.as_str()).collect();
        let symbols: Vec<Symbol> = self
            .db
            .symbols_for_files(&rel_path_strs)
            .unwrap_or_default();

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
    fn generate_symbol_embeddings(&self) {
        let Some(semantic) = self.ensure_semantic() else {
            return;
        };

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

/// The database key for an indexed file: its path relative to the index root.
///
/// A path outside `root` keeps its full spelling, matching how the COLLECT
/// stage registers files — the lookup key must be built the same way everywhere
/// or a file is silently seen as new (and, in a prune pass, as deleted).
fn rel_key(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

/// Run a code-index mutation on a long-lived facade slot, CLOSING it if the
/// database turns out to be corrupt.
///
/// The mirror of `handlers::run_mutation` for `code.sqlite`, and it exists for
/// the same reason: [`CodeDb::open`] quarantines a corrupt file, but only ever
/// at open, and it declines while any connection is live — so a daemon that
/// keeps its facade forever keeps its own corruption forever.
///
/// The signal is different, though, and deliberately so: the index database can
/// be gigabytes, so probing it after every watcher-driven reindex would cost a
/// full-file scan each time. There is no primary data here (every symbol
/// re-derives from source), so this reacts to SQLite reporting a torn file
/// during the mutation instead of hunting for damage that may never be read.
pub fn run_code_mutation<T>(
    slot: &mut Option<IndexFacade>,
    what: &str,
    f: impl FnOnce(&mut IndexFacade) -> anyhow::Result<T>,
) -> Option<anyhow::Result<T>> {
    let result = f(slot.as_mut()?);

    if let Err(e) = &result {
        if is_corruption(e) {
            tracing::error!(
                operation = what,
                error = %e,
                "code index is corrupt — closing this connection so the next open can quarantine and rebuild it"
            );
            *slot = None;
        }
    }
    Some(result)
}

/// True when an error from the code-index pipeline means the database file
/// itself is torn, rather than the operation being wrong.
///
/// The pipeline erases types into `anyhow`, so the chain is searched for either
/// SQLite's own corruption codes or an mdkb error that already classified it.
fn is_corruption(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(e) = cause.downcast_ref::<crate::error::Error>() {
            return e.is_index_corrupt();
        }
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                )
        )
    })
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

        // Semantic should NOT be initialized on construction (lazy via OnceLock)
        assert!(
            facade.semantic.get().is_none(),
            "semantic OnceLock should be uninitialized on create"
        );
    }

    #[test]
    fn test_facade_open_or_create_does_not_load_model() {
        let dir = tempfile::tempdir().unwrap();
        let facade = IndexFacade::open_or_create(dir.path().join("code.sqlite")).unwrap();

        assert!(
            facade.semantic.get().is_none(),
            "semantic OnceLock should be uninitialized on open_or_create"
        );
    }

    #[test]
    fn test_facade_read_only_requires_an_existing_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        assert!(IndexFacade::open_read_only(&path).is_err());
        assert!(!path.exists());
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

    /// A project tree with one file at `src/lib.rs`, plus an open facade.
    fn project_with_a_source_subdir() -> (tempfile::TempDir, tempfile::TempDir, IndexFacade) {
        let src_dir = tempfile::tempdir().unwrap();
        fs::create_dir(src_dir.path().join("src")).unwrap();
        fs::write(src_dir.path().join("src/lib.rs"), "pub fn hello() {}").unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        (src_dir, db_dir, facade)
    }

    const IMPORTS_RS: &str = "use std::fmt;\n\
                              use std::io::Read as R;\n\
                              use std::collections::*;\n\
                              pub fn go() {}\n";

    /// Every stored import row, as `(path, alias, is_glob)`, ordered by path.
    fn import_rows(facade: &IndexFacade) -> Vec<(String, Option<String>, bool)> {
        let mut stmt = facade
            .db
            .conn()
            .prepare("SELECT path, alias, is_glob FROM code_imports ORDER BY path")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn imports_are_stored_with_their_alias_and_glob_flag() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("lib.rs"), IMPORTS_RS).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();

        facade.index_directory(src_dir.path()).unwrap();

        assert_eq!(
            import_rows(&facade),
            vec![
                ("std::collections".to_string(), None, true),
                ("std::fmt".to_string(), None, false),
                ("std::io::Read".to_string(), Some("R".to_string()), false),
            ]
        );
    }

    #[test]
    fn reindexing_a_file_does_not_duplicate_import_rows() {
        let src_dir = tempfile::tempdir().unwrap();
        let path = src_dir.path().join("lib.rs");
        fs::write(&path, IMPORTS_RS).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();

        facade.index_directory(src_dir.path()).unwrap();
        let first = import_rows(&facade);
        // Touch the body so the mtime check sees a change and re-parses.
        fs::write(&path, format!("{IMPORTS_RS}pub fn also() {{}}\n")).unwrap();
        facade.reindex_files(src_dir.path(), &[path]).unwrap();

        assert_eq!(import_rows(&facade), first);
    }

    #[test]
    fn indexing_a_subdirectory_keys_files_from_the_project_root() {
        let (src_dir, _db_dir, mut facade) = project_with_a_source_subdir();
        let root = src_dir.path();

        facade.index_scope(root, &root.join("src")).unwrap();

        let keys: Vec<String> = facade.db.get_file_mtimes().unwrap().into_keys().collect();
        assert_eq!(keys, vec!["src/lib.rs".to_string()]);
    }

    #[test]
    fn indexing_a_subdirectory_then_the_whole_tree_leaves_one_row_per_file() {
        let (src_dir, _db_dir, mut facade) = project_with_a_source_subdir();
        let root = src_dir.path();

        facade.index_scope(root, &root.join("src")).unwrap();
        facade.index_directory(root).unwrap();

        assert_eq!(facade.file_count(), 1);
    }

    #[test]
    fn a_symbol_indexed_through_a_subdirectory_is_found_by_its_root_relative_path() {
        let (src_dir, _db_dir, mut facade) = project_with_a_source_subdir();
        let root = src_dir.path();

        facade.index_scope(root, &root.join("src")).unwrap();

        let found = facade.find_symbols_by_file("src/lib.rs", 10);
        assert!(
            found.iter().any(|s| s.name.as_ref() == "hello"),
            "{found:?}"
        );
    }

    #[test]
    fn an_index_row_whose_file_no_longer_exists_is_dropped() {
        let (src_dir, _db_dir, mut facade) = project_with_a_source_subdir();
        let root = src_dir.path();
        facade.index_directory(root).unwrap();
        assert_eq!(facade.file_count(), 1);

        fs::remove_file(root.join("src/lib.rs")).unwrap();
        facade.index_directory(root).unwrap();

        assert_eq!(facade.file_count(), 0);
    }

    #[test]
    fn test_index_directory_no_changes_marks_scan_completed() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("main.rs"), "pub fn hello() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        facade
            .db
            .conn()
            .execute(
                "UPDATE code_metadata SET value='1' WHERE key=?1",
                [crate::code::storage::schema::LAST_INDEX_SCAN_KEY],
            )
            .unwrap();

        let stats = facade.index_directory(src_dir.path()).unwrap();

        assert_eq!(stats.files_indexed, 0);
        let scan_at = facade.db.last_index_scan_at().unwrap().unwrap();
        assert!(
            scan_at > 1,
            "no-op index scan must refresh scan marker, got {scan_at}"
        );
    }

    #[test]
    fn test_reindex_files_no_changes_marks_scan_completed() {
        let src_dir = tempfile::tempdir().unwrap();
        let path = src_dir.path().join("main.rs");
        fs::write(&path, "pub fn hello() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        facade
            .db
            .conn()
            .execute(
                "UPDATE code_metadata SET value='1' WHERE key=?1",
                [crate::code::storage::schema::LAST_INDEX_SCAN_KEY],
            )
            .unwrap();

        let stats = facade.reindex_files(src_dir.path(), &[path]).unwrap();

        assert_eq!(stats.files_indexed, 0);
        let scan_at = facade.db.last_index_scan_at().unwrap().unwrap();
        assert!(
            scan_at > 1,
            "no-op reindex_files must refresh scan marker, got {scan_at}"
        );
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
        for hash in hashes.values() {
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

    #[test]
    fn update_is_noop_when_nothing_changed() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        assert_eq!(facade.symbol_count(), 2);

        // No file changed → update must reindex zero files AND keep the index.
        let stats = facade.update(src_dir.path()).unwrap();
        assert_eq!(
            stats.files_indexed, 0,
            "no-change update must not re-parse any file (was a full wipe+rebuild)"
        );
        assert_eq!(
            facade.symbol_count(),
            2,
            "no-change update must not clear the index"
        );
        assert!(facade.get_symbol_by_name("aaa").is_some());
        assert!(facade.get_symbol_by_name("bbb").is_some());
    }

    #[test]
    fn update_does_not_wipe_index_when_file_count_fails() {
        // DATA-D1: a transient DB error on the file count must NOT be read as
        // "empty index" and trigger reindex()->clear(). We simulate the error by
        // renaming code_files (so `SELECT COUNT(*) FROM code_files` fails) while
        // leaving code_symbols intact, then assert update() surfaces the error
        // and the symbols are still there.
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        let symbols_before = facade.db.symbol_count().unwrap();
        assert!(symbols_before > 0);

        facade
            .db
            .conn()
            .execute_batch("ALTER TABLE code_files RENAME TO code_files_tmp")
            .unwrap();

        let res = facade.update(src_dir.path());
        assert!(
            res.is_err(),
            "update must surface the count error, not silently wipe + rebuild"
        );

        facade
            .db
            .conn()
            .execute_batch("ALTER TABLE code_files_tmp RENAME TO code_files")
            .unwrap();
        assert_eq!(
            facade.db.symbol_count().unwrap(),
            symbols_before,
            "index must NOT be wiped on a transient count error (clear() must not run)"
        );
    }

    #[test]
    fn update_reindexes_only_changed_file() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        // Change only b.rs.
        fs::write(src_dir.path().join("b.rs"), "pub fn ccc() {}").unwrap();
        let stats = facade.update(src_dir.path()).unwrap();
        assert_eq!(stats.files_indexed, 1, "only the changed file is reindexed");
        assert_eq!(
            stats.files_discovered, 2,
            "files_discovered must report the full repo walk (2 files), not just \
             the 1 changed file the pipeline re-processed"
        );
        assert!(facade.get_symbol_by_name("aaa").is_some());
        assert!(facade.get_symbol_by_name("bbb").is_none());
        assert!(facade.get_symbol_by_name("ccc").is_some());
    }

    #[test]
    fn update_drops_files_deleted_from_disk() {
        // A file removed from disk is absent from the walk, so the changed/deleted
        // diff over the walk can never see it: without an explicit prune its
        // symbols answered searches forever (only a full reindex cleared them).
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        assert_eq!(facade.file_count(), 2);

        fs::remove_file(src_dir.path().join("a.rs")).unwrap();

        let stats = facade.update(src_dir.path()).unwrap();
        assert_eq!(stats.files_removed, 1, "the deleted file must be reported");
        assert_eq!(stats.files_indexed, 0, "a pure deletion re-parses nothing");
        assert!(
            facade.get_symbol_by_name("aaa").is_none(),
            "symbols of a deleted file must not survive the update"
        );
        assert!(facade.get_symbol_by_name("bbb").is_some());
        assert_eq!(facade.file_count(), 1);
    }

    #[test]
    fn update_after_deletion_reports_nothing_removed_on_the_next_run() {
        // The prune must be idempotent: a second update finds nothing stale, so
        // a steady-state run stays quiet instead of re-reporting the deletion.
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        fs::remove_file(src_dir.path().join("a.rs")).unwrap();
        facade.update(src_dir.path()).unwrap();

        let stats = facade.update(src_dir.path()).unwrap();
        assert_eq!(stats.files_removed, 0);
        assert_eq!(facade.file_count(), 1);
    }

    #[test]
    fn reindex_files_with_a_subset_prunes_nothing() {
        // `reindex_files` is also driven by the watcher with a handful of changed
        // paths. Deletion detection must stay out of it: pruning "indexed but not
        // in this batch" there would drop the whole rest of the index.
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        fs::write(src_dir.path().join("b.rs"), "pub fn ccc() {}").unwrap();
        let stats = facade
            .reindex_files(src_dir.path(), &[src_dir.path().join("b.rs")])
            .unwrap();

        assert_eq!(stats.files_removed, 0);
        assert!(
            facade.get_symbol_by_name("aaa").is_some(),
            "a file outside the batch must not be treated as deleted"
        );
        assert!(facade.get_symbol_by_name("ccc").is_some());
    }

    #[test]
    #[ignore = "requires ONNX model download (see tests/e2e_llm.rs convention)"]
    fn incremental_update_reembeds_only_changed_symbols() {
        // PERF-D3: an incremental update of one file must re-embed only that
        // file's symbols, reusing the stored vectors for everything else. We
        // plant a sentinel embedding on an UNCHANGED symbol; if the buggy full
        // re-embed path ran, it would overwrite the sentinel with a fresh vector.
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();

        let bbb_id = facade
            .find_symbols_by_name("bbb")
            .first()
            .expect("bbb indexed")
            .id
            .value();

        // Overwrite the store so bbb's vector is a recognisable sentinel, keeping
        // every other symbol's real embedding.
        {
            let semantic = facade.ensure_semantic().expect("semantic (model present)");
            let mut all = semantic.store_load_filtered(|_| true);
            for (id, vec) in &mut all {
                if *id == bbb_id {
                    for value in vec.iter_mut() {
                        *value = 0.5;
                    }
                }
            }
            semantic
                .generate_embeddings_incremental(&all, &[])
                .expect("seed sentinel");
        }

        // Change only a.rs — bbb is untouched.
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa_v2() {}").unwrap();
        facade.update(src_dir.path()).unwrap();

        let bbb_after = facade
            .ensure_semantic()
            .unwrap()
            .store_load_filtered(|id| id == bbb_id);
        assert_eq!(bbb_after.len(), 1, "bbb embedding must still be present");
        assert!(
            bbb_after[0]
                .1
                .iter()
                .all(|&v| (v - 0.5).abs() < f32::EPSILON),
            "bbb's sentinel embedding must be preserved — a 1-file change must not \
             re-embed unchanged symbols"
        );
        // The changed symbol is freshly embedded and searchable.
        assert!(facade.get_symbol_by_name("aaa_v2").is_some());
    }

    #[test]
    fn update_builds_from_empty() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        // Empty index → update falls through to a full build.
        let stats = facade.update(src_dir.path()).unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert!(facade.get_symbol_by_name("aaa").is_some());
    }

    /// Re-indexing a file whose row already exists (i.e. a delete-miss, the live
    /// daemon condition behind the ~149 "Code reindex failed: FOREIGN KEY
    /// constraint failed" logs) must not corrupt file ids. `index_files` does not
    /// delete first, so the second pass exercises `insert_file`'s upsert path: a
    /// stale `last_insert_rowid()` would hand symbols/relationships a `file_id`
    /// with no matching `code_files` row and raise a FK violation.
    #[test]
    fn reindex_file_with_relationships_does_not_violate_fk() {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join("a.rs"), "pub fn helper() {}").unwrap();
        fs::write(src_dir.path().join("b.rs"), "pub fn caller() { helper(); }").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        assert!(facade.get_symbol_by_name("caller").is_some());
        let rels_before = facade.relationship_count();
        assert!(rels_before > 0, "caller() -> helper() should be recorded");

        // Re-index b.rs without deleting its existing row first (delete-miss).
        let b_path = src_dir.path().join("b.rs");
        let result = facade.index_files(src_dir.path(), &[b_path]);
        assert!(
            result.is_ok(),
            "re-index over an existing file row must not raise FK violation: {:?}",
            result.err()
        );

        // Integrity preserved: no duplicate symbols, relationship intact.
        assert!(facade.get_symbol_by_name("caller").is_some());
        assert!(facade.get_symbol_by_name("helper").is_some());
        assert_eq!(
            facade.symbol_count(),
            2,
            "re-index must not duplicate symbols"
        );
        assert!(
            facade.relationship_count() > 0,
            "caller() -> helper() relationship must survive re-index"
        );
    }

    /// Every stored relationship, as `(from_name, to_name, kind)`.
    fn relationship_rows(facade: &IndexFacade) -> Vec<(String, String, String)> {
        let mut stmt = facade
            .db
            .conn()
            .prepare(
                "SELECT from_name, to_name, kind FROM code_relationships ORDER BY kind, to_name",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// Index one file of the given name and return its relationship rows.
    fn relationships_for(name: &str, source: &str) -> Vec<(String, String, String)> {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join(name), source).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        relationship_rows(&facade)
    }

    /// Index one file and return `(symbol name, scope_context JSON)` for every
    /// symbol stored as a member of a type.
    fn class_members_for(name: &str, source: &str) -> Vec<(String, String)> {
        let src_dir = tempfile::tempdir().unwrap();
        fs::write(src_dir.path().join(name), source).unwrap();
        let db_dir = tempfile::tempdir().unwrap();
        let mut facade = IndexFacade::create(db_dir.path().join("code.sqlite")).unwrap();
        facade.index_directory(src_dir.path()).unwrap();
        let mut stmt = facade
            .db
            .conn()
            .prepare(
                "SELECT name, scope_context FROM code_symbols \
                 WHERE scope_context LIKE '%ClassMember%' ORDER BY name",
            )
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// A member has to record which type it belongs to. Stored without the
    /// owner, `open` is a method of nothing: no caller can tell `Store::open`
    /// from `Config::open`, which is the whole reason two symbols share a name.
    ///
    /// Rust states ownership three different ways — an inherent `impl`, a trait
    /// `impl` and the trait's own body — and all three have to answer.
    #[test]
    fn rust_members_record_the_type_they_belong_to() {
        let members = class_members_for(
            "lib.rs",
            "pub struct Store { pub path: String }\n\
             pub trait Open { fn open(&self); }\n\
             impl Store { pub fn load(&self) {} }\n\
             impl Open for Store { fn open(&self) {} }\n",
        );

        for (name, owner) in [("path", "Store"), ("load", "Store"), ("open", "Store")] {
            assert!(
                members
                    .iter()
                    .any(|(n, scope)| n == name && scope.contains(owner)),
                "{name} must be recorded as a member of {owner}: {members:?}"
            );
        }
        // The trait's own requirement belongs to the trait, not to any impl.
        assert!(
            members
                .iter()
                .any(|(n, scope)| n == "open" && scope.contains("Open")),
            "the trait requirement must be recorded as a member of Open: {members:?}"
        );
    }

    /// Same requirement, a language whose members are found by a different
    /// parser: this is shared context tracking, not a Rust detail.
    #[test]
    fn python_methods_record_the_class_they_belong_to() {
        let members = class_members_for(
            "app.py",
            "class Store:\n    def load(self):\n        pass\n\nclass Cache:\n    def load(self):\n        pass\n",
        );

        // The Python parser already qualifies the symbol name; the scope has to
        // agree with it rather than leave the owner to be parsed back out.
        for (name, owner) in [("Store.load", "Store"), ("Cache.load", "Cache")] {
            assert!(
                members
                    .iter()
                    .any(|(n, scope)| n == name && scope.contains(owner)),
                "{name} must be recorded as a member of {owner}: {members:?}"
            );
        }
    }

    /// A Godot script declares its base class on the first line. The hierarchy of
    /// a whole project is missing from the index if that never reaches storage.
    #[test]
    fn gdscript_extends_reaches_the_stored_relationships() {
        let rows = relationships_for(
            "player.gd",
            "extends Node2D\nclass_name Player\n\nclass Inner extends Resource:\n\tvar x: int\n",
        );

        assert!(
            rows.contains(&("Player".into(), "Node2D".into(), "Extends".into())),
            "file-level extends: {rows:?}"
        );
        assert!(
            rows.contains(&("Inner".into(), "Resource".into(), "Extends".into())),
            "inner class extends: {rows:?}"
        );
    }

    /// A script with no `class_name` is still a class and still has a base; the
    /// per-file `<module>` symbol is what names it.
    #[test]
    fn gdscript_extends_is_recorded_for_an_unnamed_script() {
        let rows = relationships_for("enemy.gd", "extends Node\n\nfunc _ready():\n\tpass\n");

        assert!(
            rows.contains(&("<module>".into(), "Node".into(), "Extends".into())),
            "unnamed script extends: {rows:?}"
        );
    }

    #[test]
    fn php_inheritance_and_type_uses_reach_the_stored_relationships() {
        let rows = relationships_for(
            "Repo.php",
            "<?php\n\
             class Repo extends BaseRepo implements Countable, JsonSerializable {\n\
                 private Logger $logger;\n\
                 public ?Cache $cache = null;\n\
                 public function find(int $id, Query $q): Entity { return null; }\n\
             }\n",
        );

        assert!(
            rows.contains(&("Repo".into(), "BaseRepo".into(), "Extends".into())),
            "extends: {rows:?}"
        );
        for iface in ["Countable", "JsonSerializable"] {
            assert!(
                rows.contains(&("Repo".into(), iface.into(), "Implements".into())),
                "implements {iface}: {rows:?}"
            );
        }
        for used in ["Logger", "Cache", "Query", "Entity"] {
            assert!(
                rows.iter()
                    .any(|(_, to, kind)| to == used && kind == "Uses"),
                "uses {used}: {rows:?}"
            );
        }
        // `int` is a builtin, not a symbol anything can resolve to.
        assert!(
            !rows.iter().any(|(_, to, _)| to == "int"),
            "a primitive type is not a used symbol: {rows:?}"
        );
    }

    /// Lua has no classes, interfaces or type annotations, so calls are the only
    /// relationship it can produce. This pins that they still reach storage.
    #[test]
    fn lua_calls_reach_the_stored_relationships() {
        let rows = relationships_for(
            "main.lua",
            "function helper() end\nfunction caller() helper() end\n",
        );

        assert!(
            rows.contains(&("caller".into(), "helper".into(), "Calls".into())),
            "lua call: {rows:?}"
        );
    }
}
