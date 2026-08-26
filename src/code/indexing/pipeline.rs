//! Five-stage parallel indexing pipeline.
//!
//! ```text
//! DISCOVER ──► READ ──► PARSE ──► COLLECT ──► INDEX
//! (walk FS)  (I/O)   (CPU)    (assign IDs)  (SQLite)
//! ```
//!
//! Stages communicate via bounded crossbeam channels.
//! DISCOVER, READ, and PARSE run on multiple threads; COLLECT and
//! INDEX are single-threaded to guarantee sequential ID assignment
//! and consistent SQLite commits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::code::indexing::hasher;
use crate::code::indexing::types::{
    CollectedImport, CollectedRelationship, FileContent, FileRegistration, IndexBatch, IndexStats,
    ParsedFile, RawImport, RawRelationship, RawSymbol,
};
use crate::code::indexing::walker;
use crate::code::parsing::c_lang::CParser;
use crate::code::parsing::cpp::CppParser;
use crate::code::parsing::csharp::CSharpParser;
use crate::code::parsing::gdscript::GdscriptParser;
use crate::code::parsing::go::GoParser;
use crate::code::parsing::java::JavaParser;
use crate::code::parsing::kotlin::KotlinParser;
use crate::code::parsing::language::Language;
use crate::code::parsing::lua::LuaParser;
use crate::code::parsing::parser::LanguageParser;
use crate::code::parsing::php::PhpParser;
use crate::code::parsing::python::PythonParser;
use crate::code::parsing::rust::RustParser;
use crate::code::parsing::swift::SwiftParser;
use crate::code::parsing::typescript::TypeScriptParser;
use crate::code::relationship::RelationKind;
use crate::code::storage::CodeDb;
use crate::code::symbol::Symbol;
use crate::code::symbol::Visibility;
use crate::code::types::{FileId, SymbolCounter, SymbolId};
use crate::code::types::{Range, SymbolKind};

/// Default channel buffer size for inter-stage communication.
const CHANNEL_SIZE: usize = 256;

/// Number of symbols per batch before flushing to the INDEX stage.
const BATCH_SIZE: usize = 5000;

/// Number of reader threads (I/O-bound).
const READ_THREADS: usize = 4;

/// Skip parsing files larger than this. Hand-written source is virtually never
/// this big; multi-MB files are minified bundles, vendored blobs, or generated
/// code whose deeply-nested ASTs dominate parse CPU (the driver of the ~165s
/// reindex) while yielding little useful symbol signal. Skipped files are logged
/// so an unexpectedly-large source file is discoverable, not silently dropped.
const MAX_INDEXABLE_FILE_BYTES: u64 = 1024 * 1024;

// ---------------------------------------------------------------------------
// Pipeline configuration
// ---------------------------------------------------------------------------

/// Configuration for the indexing pipeline.
#[derive(Debug)]
pub struct PipelineConfig {
    pub channel_size: usize,
    pub batch_size: usize,
    pub read_threads: usize,
    pub ignore_patterns: Vec<String>,
    /// When true, honor `.gitignore` (and `# mdkb:index`); when false, read
    /// `.mdkbignore` instead.
    pub respect_gitignore: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            channel_size: CHANNEL_SIZE,
            batch_size: BATCH_SIZE,
            read_threads: READ_THREADS,
            ignore_patterns: Vec::new(),
            respect_gitignore: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// JoinHandles for the spawned pipeline stages, so the driver can join them,
/// surface a stage panic (otherwise seen only as a silent channel close that
/// makes a partial index look like a clean success), and recover the PARSE
/// error count.
struct StageHandles {
    discover: thread::JoinHandle<()>,
    readers: Vec<thread::JoinHandle<()>>,
    parse: thread::JoinHandle<u32>,
    collect: thread::JoinHandle<CollectStats>,
}

impl StageHandles {
    /// Join every stage, converting a panic into an error. Returns the number
    /// of files the PARSE stage failed on.
    fn join_all(self) -> anyhow::Result<u32> {
        join_stage("DISCOVER", self.discover)?;
        for reader in self.readers {
            join_stage("READ", reader)?;
        }
        let parse_errors = join_stage("PARSE", self.parse)?;
        join_stage("COLLECT", self.collect)?;
        Ok(parse_errors)
    }
}

/// Join one stage thread, turning a panic (an `Err` from `join`) into an
/// `anyhow` error rather than letting it vanish as a clean channel close.
fn join_stage<T>(name: &str, handle: thread::JoinHandle<T>) -> anyhow::Result<T> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("indexing {name} stage panicked"))
}

