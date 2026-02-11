//! Five-stage parallel indexing pipeline.
//!
//! ```text
//! DISCOVER ──► READ ──► PARSE ──► COLLECT ──► INDEX
//! (walk FS)  (I/O)   (CPU)    (assign IDs)  (Tantivy)
//! ```
//!
//! Stages communicate via bounded crossbeam channels.
//! DISCOVER, READ, and PARSE run on multiple threads; COLLECT and
//! INDEX are single-threaded to guarantee sequential ID assignment
//! and consistent Tantivy commits.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::code::indexing::hasher;
use crate::code::indexing::types::{
    FileContent, FileRegistration, IndexBatch, IndexStats, ParsedFile, RawImport, RawRelationship,
    RawSymbol, UnresolvedRelationship,
};
use crate::code::indexing::walker;
use crate::code::parsing::c_lang::CParser;
use crate::code::parsing::cpp::CppParser;
use crate::code::parsing::csharp::CSharpParser;
use crate::code::parsing::gdscript::GdscriptParser;
use crate::code::parsing::go::GoParser;
use crate::code::parsing::import::Import;
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
use crate::code::storage::CodeIndex;
use crate::code::symbol::Symbol;
use crate::code::types::{FileId, Range, SymbolCounter, SymbolId};

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
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            channel_size: CHANNEL_SIZE,
            batch_size: BATCH_SIZE,
            read_threads: READ_THREADS,
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run the full indexing pipeline on a directory, writing results to the given index.
///
/// Returns statistics and any unresolved relationships (for future resolution).
pub fn index_directory(
    root: &Path,
    index: &CodeIndex,
    config: &PipelineConfig,
) -> anyhow::Result<(IndexStats, Vec<UnresolvedRelationship>)> {
    let (path_tx, path_rx) = bounded::<PathBuf>(config.channel_size);
    let (content_tx, content_rx) = bounded::<FileContent>(config.channel_size);
    let (parsed_tx, parsed_rx) = bounded::<ParsedFile>(config.channel_size);
    let (batch_tx, batch_rx) = bounded::<IndexBatch>(config.channel_size / 4 + 1);

    let root = root.to_path_buf();

    // DISCOVER stage (single thread)
    let discover_root = root.clone();
    let discover = thread::spawn(move || stage_discover(&discover_root, &path_tx));

    // READ stage (multiple I/O threads)
    let mut readers = Vec::with_capacity(config.read_threads);
    for _ in 0..config.read_threads {
        let rx = path_rx.clone();
        let tx = content_tx.clone();
        readers.push(thread::spawn(move || stage_read(&rx, &tx)));
    }
    // Drop our copies so channels close when workers finish
    drop(path_rx);
    drop(content_tx);

    // PARSE stage (single thread with sequential parsing)
    let parse_handle = thread::spawn(move || stage_parse(&content_rx, &parsed_tx));

    // COLLECT stage (single thread - sequential ID assignment)
    let collect_root = root.clone();
    let batch_size = config.batch_size;
    let collect_handle = thread::spawn(move || {
        stage_collect(&collect_root, &parsed_rx, &batch_tx, batch_size)
    });

    // INDEX stage runs on this thread
    let (stats, unresolved) = stage_index(index, &batch_rx)?;

    // Wait for all stages to complete
    let files_discovered = discover.join().map_err(|_| anyhow::anyhow!("discover thread panicked"))?;
    for reader in readers {
        reader.join().map_err(|_| anyhow::anyhow!("reader thread panicked"))?;
    }
    let parse_errors = parse_handle.join().map_err(|_| anyhow::anyhow!("parse thread panicked"))?;
    let collect_stats = collect_handle.join().map_err(|_| anyhow::anyhow!("collect thread panicked"))?;

    let final_stats = IndexStats {
        files_discovered,
        files_indexed: collect_stats.files_indexed,
        symbols_indexed: stats.symbols_indexed,
        relationships_collected: collect_stats.relationships_collected,
        files_skipped: parse_errors,
        errors: stats.errors,
    };

    if parse_errors > 0 {
        tracing::error!(
            "Pipeline completed with {} parse errors. Impact: {} files not indexed (unsupported language or parse failure).",
            parse_errors,
            parse_errors
        );
    }

    Ok((final_stats, unresolved))
}

// ---------------------------------------------------------------------------
// Stage 1: DISCOVER
// ---------------------------------------------------------------------------

