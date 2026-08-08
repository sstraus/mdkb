//! The code index: building it, and querying symbols and their relations.
//!
//! A separate store (`code.sqlite`) with its own lifecycle. The MCP code tools
//! and the daemon's watcher-driven reindex both drive it, so it is not a
//! command-line concern.

use std::path::Path;

use crate::error::{Error, Result};

fn open_code_read_only(root: &Path) -> Result<crate::code::indexing::IndexFacade> {
    let index_path = root.join(".mdkb/code.sqlite");
    if !index_path.exists() {
        return Err(Error::other(format!(
            "Code index not found at {}; run `mdkb code index` first",
            index_path.display()
        )));
    }
    crate::code::indexing::IndexFacade::open_read_only(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index read-only: {e}")))
}

/// Handle `mdkb code init` - initialize code index directory (idempotent).
pub fn handle_code_init(root: &Path) -> Result<()> {
    let index_path = root.join(".mdkb/code.sqlite");
    if index_path.exists() {
        return Ok(());
    }
    crate::code::indexing::IndexFacade::create(&index_path)
        .map_err(|e| Error::other(format!("Failed to create code index: {}", e)))?;
    Ok(())
}
/// Handle `mdkb code index` - build code index from source files.
pub fn handle_code_index(
    root: &Path,
    paths: &[String],
) -> Result<crate::code::indexing::types::IndexStats> {
    let index_path = root.join(".mdkb/code.sqlite");
    let mut facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let result = if paths.is_empty() {
        // Whole-tree refresh: `update` (not `index_directory`) because only a
        // caller that walked everything may prune files deleted from disk, and
        // this is that caller. The per-path branch below is NOT — it indexes one
        // subdirectory at a time, where "indexed but absent" is the rest of the
        // project, not a deletion.
        facade
            .update(root)
            .map_err(|e| Error::other(format!("Indexing failed: {}", e)))
    } else {
        index_paths(&mut facade, root, paths).map_err(|e| Error::other(e.to_string()))
    };
    crate::llm::release_cached_service();
    result
}

/// Index the named paths into an already-open facade, accumulating their stats.
///
/// Separate from [`handle_code_index`] because the daemon holds its facade for
/// the process lifetime and must not open a second one: `handle_code_index`
/// opens its own, which is exactly the extra writer the routing exists to
/// remove. Both call this so path validation and accumulation have one
/// definition.
///
/// Each path is canonicalized and checked against the root, so a `../` in a
/// user-supplied path cannot make the indexer walk outside the project.
pub fn index_paths(
    facade: &mut crate::code::indexing::IndexFacade,
    root: &Path,
    paths: &[String],
) -> anyhow::Result<crate::code::indexing::types::IndexStats> {
    let root_canonical = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Cannot resolve root: {e}"))?;
    let mut total = crate::code::indexing::types::IndexStats::default();
    for p in paths {
        let canonical = root
            .join(p)
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Cannot resolve path '{p}': {e}"))?;
        if !canonical.starts_with(&root_canonical) {
            anyhow::bail!("Path '{p}' escapes project root");
        }
        let stats = facade
            .index_directory(&canonical)
            .map_err(|e| anyhow::anyhow!("Indexing '{p}' failed: {e}"))?;
        total.files_discovered += stats.files_discovered;
        total.files_indexed += stats.files_indexed;
        total.files_removed += stats.files_removed;
        total.symbols_indexed += stats.symbols_indexed;
        total.relationships_collected += stats.relationships_collected;
    }
    Ok(total)
}
/// Handle `mdkb code index --force` - full reindex discarding existing data.
pub fn handle_code_reindex(
    root: &Path,
    paths: &[String],
) -> Result<crate::code::indexing::types::IndexStats> {
    let index_path = root.join(".mdkb/code.sqlite");
    let mut facade = crate::code::indexing::IndexFacade::open_or_create(&index_path)
        .map_err(|e| Error::other(format!("Failed to open code index: {}", e)))?;

    let result = reindex_paths(&mut facade, root, paths);
    crate::llm::release_cached_service();
    result.map_err(|e| Error::other(e.to_string()))
}

/// Reindex through an already-open facade owned by the daemon.
pub fn reindex_paths(
    facade: &mut crate::code::indexing::IndexFacade,
    root: &Path,
    paths: &[String],
) -> anyhow::Result<crate::code::indexing::types::IndexStats> {
    let target = if paths.is_empty() {
        root.to_path_buf()
    } else {
        let root_canonical = root
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Cannot resolve root: {e}"))?;
        // Validate all paths, use first (reindex clears DB so multiple passes don't combine)
        for p in paths {
            let candidate = root.join(p);
            let canonical = candidate
                .canonicalize()
                .map_err(|e| anyhow::anyhow!("Cannot resolve path '{p}': {e}"))?;
            if !canonical.starts_with(&root_canonical) {
                anyhow::bail!("Path '{p}' escapes project root");
            }
        }
        // Use first path as reindex target (reindex is a full rebuild)
        root.join(&paths[0])
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("Cannot resolve path '{}': {e}", paths[0]))?
    };

    facade
        .reindex(&target)
        .map_err(|e| anyhow::anyhow!("Reindexing failed: {e}"))
}
/// Handle `mdkb code search` - fuzzy symbol search.
pub fn handle_code_search(
    root: &Path,
    query: &str,
    limit: usize,
    kind_filter: Option<&str>,
) -> Result<Vec<crate::code::symbol::Symbol>> {
    let facade = open_code_read_only(root)?;

    let mut results = facade.search_symbols(query, limit * 2);

    if let Some(kind_str) = kind_filter {
        let kind: crate::code::types::SymbolKind = kind_str
            .parse()
            .map_err(|_| Error::other(format!("Unknown symbol kind: {}", kind_str)))?;
        results.retain(|s| s.kind == kind);
    }

    results.truncate(limit);
    Ok(results)
}
/// Handle `mdkb code find` - exact symbol lookup.
pub fn handle_code_find(
    root: &Path,
    name: &str,
    kind_filter: Option<&str>,
    file_filter: Option<&str>,
) -> Result<Vec<crate::code::symbol::Symbol>> {
    let facade = open_code_read_only(root)?;

    let mut results = facade.find_symbols_by_name(name);

    if let Some(kind_str) = kind_filter {
        let kind: crate::code::types::SymbolKind = kind_str
            .parse()
            .map_err(|_| Error::other(format!("Unknown symbol kind: {}", kind_str)))?;
        results.retain(|s| s.kind == kind);
    }

    if let Some(file_substr) = file_filter {
        results.retain(|s| s.file_path.contains(file_substr));
    }

    Ok(results)
}
/// Handle `mdkb code calls` - show what a symbol calls.
pub fn handle_code_calls(
    root: &Path,
    name: &str,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<crate::code::symbol::Symbol>,
)> {
    let facade = open_code_read_only(root)?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let callees = facade.get_called_functions(symbol.id);
    Ok((symbol, callees))
}
/// Handle `mdkb code callers` - show what calls a symbol.
pub fn handle_code_callers(
    root: &Path,
    name: &str,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<crate::code::symbol::Symbol>,
)> {
    let facade = open_code_read_only(root)?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let callers = facade.get_calling_functions(symbol.id);
    Ok((symbol, callers))
}
/// Handle `mdkb code impact` - impact analysis from a symbol.
pub fn handle_code_impact(
    root: &Path,
    name: &str,
    depth: usize,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<crate::code::symbol::Symbol>,
)> {
    let facade = open_code_read_only(root)?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let impacted_ids = facade.get_impact_radius(symbol.id, depth);
    let impacted: Vec<_> = impacted_ids
        .iter()
        .filter_map(|&id| facade.get_symbol(id))
        .collect();

    Ok((symbol, impacted))
}
/// Handle `mdkb code info` - show index statistics.
pub fn handle_code_info(root: &Path) -> Result<CodeInfoResult> {
    let facade = open_code_read_only(root)?;

    Ok(CodeInfoResult {
        symbols: facade.symbol_count(),
        files: facade.file_count(),
        relationships: facade.relationship_count(),
    })
}
/// Handle `mdkb code parse` - parse a single file and return symbols.
pub fn handle_code_parse(file: &Path) -> Result<Vec<crate::code::symbol::Symbol>> {
    use crate::code::parsing::language::Language;
    use crate::code::parsing::parser::LanguageParser;
    use crate::code::types::{FileId, SymbolCounter};

    let language = Language::from_path(file).ok_or_else(|| {
        Error::other(format!("Unsupported language for file: {}", file.display()))
    })?;

    let code = std::fs::read_to_string(file)
        .map_err(|e| Error::other(format!("Failed to read '{}': {}", file.display(), e)))?;

    let file_id = FileId::new(1).expect("1 is valid");
    let mut counter = SymbolCounter::new();

    let mut parser: Box<dyn LanguageParser> = match language {
        Language::Rust => Box::new(
            crate::code::parsing::rust::RustParser::new()
                .map_err(|e| Error::other(format!("Failed to create Rust parser: {}", e)))?,
        ),
        Language::Go => Box::new(
            crate::code::parsing::go::GoParser::new()
                .map_err(|e| Error::other(format!("Failed to create Go parser: {}", e)))?,
        ),
        Language::TypeScript | Language::JavaScript => Box::new(
            crate::code::parsing::typescript::TypeScriptParser::new()
                .map_err(|e| Error::other(format!("Failed to create TypeScript parser: {}", e)))?,
        ),
        Language::Python => Box::new(
            crate::code::parsing::python::PythonParser::new()
                .map_err(|e| Error::other(format!("Failed to create Python parser: {}", e)))?,
        ),
        Language::Java => Box::new(
            crate::code::parsing::java::JavaParser::new()
                .map_err(|e| Error::other(format!("Failed to create Java parser: {}", e)))?,
        ),
        Language::C => Box::new(
            crate::code::parsing::c_lang::CParser::new()
                .map_err(|e| Error::other(format!("Failed to create C parser: {}", e)))?,
        ),
        Language::Cpp => Box::new(
            crate::code::parsing::cpp::CppParser::new()
                .map_err(|e| Error::other(format!("Failed to create C++ parser: {}", e)))?,
        ),
        Language::CSharp => Box::new(
            crate::code::parsing::csharp::CSharpParser::new()
                .map_err(|e| Error::other(format!("Failed to create C# parser: {}", e)))?,
        ),
        Language::Php => Box::new(
            crate::code::parsing::php::PhpParser::new()
                .map_err(|e| Error::other(format!("Failed to create PHP parser: {}", e)))?,
        ),
        Language::Swift => Box::new(
            crate::code::parsing::swift::SwiftParser::new()
                .map_err(|e| Error::other(format!("Failed to create Swift parser: {}", e)))?,
        ),
        Language::Lua => Box::new(
            crate::code::parsing::lua::LuaParser::new()
                .map_err(|e| Error::other(format!("Failed to create Lua parser: {}", e)))?,
        ),
        Language::Gdscript => Box::new(
            crate::code::parsing::gdscript::GdscriptParser::new()
                .map_err(|e| Error::other(format!("Failed to create GDScript parser: {}", e)))?,
        ),
        Language::Kotlin => Box::new(
            crate::code::parsing::kotlin::KotlinParser::new()
                .map_err(|e| Error::other(format!("Failed to create Kotlin parser: {}", e)))?,
        ),
    };

    let symbols = parser.parse(&code, file_id, &mut counter);
    Ok(symbols)
}
/// Result of `code info` command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodeInfoResult {
    pub symbols: u64,
    pub files: u64,
    pub relationships: usize,
}