/// Run the full indexing pipeline on a directory, writing results to the given database.
///
/// Returns statistics about the indexing run.
pub fn index_directory(
    root: &Path,
    db: &CodeDb,
    config: &PipelineConfig,
) -> anyhow::Result<IndexStats> {
    index_scope(root, root, db, config)
}

/// Walk `scope`, but store every path relative to `root`.
///
/// Indexing a subdirectory must not rename its files in the database, so the
/// directory to walk and the prefix paths are relative to are separate inputs.
pub fn index_scope(
    root: &Path,
    scope: &Path,
    db: &CodeDb,
    config: &PipelineConfig,
) -> anyhow::Result<IndexStats> {
    let (batch_rx, handles) = spawn_pipeline_stages(root, scope, None, config)?;
    run_index_stage(db, batch_rx, handles)
}

/// Run the indexing pipeline on specific files (not a full directory walk).
///
/// Identical to `index_directory` but replaces the DISCOVER stage with
/// the given file list. Reuses all other stages.
pub fn index_files(
    paths: &[PathBuf],
    root: &Path,
    db: &CodeDb,
    config: &PipelineConfig,
) -> anyhow::Result<IndexStats> {
    if paths.is_empty() {
        return Ok(IndexStats::default());
    }

    let (batch_rx, handles) = spawn_pipeline_stages(root, root, Some(paths), config)?;
    run_index_stage(db, batch_rx, handles)
}

/// Drive the INDEX stage on this thread, then join the upstream stages so a
/// panic surfaces as an error and the PARSE error count is folded into the
/// stats. Joins even when INDEX failed (to reap threads); a stage panic takes
/// precedence over the INDEX error as the more likely root cause.
fn run_index_stage(
    db: &CodeDb,
    batch_rx: Receiver<IndexBatch>,
    handles: StageHandles,
) -> anyhow::Result<IndexStats> {
    let index_result = stage_index(db, &batch_rx);
    // Drop our receiver so any upstream stage blocked on a full channel unblocks
    // (its send fails) and the threads can finish, keeping join_all from hanging.
    drop(batch_rx);

    let parse_errors = handles.join_all()?;
    let mut stats = index_result?;
    stats.parse_errors = parse_errors;
    Ok(stats)
}

/// Spawn the DISCOVER → READ → PARSE → COLLECT pipeline stages.
///
/// If `explicit_paths` is Some, uses those paths instead of walking `root`.
/// Returns the batch receiver for the INDEX stage to consume.
fn spawn_pipeline_stages(
    root: &Path,
    scope: &Path,
    explicit_paths: Option<&[PathBuf]>,
    config: &PipelineConfig,
) -> anyhow::Result<(Receiver<IndexBatch>, StageHandles)> {
    let (path_tx, path_rx) = bounded::<PathBuf>(config.channel_size);
    let (content_tx, content_rx) = bounded::<FileContent>(config.channel_size);
    let (parsed_tx, parsed_rx) = bounded::<ParsedFile>(config.channel_size);
    let (batch_tx, batch_rx) = bounded::<IndexBatch>(config.channel_size / 4 + 1);

    let root = root.to_path_buf();

    // DISCOVER stage — both branches return () so the handle type is uniform.
    let discover = if let Some(paths) = explicit_paths {
        let paths_owned: Vec<PathBuf> = paths.to_vec();
        thread::spawn(move || {
            for path in paths_owned {
                if path_tx.send(path).is_err() {
                    break;
                }
            }
        })
    } else {
        let discover_root = scope.to_path_buf();
        let discover_patterns = config.ignore_patterns.clone();
        let discover_respect_gitignore = config.respect_gitignore;
        thread::spawn(move || {
            stage_discover(
                &discover_root,
                &discover_patterns,
                discover_respect_gitignore,
                &path_tx,
            );
        })
    };

    // READ stage (multiple I/O threads)
    let mut readers = Vec::with_capacity(config.read_threads);
    for _ in 0..config.read_threads {
        let rx = path_rx.clone();
        let tx = content_tx.clone();
        readers.push(thread::spawn(move || stage_read(&rx, &tx)));
    }
    drop(path_rx);
    drop(content_tx);

    // PARSE stage — needs larger stack for deeply nested ASTs (e.g. minified JS)
    let parse = thread::Builder::new()
        .name("mdkb-parse".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || stage_parse(&content_rx, &parsed_tx))
        .map_err(|e| anyhow::anyhow!("Failed to spawn parse thread: {e}"))?;

    // COLLECT stage
    let batch_size = config.batch_size;
    let collect = thread::spawn(move || stage_collect(&root, &parsed_rx, &batch_tx, batch_size));

    Ok((
        batch_rx,
        StageHandles {
            discover,
            readers,
            parse,
            collect,
        },
    ))
}