/// Walk the filesystem and send discovered file paths to the channel.
fn stage_discover(root: &Path, tx: &Sender<PathBuf>) -> u32 {
    let paths = walker::discover_files(root);
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
    let mut parsers: HashMap<Language, Box<dyn LanguageParser>> = HashMap::new();

    while let Ok(fc) = rx.recv() {
        let Some(language) = Language::from_path(&fc.path) else {
            tracing::warn!("Unsupported or unknown language for file: {}", fc.path.display());
            errors += 1;
            continue;
        };

        // Get or create parser for this language
        let parser = match parsers.entry(language).or_insert_with(|| {
            // This creates a None for unsupported languages. We handle it below.
            create_parser(language).unwrap_or_else(|| {
                // Placeholder: will be caught by the None check
                Box::new(NullParser)
            })
        }) {
            p if p.language() == language => p,
            _ => {
                errors += 1;
                continue;
            }
        };

        // Use a temporary counter - real IDs assigned in COLLECT
        let dummy_file_id = FileId::new(1).unwrap();
        let mut counter = SymbolCounter::new();

        let symbols = parser.parse(&fc.content, dummy_file_id, &mut counter);
        let imports_raw = parser.find_imports(&fc.content, dummy_file_id);
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

        // Convert imports to RawImport (strip dummy file_id)
        let raw_imports: Vec<RawImport> = imports_raw
            .into_iter()
            .map(|i| RawImport {
                path: i.path,
                alias: i.alias,
                is_glob: i.is_glob,
                is_type_only: i.is_type_only,
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
            raw_imports,
            raw_relationships,
        };

        if tx.send(parsed).is_err() {
            break;
        }
    }

    errors
}

/// Stub parser for unsupported languages. Never actually called for parsing.
struct NullParser;

impl LanguageParser for NullParser {
    fn parse(&mut self, _: &str, _: FileId, _: &mut SymbolCounter) -> Vec<Symbol> {
        Vec::new()
    }
    fn language(&self) -> Language {
        // Return a language that won't match anything expected
        Language::Lua
    }
    fn extract_doc_comment(&self, _: &tree_sitter::Node, _: &str) -> Option<String> {
        None
    }
    fn find_calls<'a>(&mut self, _: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }
    fn find_implementations<'a>(&mut self, _: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }
    fn find_uses<'a>(&mut self, _: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }
    fn find_defines<'a>(&mut self, _: &'a str) -> Vec<(&'a str, &'a str, Range)> {
        Vec::new()
    }
    fn find_imports(&mut self, _: &str, _: FileId) -> Vec<Import> {
        Vec::new()
    }
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
        let timestamp = hasher::utc_timestamp();

        // Register file
        batch.file_registrations.push(FileRegistration {
            path: parsed.path.clone(),
            file_id,
            content_hash: parsed.content_hash,
            language: parsed.language,
            timestamp,
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

        // Convert raw imports
        for raw_import in parsed.raw_imports {
            batch.imports.push(Import {
                path: raw_import.path,
                alias: raw_import.alias,
                file_id,
                is_glob: raw_import.is_glob,
                is_type_only: raw_import.is_type_only,
            });
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

            batch.unresolved_relationships.push(UnresolvedRelationship {
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

/// Write batches to Tantivy and return final statistics.
fn stage_index(
    index: &CodeIndex,
    rx: &Receiver<IndexBatch>,
) -> anyhow::Result<(IndexStats, Vec<UnresolvedRelationship>)> {
    let mut writer = index.writer()?;
    let schema = index.schema();
    let mut stats = IndexStats::default();
    let mut all_unresolved = Vec::new();
    let mut batches_since_commit = 0u32;

    while let Ok(batch) = rx.recv() {
        // Write file registrations
        for reg in &batch.file_registrations {
            let doc = tantivy::doc!(
                schema.doc_type => "file",
                schema.file_id => u64::from(reg.file_id.value()),
                schema.file_path => reg.path.to_string_lossy().as_ref(),
                schema.file_hash => reg.content_hash.as_str(),
                schema.file_timestamp => reg.timestamp,
                schema.file_mtime => reg.mtime,
                schema.language => reg.language.config_key()
            );
            writer.add_document(doc)?;
        }

        // Write symbols
        for (symbol, _path) in &batch.symbols {
            let doc = tantivy::doc!(
                schema.doc_type => "symbol",
                schema.symbol_id => u64::from(symbol.id.value()),
                schema.name => symbol.as_name(),
                schema.name_text => symbol.as_name(),
                schema.kind => symbol.kind.to_string(),
                schema.file_path => &*symbol.file_path,
                schema.file_id => u64::from(symbol.file_id.value()),
                schema.line_number => u64::from(symbol.range.start_line),
                schema.column => u64::from(symbol.range.start_column),
                schema.end_line => u64::from(symbol.range.end_line),
                schema.end_column => u64::from(symbol.range.end_column),
                schema.visibility => symbol.visibility as u64
            );
            // Add optional fields
            let mut doc = doc;
            if let Some(sig) = symbol.as_signature() {
                doc.add_text(schema.signature, sig);
            }
            if let Some(doc_comment) = symbol.as_doc_comment() {
                doc.add_text(schema.doc_comment, doc_comment);
            }
            if let Some(module) = symbol.as_module_path() {
                doc.add_text(schema.module_path, module);
            }
            if let Some(scope) = &symbol.scope_context {
                doc.add_text(schema.scope_context, format!("{scope:?}"));
            }
            writer.add_document(doc)?;
            stats.symbols_indexed += 1;
        }

        // Write imports as metadata
        for import in &batch.imports {
            let doc = tantivy::doc!(
                schema.doc_type => "metadata",
                schema.meta_key => format!("import:{}:{}", import.file_id.value(), import.path),
                schema.file_id => u64::from(import.file_id.value())
            );
            writer.add_document(doc)?;
        }

        // Accumulate unresolved relationships
        all_unresolved.extend(batch.unresolved_relationships);

        batches_since_commit += 1;
        if batches_since_commit >= 5 {
            writer.commit()?;
            batches_since_commit = 0;
        }
    }

    // Final commit
    writer.commit()?;
    index.reload()?;

    Ok((stats, all_unresolved))
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
        }
    }

    #[test]
    fn test_index_empty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let idx_dir = tempfile::tempdir().unwrap();
        let index = CodeIndex::create(idx_dir.path().join("idx")).unwrap();

        let (stats, unresolved) =
            index_directory(dir.path(), &index, &test_config()).unwrap();

        assert_eq!(stats.files_discovered, 0);
        assert_eq!(stats.files_indexed, 0);
        assert_eq!(stats.symbols_indexed, 0);
        assert!(unresolved.is_empty());
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

        let idx_dir = tempfile::tempdir().unwrap();
        let index = CodeIndex::create(idx_dir.path().join("idx")).unwrap();

        let (stats, _) = index_directory(dir.path(), &index, &test_config()).unwrap();

        assert_eq!(stats.files_discovered, 1);
        assert_eq!(stats.files_indexed, 1);
        assert!(stats.symbols_indexed >= 2); // at least hello and Foo
    }

    #[test]
    fn test_index_multiple_languages() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("main.rs"), "fn main() {}").unwrap();
        fs::write(dir.path().join("app.go"), "package main\nfunc main() {}").unwrap();
        fs::write(dir.path().join("index.ts"), "function greet() {}").unwrap();
        fs::write(dir.path().join("script.py"), "def greet():\n    pass").unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let index = CodeIndex::create(idx_dir.path().join("idx")).unwrap();

        let (stats, _) = index_directory(dir.path(), &index, &test_config()).unwrap();

        assert_eq!(stats.files_discovered, 4);
        assert_eq!(stats.files_indexed, 4);
        assert!(stats.symbols_indexed >= 4); // at least one per file
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

        let idx_dir = tempfile::tempdir().unwrap();
        let index = CodeIndex::create(idx_dir.path().join("idx")).unwrap();

        let (stats, unresolved) =
            index_directory(dir.path(), &index, &test_config()).unwrap();

        assert!(stats.symbols_indexed >= 2);
        // Should have at least one Calls relationship
        assert!(
            unresolved.iter().any(|r| r.kind == RelationKind::Calls),
            "expected Calls relationship, got: {unresolved:?}"
        );
    }

    #[test]
    fn test_index_writes_to_tantivy() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "pub fn search_me() {}").unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let index = CodeIndex::create(idx_dir.path().join("idx")).unwrap();

        index_directory(dir.path(), &index, &test_config()).unwrap();

        // Query Tantivy to verify writes
        let searcher = index.reader().searcher();
        assert!(searcher.num_docs() > 0, "expected documents in index");
    }

    #[test]
    fn test_unsupported_files_skipped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "# Hello").unwrap();
        fs::write(dir.path().join("data.json"), "{}").unwrap();
        fs::write(dir.path().join("lib.rs"), "fn foo() {}").unwrap();

        let idx_dir = tempfile::tempdir().unwrap();
        let index = CodeIndex::create(idx_dir.path().join("idx")).unwrap();

        let (stats, _) = index_directory(dir.path(), &index, &test_config()).unwrap();

        // Only the .rs file should be discovered and indexed
        assert_eq!(stats.files_discovered, 1);
        assert_eq!(stats.files_indexed, 1);
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
}
