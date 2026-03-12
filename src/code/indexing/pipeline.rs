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

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::code::indexing::hasher;
use crate::code::indexing::types::{
    FileContent, FileRegistration, IndexBatch, IndexStats, ParsedFile, RawRelationship,
    RawSymbol, CollectedRelationship,
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
use crate::code::types::{FileId, SymbolCounter, SymbolId};

/// Default channel buffer size for inter-stage communication.
const CHANNEL_SIZE: usize = 256;

/// Number of symbols per batch before flushing to the INDEX stage.
const BATCH_SIZE: usize = 5000;

/// Number of reader threads (I/O-bound).
const READ_THREADS: usize = 4;

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
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            channel_size: CHANNEL_SIZE,
            batch_size: BATCH_SIZE,
            read_threads: READ_THREADS,
            ignore_patterns: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run the full indexing pipeline on a directory, writing results to the given database.
///
/// Returns statistics about the indexing run.
pub fn index_directory(
    root: &Path,
    db: &CodeDb,
    config: &PipelineConfig,
) -> anyhow::Result<IndexStats> {
    let batch_rx = spawn_pipeline_stages(root, None, config)?;

    // INDEX stage runs on this thread
    let stats = stage_index(db, &batch_rx)?;

    Ok(stats)
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

    let batch_rx = spawn_pipeline_stages(root, Some(paths), config)?;

    // INDEX stage runs on this thread
    let stats = stage_index(db, &batch_rx)?;

    Ok(stats)
}

/// Spawn the DISCOVER → READ → PARSE → COLLECT pipeline stages.
///
/// If `explicit_paths` is Some, uses those paths instead of walking `root`.
/// Returns the batch receiver for the INDEX stage to consume.
fn spawn_pipeline_stages(
    root: &Path,
    explicit_paths: Option<&[PathBuf]>,
    config: &PipelineConfig,
) -> anyhow::Result<Receiver<IndexBatch>> {
    let (path_tx, path_rx) = bounded::<PathBuf>(config.channel_size);
    let (content_tx, content_rx) = bounded::<FileContent>(config.channel_size);
    let (parsed_tx, parsed_rx) = bounded::<ParsedFile>(config.channel_size);
    let (batch_tx, batch_rx) = bounded::<IndexBatch>(config.channel_size / 4 + 1);

    let root = root.to_path_buf();

    // DISCOVER stage
    if let Some(paths) = explicit_paths {
        let paths_owned: Vec<PathBuf> = paths.to_vec();
        thread::spawn(move || {
            for path in paths_owned {
                if path_tx.send(path).is_err() {
                    break;
                }
            }
        });
    } else {
        let discover_root = root.clone();
        let discover_patterns = config.ignore_patterns.clone();
        thread::spawn(move || stage_discover(&discover_root, &discover_patterns, &path_tx));
    }

    // READ stage (multiple I/O threads)
    for _ in 0..config.read_threads {
        let rx = path_rx.clone();
        let tx = content_tx.clone();
        thread::spawn(move || stage_read(&rx, &tx));
    }
    drop(path_rx);
    drop(content_tx);

    // PARSE stage
    thread::spawn(move || stage_parse(&content_rx, &parsed_tx));

    // COLLECT stage
    let batch_size = config.batch_size;
    thread::spawn(move || stage_collect(&root, &parsed_rx, &batch_tx, batch_size));

    Ok(batch_rx)
}

// ---------------------------------------------------------------------------
// Stage 1: DISCOVER
// ---------------------------------------------------------------------------

/// Walk the filesystem and send discovered file paths to the channel.
fn stage_discover(root: &Path, ignore_patterns: &[String], tx: &Sender<PathBuf>) -> u32 {
    let paths = walker::discover_files(root, ignore_patterns);
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
        if tx.send(FileContent { path, content, hash }).is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Stage 3: PARSE
// ---------------------------------------------------------------------------

/// Create a language parser for the given language.
fn create_parser(language: Language) -> Option<Box<dyn LanguageParser>> {
    match language {
        Language::Rust => RustParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>),
        Language::Go => GoParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>),
        Language::TypeScript | Language::JavaScript => {
            TypeScriptParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>)
        }
        Language::Python => {
            PythonParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>)
        }
        Language::Java => JavaParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>),
        Language::C => CParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>),
        Language::Cpp => CppParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>),
        Language::CSharp => {
            CSharpParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>)
        }
        Language::Php => PhpParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>),
        Language::Swift => {
            SwiftParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>)
        }
        Language::Lua => LuaParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>),
        Language::Gdscript => {
            GdscriptParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>)
        }
        Language::Kotlin => {
            KotlinParser::new().ok().map(|p| Box::new(p) as Box<dyn LanguageParser>)
        }
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
            tracing::warn!("Unsupported or unknown language for file: {}", fc.path.display());
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
        let _imports = parser.find_imports(&fc.content, dummy_file_id);
        let calls = parser.find_calls(&fc.content);
        let implementations = parser.find_implementations(&fc.content);
        let extends = parser.find_extends(&fc.content);
        let uses = parser.find_uses(&fc.content);
        let defines = parser.find_defines(&fc.content);

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
            raw_symbols,
            raw_relationships,
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
        });

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

        // Convert raw relationships, resolving from_id where possible
        for raw_rel in parsed.raw_relationships {
            let from_id = symbol_lookup
                .get(&(raw_rel.from_name.clone(), file_id.value(), raw_rel.from_range.start_line))
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
fn stage_index(
    db: &CodeDb,
    rx: &Receiver<IndexBatch>,
) -> anyhow::Result<IndexStats> {
    let mut stats = IndexStats::default();

    while let Ok(batch) = rx.recv() {
        // Wrap each batch in a transaction for atomicity and performance.
        // Without this, each INSERT is an autocommit with its own fsync.
        db.conn().execute_batch("BEGIN")?;

        // Write file registrations
        for reg in &batch.file_registrations {
            let abs_path = reg.path.to_string_lossy();
            db.insert_file(
                &abs_path,
                &reg.rel_path,
                &reg.content_hash,
                Some(reg.language.config_key()),
                Some(reg.mtime as i64),
            )?;
            stats.files_discovered += 1;
            stats.files_indexed += 1;
        }

        // Write symbols — get the file_id from the file registration
        for (symbol, _path) in &batch.symbols {
            let scope_json = symbol.scope_context.as_ref().and_then(|sc| {
                serde_json::to_string(sc).ok()
            });
            db.insert_symbol(
                symbol.as_name(),
                &symbol.kind.to_string(),
                i64::from(symbol.file_id.value()),
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
            stats.symbols_indexed += 1;
        }

        // Write relationships directly to SQLite (no longer "unresolved")
        for rel in &batch.unresolved_relationships {
            let to_range = rel.to_range.as_ref();
            db.insert_relationship(
                rel.from_id.map(|id| i64::from(id.value())),
                &rel.from_name,
                &rel.to_name,
                &rel.kind.to_string(),
                i64::from(rel.file_id.value()),
                to_range.map(|r| r.start_line),
                to_range.map(|r| r.start_column),
            )?;
            stats.relationships_collected += 1;
        }

        db.conn().execute_batch("COMMIT")?;
    }

    Ok(stats)
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

        assert!(db.symbol_count().unwrap() > 0, "expected symbols in database");
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
        let paths = vec![
            dir.path().join("a.rs"),
            dir.path().join("b.rs"),
        ];
        let stats = index_files(&paths, dir.path(), &db, &test_config()).unwrap();

        assert!(stats.symbols_indexed >= 2);
        assert_eq!(db.file_count().unwrap(), 2);
    }

    #[test]
    fn test_index_files_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let (_db_dir, db) = temp_db();

        let stats = index_files(&[], dir.path(), &db, &test_config()).unwrap();
        assert_eq!(stats.symbols_indexed, 0);
    }
}