// ---------------------------------------------------------------------------
// Stage 1: DISCOVER
// ---------------------------------------------------------------------------

/// Walk the filesystem and send discovered file paths to the channel.
fn stage_discover(
    root: &Path,
    ignore_patterns: &[String],
    respect_gitignore: bool,
    tx: &Sender<PathBuf>,
) -> u32 {
    let paths = walker::discover_files(root, ignore_patterns, respect_gitignore);
    let count = paths.len() as u32;
    for path in paths {
        if tx.send(path).is_err() {
            break; // downstream closed
        }
    }
    count
}

// ---------------------------------------------------------------------------
// Stage 2: READ
// ---------------------------------------------------------------------------

/// Read file content and compute hashes, sending results downstream.
fn stage_read(rx: &Receiver<PathBuf>, tx: &Sender<FileContent>) {
    while let Ok(path) = rx.recv() {
        // Cheaply skip oversized files by metadata before reading them into memory.
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > MAX_INDEXABLE_FILE_BYTES {
                tracing::warn!(
                    "Skipping {} ({} bytes > {} cap): likely minified/generated; \
                     not indexed to bound parse cost.",
                    path.display(),
                    meta.len(),
                    MAX_INDEXABLE_FILE_BYTES,
                );
                continue;
            }
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(
                    "Failed to read file {}: {e}. Impact: file will not be indexed.",
                    path.display()
                );
                continue;
            }
        };
        let hash = hasher::content_hash(&content);
        let token_estimate =
            crate::metrics::tokens::count_tokens(&content).min(u32::MAX as usize) as u32;
        if tx
            .send(FileContent {
                path,
                content,
                hash,
                token_estimate,
            })
            .is_err()
        {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 3: PARSE
// ---------------------------------------------------------------------------

/// Box a successfully-constructed parser, or log why construction failed.
///
/// A parser that fails to build (e.g. a tree-sitter grammar/query error) means a
/// whole language silently vanishes from the index. Log the language + cause so
/// the failure is diagnosable instead of an unexplained empty result.
fn build_parser<P: LanguageParser + 'static>(
    language: Language,
    result: Result<P, String>,
) -> Option<Box<dyn LanguageParser>> {
    match result {
        Ok(p) => Some(Box::new(p) as Box<dyn LanguageParser>),
        Err(e) => {
            tracing::error!(
                "Failed to construct {language:?} parser: {e}. \
                 Impact: {language:?} files will not be indexed."
            );
            None
        }
    }
}

/// Create a language parser for the given language.
fn create_parser(language: Language) -> Option<Box<dyn LanguageParser>> {
    match language {
        Language::Rust => build_parser(language, RustParser::new()),
        Language::Go => build_parser(language, GoParser::new()),
        Language::TypeScript | Language::JavaScript => {
            build_parser(language, TypeScriptParser::new())
        }
        Language::Python => build_parser(language, PythonParser::new()),
        Language::Java => build_parser(language, JavaParser::new()),
        Language::C => build_parser(language, CParser::new()),
        Language::Cpp => build_parser(language, CppParser::new()),
        Language::CSharp => build_parser(language, CSharpParser::new()),
        Language::Php => build_parser(language, PhpParser::new()),
        Language::Swift => build_parser(language, SwiftParser::new()),
        Language::Lua => build_parser(language, LuaParser::new()),
        Language::Gdscript => build_parser(language, GdscriptParser::new()),
        Language::Kotlin => build_parser(language, KotlinParser::new()),
    }
}

/// Parse file content into symbols, imports, and relationships.
/// Returns the number of files that failed to parse.
fn stage_parse(rx: &Receiver<FileContent>, tx: &Sender<ParsedFile>) -> u32 {
    let mut errors = 0u32;
    // Cache parsers by language to avoid re-creating them per file
    let mut parsers: HashMap<Language, Option<Box<dyn LanguageParser>>> = HashMap::new();

    while let Ok(fc) = rx.recv() {
        let Some(language) = Language::from_path(&fc.path) else {
            tracing::warn!(
                "Unsupported or unknown language for file: {}",
                fc.path.display()
            );
            errors += 1;
            continue;
        };

        // Get or create parser for this language
        let Some(parser) = parsers
            .entry(language)
            .or_insert_with(|| create_parser(language))
            .as_mut()
        else {
            errors += 1;
            continue;
        };

        // Use a temporary counter - real IDs assigned in COLLECT
        let dummy_file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let symbols = parser.parse(&fc.content, dummy_file_id, &mut counter);
        let calls = parser.find_calls(&fc.content);
        let implementations = parser.find_implementations(&fc.content);
        let extends = parser.find_extends(&fc.content);
        let uses = parser.find_uses(&fc.content);
        let defines = parser.find_defines(&fc.content);
        // `find_imports` takes a file id only to stamp the `Import` it returns;
        // COLLECT assigns the real one, so the dummy is discarded here.
        let raw_imports: Vec<RawImport> = parser
            .find_imports(&fc.content, dummy_file_id)
            .into_iter()
            .map(|i| RawImport {
                path: i.path.into(),
                alias: i.alias.map(Into::into),
                is_glob: i.is_glob,
                is_type_only: i.is_type_only,
            })
            .collect();

        // Convert symbols to RawSymbol (strip dummy IDs)
        let raw_symbols: Vec<RawSymbol> = symbols
            .into_iter()
            .map(|s| RawSymbol {
                name: s.name,
                kind: s.kind,
                range: s.range,
                signature: s.signature,
                doc_comment: s.doc_comment,
                visibility: s.visibility,
                scope_context: s.scope_context,
            })
            .collect();

        // Collect all relationships from parser results
        let mut raw_relationships = Vec::new();

        for (caller, callee, range) in calls {
            raw_relationships.push(RawRelationship {
                from_name: caller.into(),
                from_range: range,
                to_name: callee.into(),
                to_range: range,
                kind: RelationKind::Calls,
            });
        }

        for (type_name, trait_name, range) in implementations {
            raw_relationships.push(RawRelationship {
                from_name: type_name.into(),
                from_range: range,
                to_name: trait_name.into(),
                to_range: range,
                kind: RelationKind::Implements,
            });
        }

        for (derived, base, range) in extends {
            raw_relationships.push(RawRelationship {
                from_name: derived.into(),
                from_range: range,
                to_name: base.into(),
                to_range: range,
                kind: RelationKind::Extends,
            });
        }

        for (context, used_type, range) in uses {
            raw_relationships.push(RawRelationship {
                from_name: context.into(),
                from_range: range,
                to_name: used_type.into(),
                to_range: range,
                kind: RelationKind::Uses,
            });
        }

        for (definer, method, range) in defines {
            raw_relationships.push(RawRelationship {
                from_name: definer.into(),
                from_range: range,
                to_name: method.into(),
                to_range: range,
                kind: RelationKind::Defines,
            });
        }

        let parsed = ParsedFile {
            path: fc.path,
            content_hash: fc.hash,
            language,
            token_estimate: fc.token_estimate,
            raw_symbols,
            raw_relationships,
            raw_imports,
        };

        if tx.send(parsed).is_err() {
            break;
        }
    }

    errors
}

// ---------------------------------------------------------------------------
// Stage 4: COLLECT
// ---------------------------------------------------------------------------

/// Partial statistics from the COLLECT stage.
struct CollectStats {
    files_indexed: u32,
    relationships_collected: u32,
}

/// Assign sequential IDs to symbols and files, batch results for INDEX.
fn stage_collect(
    root: &Path,
    rx: &Receiver<ParsedFile>,
    tx: &Sender<IndexBatch>,
    batch_size: usize,
) -> CollectStats {
    let mut file_counter: u32 = 0;
    let mut symbol_counter = SymbolCounter::new();
    let mut batch = IndexBatch::default();
    let mut stats = CollectStats {
        files_indexed: 0,
        relationships_collected: 0,
    };

    // Cache for resolving from_id in relationships: (name, file_id, range) → SymbolId
    let mut symbol_lookup: HashMap<(Box<str>, u32, u32), SymbolId> = HashMap::new();
    // Fallback: (name, file_id) → SymbolId
    let mut name_in_file: HashMap<(Box<str>, u32), SymbolId> = HashMap::new();

    while let Ok(parsed) = rx.recv() {
        file_counter += 1;
        let Some(file_id) = FileId::new(file_counter) else {
            continue; // shouldn't happen
        };

        let file_path = parsed.path.strip_prefix(root).unwrap_or(&parsed.path);
        let file_path_str: Box<str> = file_path.to_string_lossy().into();

        let mtime = hasher::file_mtime(&parsed.path).unwrap_or(0);

        // Register file
        batch.file_registrations.push(FileRegistration {
            path: parsed.path.clone(),
            rel_path: file_path_str.clone(),
            file_id,
            content_hash: parsed.content_hash,
            language: parsed.language,
            mtime,
            token_estimate: parsed.token_estimate,
        });

        for import in parsed.raw_imports {
            batch.imports.push(CollectedImport { file_id, import });
        }

        // Convert raw symbols to Symbol with real IDs
        for raw in parsed.raw_symbols {
            let sym_id = symbol_counter.next_id();

            // Cache for relationship resolution
            let name_key: Box<str> = (*raw.name).into();
            symbol_lookup.insert(
                (name_key.clone(), file_id.value(), raw.range.start_line),
                sym_id,
            );
            name_in_file.insert((name_key, file_id.value()), sym_id);

            let symbol = Symbol {
                id: sym_id,
                name: raw.name,
                kind: raw.kind,
                file_id,
                range: raw.range,
                file_path: file_path_str.clone(),
                signature: raw.signature,
                doc_comment: raw.doc_comment,
                module_path: None,
                visibility: raw.visibility,
                scope_context: raw.scope_context,
            };

            batch.symbols.push((symbol, parsed.path.clone()));
        }

        // Create synthetic <module> symbol if any relationship uses it as caller
        let module_key: Box<str> = "<module>".into();
        if !name_in_file.contains_key(&(module_key.clone(), file_id.value()))
            && parsed
                .raw_relationships
                .iter()
                .any(|r| &*r.from_name == "<module>")
        {
            let sym_id = symbol_counter.next_id();
            name_in_file.insert((module_key, file_id.value()), sym_id);

            let symbol = Symbol {
                id: sym_id,
                name: "<module>".into(),
                kind: SymbolKind::Module,
                file_id,
                range: Range {
                    start_line: 0,
                    start_column: 0,
                    end_line: 0,
                    end_column: 0,
                },
                file_path: file_path_str.clone(),
                signature: None,
                doc_comment: None,
                module_path: None,
                visibility: Visibility::Private,
                scope_context: None,
            };
            batch.symbols.push((symbol, parsed.path.clone()));
        }

        // Convert raw relationships, resolving from_id where possible
        for raw_rel in parsed.raw_relationships {
            let from_id = symbol_lookup
                .get(&(
                    raw_rel.from_name.clone(),
                    file_id.value(),
                    raw_rel.from_range.start_line,
                ))
                .copied()
                .or_else(|| {
                    name_in_file
                        .get(&(raw_rel.from_name.clone(), file_id.value()))
                        .copied()
                });

            batch.unresolved_relationships.push(CollectedRelationship {
                from_id,
                from_name: raw_rel.from_name,
                to_name: raw_rel.to_name,
                file_id,
                kind: raw_rel.kind,
                to_range: Some(raw_rel.to_range),
            });
            stats.relationships_collected += 1;
        }

        stats.files_indexed += 1;

        // Flush batch when it reaches the configured size
        if batch.symbol_count() >= batch_size {
            if tx.send(batch).is_err() {
                return stats;
            }
            batch = IndexBatch::default();
        }
    }

    // Flush remaining
    if batch.symbol_count() > 0 || !batch.file_registrations.is_empty() {
        let _ = tx.send(batch);
    }

    stats
}

// ---------------------------------------------------------------------------
// Stage 5: INDEX
// ---------------------------------------------------------------------------

/// Write batches to SQLite and return final statistics.
fn stage_index(db: &CodeDb, rx: &Receiver<IndexBatch>) -> anyhow::Result<IndexStats> {
    let mut stats = IndexStats::default();

    while let Ok(batch) = rx.recv() {
        // Wrap each batch in a transaction for atomicity and performance.
        // Without this, each INSERT is an autocommit with its own fsync.
        db.conn().execute_batch("BEGIN")?;

        let result = write_batch(db, &batch, &mut stats);
        match result {
            Ok(()) => db.conn().execute_batch("COMMIT")?,
            Err(e) => {
                let _ = db.conn().execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }

    Ok(stats)
}

/// Write a single batch of index data to the database.
///
/// Called within a transaction managed by `stage_index`.
fn write_batch(db: &CodeDb, batch: &IndexBatch, stats: &mut IndexStats) -> anyhow::Result<()> {
    // Build mapping from pipeline-assigned file IDs to real SQLite rowids.
    // The pipeline assigns sequential IDs (1,2,3...) but after incremental
    // reindex, SQLite rowids won't match (e.g. auto-incremented past old max).
    let mut file_id_map: HashMap<u32, i64> = HashMap::with_capacity(batch.file_registrations.len());

    for reg in &batch.file_registrations {
        let real_id = db.insert_file(
            &reg.rel_path,
            &reg.rel_path,
            &reg.content_hash,
            Some(reg.language.config_key()),
            Some(reg.mtime as i64),
            Some(i64::from(reg.token_estimate)),
        )?;
        file_id_map.insert(reg.file_id.value(), real_id);
        stats.files_discovered += 1;
        stats.files_indexed += 1;
    }

    for collected in &batch.imports {
        let real_file_id = *file_id_map.get(&collected.file_id.value()).ok_or_else(|| {
            anyhow::anyhow!(
                "import {:?} references file_id {} not registered in this batch",
                collected.import.path,
                collected.file_id.value(),
            )
        })?;
        db.insert_import(
            real_file_id,
            &collected.import.path,
            collected.import.alias.as_deref(),
            collected.import.is_glob,
            collected.import.is_type_only,
        )?;
    }

    // Map pipeline symbol IDs to real SQLite rowids
    let mut symbol_id_map: HashMap<u32, i64> = HashMap::with_capacity(batch.symbols.len());

    for (symbol, _path) in &batch.symbols {
        // A missing entry means the file was not registered in this batch — a
        // pipeline invariant break. Fall-back to the raw pipeline id would write a
        // non-existent file_id and raise a FK violation; fail loudly instead.
        let real_file_id = *file_id_map.get(&symbol.file_id.value()).ok_or_else(|| {
            anyhow::anyhow!(
                "symbol {:?} references file_id {} not registered in this batch",
                symbol.as_name(),
                symbol.file_id.value(),
            )
        })?;
        let scope_json = symbol
            .scope_context
            .as_ref()
            .and_then(|sc| serde_json::to_string(sc).ok());
        let real_sym_id = db.insert_symbol(
            symbol.as_name(),
            &symbol.kind.to_string(),
            real_file_id,
            &symbol.file_path,
            symbol.range.start_line,
            Some(symbol.range.start_column),
            Some(symbol.range.end_line),
            Some(symbol.range.end_column),
            symbol.visibility as u8,
            symbol.as_signature(),
            symbol.as_doc_comment(),
            symbol.as_module_path(),
            scope_json.as_deref(),
        )?;
        symbol_id_map.insert(symbol.id.value(), real_sym_id);
        stats.symbols_indexed += 1;
    }

    // Write relationships, remapping both file_id and from_symbol_id
    for rel in &batch.unresolved_relationships {
        let real_file_id = *file_id_map.get(&rel.file_id.value()).ok_or_else(|| {
            anyhow::anyhow!(
                "relationship {}->{} references file_id {} not registered in this batch",
                rel.from_name,
                rel.to_name,
                rel.file_id.value(),
            )
        })?;
        let real_from_id = rel
            .from_id
            .and_then(|id| symbol_id_map.get(&id.value()).copied());
        let to_range = rel.to_range.as_ref();
        db.insert_relationship(
            real_from_id,
            &rel.from_name,
            &rel.to_name,
            &rel.kind.to_string(),
            real_file_id,
            (
                to_range.map(|r| r.start_line),
                to_range.map(|r| r.start_column),
            ),
        )?;
        stats.relationships_collected += 1;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn test_config() -> PipelineConfig {
        PipelineConfig {
            channel_size: 16,
            batch_size: 100,
            read_threads: 2,
            ignore_patterns: Vec::new(),
            respect_gitignore: true,
        }
    }

    fn temp_db() -> (tempfile::TempDir, CodeDb) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("code.sqlite");
        let db = CodeDb::create(&path).unwrap();
        (dir, db)
    }

    #[test]
    fn test_index_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (_db_dir, db) = temp_db();

        let stats = index_directory(dir.path(), &db, &test_config()).unwrap();

        assert_eq!(stats.symbols_indexed, 0);
        assert_eq!(db.file_count().unwrap(), 0);
    }

    #[test]
    fn oversized_file_is_skipped_smaller_indexed() {
        let dir = tempfile::tempdir().unwrap();
        // A normal file indexes; an oversized sibling is skipped by the size cap.
        fs::write(dir.path().join("small.rs"), "pub fn small_fn() {}").unwrap();
        let big = format!(
            "pub fn big_fn() {{}}\n// {}",
            "x".repeat(MAX_INDEXABLE_FILE_BYTES as usize + 1)
        );
        fs::write(dir.path().join("big.rs"), big).unwrap();

        let (_db_dir, db) = temp_db();
        index_directory(dir.path(), &db, &test_config()).unwrap();

        assert_eq!(
            db.file_count().unwrap(),
            1,
            "only the small file should be indexed"
        );
        assert!(
            db.find_symbols_by_name("big_fn").unwrap().is_empty(),
            "symbol from the oversized file must not be indexed"
        );
        assert!(!db.find_symbols_by_name("small_fn").unwrap().is_empty());
    }

    #[test]
    fn test_index_single_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            r#"
/// A test function.
pub fn hello() -> String {
    String::from("hello")
}

struct Foo {
    bar: i32,
}
"#,
        )
        .unwrap();

        let (_db_dir, db) = temp_db();
        let stats = index_directory(dir.path(), &db, &test_config()).unwrap();

        assert_eq!(db.file_count().unwrap(), 1);
        assert!(stats.symbols_indexed >= 2); // at least hello and Foo
        assert!(db.symbol_count().unwrap() >= 2);
    }

    #[test]
    fn test_index_multiple_languages() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("app.go"), "package main\nfunc main() {}").unwrap();
        fs::write(dir.path().join("index.ts"), "function greet() {}").unwrap();
        fs::write(dir.path().join("script.py"), "def greet():\n    pass").unwrap();

        let (_db_dir, db) = temp_db();
        let stats = index_directory(dir.path(), &db, &test_config()).unwrap();

        assert!(db.file_count().unwrap() >= 4);
        assert!(stats.symbols_indexed >= 4);
    }

    #[test]
    fn test_index_extracts_relationships() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            r#"
fn caller() {
    callee();
}

fn callee() {}
"#,
        )
        .unwrap();

        let (_db_dir, db) = temp_db();
        let stats = index_directory(dir.path(), &db, &test_config()).unwrap();

        assert!(stats.symbols_indexed >= 2);
        // Relationships are now persisted in SQLite
        assert!(
            stats.relationships_collected > 0 || db.relationship_count().unwrap() > 0,
            "expected persisted relationships"
        );
    }

    #[test]
    fn test_index_writes_to_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "pub fn search_me() {}").unwrap();

        let (_db_dir, db) = temp_db();
        index_directory(dir.path(), &db, &test_config()).unwrap();

        assert!(
            db.symbol_count().unwrap() > 0,
            "expected symbols in database"
        );
        assert!(db.file_count().unwrap() > 0, "expected files in database");
    }

    #[test]
    fn test_unsupported_files_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "# Hello").unwrap();
        fs::write(dir.path().join("data.json"), "{}").unwrap();
        fs::write(dir.path().join("lib.rs"), "fn foo() {}").unwrap();

        let (_db_dir, db) = temp_db();
        let stats = index_directory(dir.path(), &db, &test_config()).unwrap();

        // Only the .rs file should be indexed
        assert_eq!(db.file_count().unwrap(), 1);
        assert!(stats.symbols_indexed >= 1);
    }

    #[test]
    fn test_create_parser_supported_languages() {
        assert!(create_parser(Language::Rust).is_some());
        assert!(create_parser(Language::Go).is_some());
        assert!(create_parser(Language::TypeScript).is_some());
        assert!(create_parser(Language::JavaScript).is_some());
        assert!(create_parser(Language::Python).is_some());
        assert!(create_parser(Language::Java).is_some());
        assert!(create_parser(Language::C).is_some());
        assert!(create_parser(Language::Cpp).is_some());
        assert!(create_parser(Language::CSharp).is_some());
        assert!(create_parser(Language::Php).is_some());
        assert!(create_parser(Language::Swift).is_some());
        assert!(create_parser(Language::Lua).is_some());
        assert!(create_parser(Language::Gdscript).is_some());
    }

    #[test]
    fn test_create_parser_kotlin() {
        assert!(create_parser(Language::Kotlin).is_some());
    }

    #[test]
    fn test_index_files_specific_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "pub fn aaa() {}").unwrap();
        fs::write(dir.path().join("b.rs"), "pub fn bbb() {}").unwrap();
        fs::write(dir.path().join("c.rs"), "pub fn ccc() {}").unwrap();

        let (_db_dir, db) = temp_db();

        // Only index 2 of the 3 files
        let paths = vec![dir.path().join("a.rs"), dir.path().join("b.rs")];
        let stats = index_files(&paths, dir.path(), &db, &test_config()).unwrap();

        assert!(stats.symbols_indexed >= 2);
        assert_eq!(db.file_count().unwrap(), 2);
    }

    #[test]
    fn parse_errors_folded_into_stats_for_unsupported_files() {
        // BUG-D1 / ARCH-D1: a file the PARSE stage can't handle (unknown
        // language) must be counted in IndexStats.parse_errors, not silently
        // dropped. index_files bypasses the walker's language filter, so we can
        // feed an unsupported extension directly.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("ok.rs"), "pub fn ok_fn() {}").unwrap();
        fs::write(dir.path().join("notcode.unknownext"), "lorem ipsum").unwrap();

        let (_db_dir, db) = temp_db();
        let paths = vec![
            dir.path().join("ok.rs"),
            dir.path().join("notcode.unknownext"),
        ];
        let stats = index_files(&paths, dir.path(), &db, &test_config()).unwrap();

        assert_eq!(
            stats.parse_errors, 1,
            "the unsupported file must be counted in parse_errors"
        );
        assert!(
            stats.symbols_indexed >= 1,
            "the supported file must still be indexed"
        );
    }

    #[test]
    fn join_stage_surfaces_a_panic_as_error() {
        // ARCH-D1: a panicked stage thread must become an anyhow error rather
        // than a silent clean channel close that reports partial data as success.
        let handle = thread::spawn(|| panic!("boom"));
        let err = join_stage("TEST", handle).unwrap_err();
        assert!(
            err.to_string().contains("TEST stage panicked"),
            "panic should surface as a stage error, got: {err}"
        );
    }

    #[test]
    fn test_index_files_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let (_db_dir, db) = temp_db();

        let stats = index_files(&[], dir.path(), &db, &test_config()).unwrap();
        assert_eq!(stats.symbols_indexed, 0);
    }

    /// Regression test: incremental index_files after deleting stale entries
    /// must not trigger FOREIGN KEY constraint failures. The pipeline assigns
    /// sequential IDs (1,2,3...) but after delete + re-insert, SQLite rowids
    /// won't start at 1. write_batch must remap pipeline IDs to real rowids.
    #[test]
    fn test_incremental_index_after_delete_no_fk_violation() {
        let dir = tempfile::tempdir().unwrap();
        // Phase 1: initial index with several files to push rowids up
        fs::write(
            dir.path().join("a.rs"),
            "pub fn aaa() { bbb(); }\nfn bbb() {}",
        )
        .unwrap();
        fs::write(dir.path().join("b.rs"), "pub fn ccc() {}").unwrap();
        fs::write(dir.path().join("c.rs"), "pub fn ddd() {}").unwrap();

        let (_db_dir, db) = temp_db();
        let stats1 = index_directory(dir.path(), &db, &test_config()).unwrap();
        assert!(stats1.files_indexed >= 3);

        // Phase 2: simulate incremental reindex — delete stale files, then re-index.
        // This mimics what CodeIndexer::index_directory does for changed files.
        let a_path = "a.rs";
        let b_path = "b.rs";
        // Delete symbols first (for FTS trigger), then files
        db.conn().execute("DELETE FROM code_symbols WHERE file_id IN (SELECT id FROM code_files WHERE path IN (?1, ?2))", rusqlite::params![a_path, b_path]).unwrap();
        db.conn()
            .execute(
                "DELETE FROM code_files WHERE path IN (?1, ?2)",
                rusqlite::params![a_path, b_path],
            )
            .unwrap();

        // Re-index the deleted files with modified content
        fs::write(
            dir.path().join("a.rs"),
            "pub fn aaa_v2() { bbb_v2(); }\nfn bbb_v2() {}",
        )
        .unwrap();
        fs::write(dir.path().join("b.rs"), "pub fn ccc_v2() {}").unwrap();

        let paths = vec![dir.path().join("a.rs"), dir.path().join("b.rs")];
        // This would fail with "FOREIGN KEY constraint failed" before the fix
        let stats2 = index_files(&paths, dir.path(), &db, &test_config()).unwrap();

        assert!(stats2.files_indexed >= 2);
        assert!(stats2.symbols_indexed >= 3); // aaa_v2, bbb_v2, ccc_v2
        assert!(db.file_count().unwrap() >= 3); // a, b (re-added) + c (untouched)
    }
}
