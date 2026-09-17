//! The code index: building it, and querying symbols and their relations.
//!
//! A separate store (`code.sqlite`) with its own lifecycle. The MCP code tools
//! and the daemon's watcher-driven reindex both drive it, so it is not a
//! command-line concern.
//!
//! Its path is `.mdkb/code.sqlite` under the project root, deliberately
//! independent of the memory store's namespace (`store::namespace`). The code
//! index is derived from the source tree, not from the corpus: a namespaced
//! process indexing the same files would produce the same rows, so a second
//! copy per namespace would be a cold rebuild for no isolation gained. Only
//! what a process *writes about the project* is namespaced; what it *reads
//! from the project* is shared. The same reasoning covers `vectors.bin` beside
//! it and the legacy-file housekeeping in `core::indexing`.

use std::path::Path;

use anyhow::Context as _;

use crate::code::storage::NameMatch;
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
        index_paths(&mut facade, root, paths).map_err(|e| Error::other(format!("{e:#}")))
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
/// user-supplied path cannot make the indexer walk outside the project. The
/// argument bounds the walk only: stored paths stay relative to the project
/// root, so indexing `src/` and indexing the whole tree agree on every name.
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
        // `.context`, not a rebuilt message: `run_code_mutation` walks the chain
        // for the SQLite cause, and a fresh `anyhow!` string has no chain.
        let stats = facade
            .index_scope(&root_canonical, &canonical)
            .with_context(|| format!("Indexing '{p}' failed"))?;
        total.absorb(&stats);
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
    result.map_err(|e| Error::other(format!("{e:#}")))
}

/// Reindex through an already-open facade owned by the daemon.
pub fn reindex_paths(
    facade: &mut crate::code::indexing::IndexFacade,
    root: &Path,
    paths: &[String],
) -> anyhow::Result<crate::code::indexing::types::IndexStats> {
    let root_canonical = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Cannot resolve root: {e}"))?;
    let scope = if paths.is_empty() {
        root_canonical.clone()
    } else {
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
        .reindex_scope(&root_canonical, &scope)
        .context("Reindexing failed")
}
/// Translate a `kind` filter to its stored spelling, with the message both the
/// CLI and the MCP server report for an unknown kind.
pub fn parse_kind_filter(kind_filter: Option<&str>) -> Result<Option<String>> {
    let Some(kind_str) = kind_filter else {
        return Ok(None);
    };
    let kind: crate::code::types::SymbolKind = kind_str.parse().map_err(|_| {
        Error::other(format!(
            "Unknown symbol kind: '{kind_str}'. Valid kinds: function, method, struct, \
             enum, trait, interface, class, module, variable, constant, field, \
             parameter, type_alias, macro, actor, signal"
        ))
    })?;
    Ok(Some(kind.to_string()))
}
/// Symbol search behind `scope=symbols`, shared by the CLI and the MCP server.
///
/// Both surfaces route here so they cannot answer the same query differently.
/// The name match is fuzzy — substring over name, signature and doc comment.
/// Exact lookup is [`handle_code_find`].
pub fn search_symbols_scoped(
    index: &crate::code::indexing::IndexFacade,
    query: &str,
    kind_filter: Option<&str>,
    file_filter: Option<&str>,
    limit: usize,
) -> Result<CodeFindResult> {
    let kind = parse_kind_filter(kind_filter)?;
    // An empty or wildcard query means "whatever the other filters allow" —
    // otherwise `--file src/mcp` alone could not list a file's symbols.
    let name = if query.is_empty() || query == "*" {
        NameMatch::Any
    } else {
        NameMatch::Fuzzy(query)
    };

    let (symbols, total) = index.query_symbols(name, kind.as_deref(), file_filter, limit);
    Ok(CodeFindResult { symbols, total })
}
/// Semantic code search behind `scope=code`, shared by the CLI and the MCP
/// server.
///
/// `threshold` is the caller's override; `None` takes the configured
/// `code.semantic_search.threshold`.
pub fn semantic_search_scoped(
    index: &crate::code::indexing::IndexFacade,
    config: &crate::config::Config,
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
    threshold: Option<f32>,
) -> Result<Vec<(crate::code::symbol::Symbol, f32)>> {
    if !config.code.semantic_search.enabled {
        return Err(Error::other(
            "Semantic code search is disabled. Enable it in .mdkb/config.toml: \
             [code.semantic_search] enabled = true, then re-index.",
        ));
    }
    let kind = parse_kind_filter(kind_filter)?;
    let threshold = threshold.unwrap_or(config.code.semantic_search.threshold as f32);

    let mut results = index
        .semantic_search(query, limit, threshold)
        .map_err(|e| Error::other(format!("Semantic code search failed: {e}")))?;

    if let Some(kind) = kind {
        results.retain(|(s, _)| s.kind.to_string() == kind);
    }
    Ok(results)
}
/// Handle `mdkb search --scope code` - semantic code search.
pub fn handle_semantic_code_search(
    root: &Path,
    config_path: &Path,
    query: &str,
    kind_filter: Option<&str>,
    limit: usize,
) -> Result<Vec<(crate::code::symbol::Symbol, f32)>> {
    let config = crate::config::Config::load_or_default(config_path);
    let facade = open_code_read_only(root)?;
    semantic_search_scoped(&facade, &config, query, kind_filter, limit, None)
}
/// Handle `mdkb search --scope symbols` - fuzzy symbol search.
pub fn handle_symbol_search(
    root: &Path,
    query: &str,
    kind_filter: Option<&str>,
    file_filter: Option<&str>,
    limit: usize,
) -> Result<CodeFindResult> {
    let facade = open_code_read_only(root)?;
    search_symbols_scoped(&facade, query, kind_filter, file_filter, limit)
}
/// Handle `mdkb code search` - fuzzy symbol search without a file filter.
pub fn handle_code_search(
    root: &Path,
    query: &str,
    limit: usize,
    kind_filter: Option<&str>,
) -> Result<CodeFindResult> {
    handle_symbol_search(root, query, kind_filter, None, limit)
}
/// Handle `mdkb code find` - exact symbol lookup.
///
/// A common name (`tests`, `new`) matches hundreds of definitions, so the list
/// is capped at `limit`. `total` keeps the pre-cap count: the caller must
/// report it, otherwise a capped list reads as the complete set.
pub fn handle_code_find(
    root: &Path,
    name: &str,
    kind_filter: Option<&str>,
    file_filter: Option<&str>,
    limit: usize,
) -> Result<CodeFindResult> {
    let facade = open_code_read_only(root)?;
    let kind = parse_kind_filter(kind_filter)?;

    let (symbols, total) =
        facade.query_symbols(NameMatch::Exact(name), kind.as_deref(), file_filter, limit);

    Ok(CodeFindResult { symbols, total })
}
/// The calls a symbol makes that the index could not place on a symbol of its
/// own.
///
/// Reported beside the resolved callees because an empty callee list means two
/// very different things — "this calls nothing" and "everything it calls lives
/// outside this index" — and the second is by far the more common.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnplacedCalls {
    /// Targets the call site named in full, which this index does not contain:
    /// `std::fs::write`.
    pub external: Vec<String>,
    /// Bare names with no candidate — a method on a receiver whose type the
    /// index does not know.
    pub unknown: Vec<String>,
}

/// One indexed callee and the confidence of the rule that placed it.
pub use crate::code::relationship::ResolvedCall;

/// Classify every outgoing call once, retaining both resolved and unplaced
/// targets. A tier at or below 2 is resolved; tier 3 and above is a candidate
/// to confirm against the source.
pub fn classify_calls(
    facade: &crate::code::indexing::IndexFacade,
    symbol_id: crate::code::types::SymbolId,
) -> (Vec<ResolvedCall>, UnplacedCalls) {
    use crate::code::relationship::CallTarget;

    let targets = facade.get_call_targets(symbol_id);
    let ids = targets
        .iter()
        .flat_map(|target| match target {
            CallTarget::Resolved { targets, .. } => targets.as_slice(),
            CallTarget::External { .. } | CallTarget::Unknown { .. } => &[],
        })
        .copied()
        .collect::<Vec<_>>();
    let symbols = facade.get_symbols_batch(&ids);
    let mut resolved = Vec::new();
    let mut unplaced = UnplacedCalls::default();

    for target in targets {
        match target {
            CallTarget::Resolved { tier, targets } => {
                let is_unique = targets.len() == 1;
                resolved.extend(targets.into_iter().filter_map(|id| {
                    symbols.get(&id).cloned().map(|symbol| ResolvedCall {
                        symbol,
                        tier,
                        is_unique,
                    })
                }));
            }
            CallTarget::External { qualifier, name } => {
                unplaced.external.push(format!("{qualifier}::{name}"));
            }
            CallTarget::Unknown { name } => unplaced.unknown.push(name),
        }
    }
    unplaced.external.sort_unstable();
    unplaced.external.dedup();
    unplaced.unknown.sort_unstable();
    unplaced.unknown.dedup();
    (resolved, unplaced)
}

/// Handle `mdkb code calls` - show what a symbol calls.
pub fn handle_code_calls(
    root: &Path,
    name: &str,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<ResolvedCall>,
    UnplacedCalls,
)> {
    let facade = open_code_read_only(root)?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let (callees, unplaced) = classify_calls(&facade, symbol.id);
    Ok((symbol, callees, unplaced))
}
/// Handle `mdkb code callers` - show what calls a symbol.
///
/// Returns the callers and how many of them arrived through a call no rule
/// could place. That count is what tells the reader how far to trust the list:
/// an unplaced call names this symbol only because it names *a* symbol of this
/// name, and every other one of that name is just as good a candidate.
pub fn handle_code_callers(
    root: &Path,
    name: &str,
) -> Result<(
    crate::code::symbol::Symbol,
    Vec<crate::code::symbol::Symbol>,
    usize,
)> {
    let facade = open_code_read_only(root)?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let by_tier = facade.get_callers_by_tier(symbol.id);
    let unplaced = by_tier
        .iter()
        .filter(|(_, tier)| *tier == crate::code::storage::TIER_UNPLACED)
        .count();
    let callers = by_tier.into_iter().map(|(caller, _)| caller).collect();
    Ok((symbol, callers, unplaced))
}
/// An impact answer, split by what the walk can stand behind.
#[derive(Debug, Default)]
pub struct ImpactReport {
    /// Reached through calls a rule placed. These are the symbols to act on.
    pub impacted: Vec<crate::code::symbol::Symbol>,
    /// Reached only by a bare name match, and so never walked through. They
    /// are in the list because one of them may be the real caller; they are
    /// apart from it because most of them are not.
    pub ambiguous: Vec<crate::code::symbol::Symbol>,
    /// How many of the ambiguous ones the walk stopped at while depth
    /// remained. The radius is short by whatever they call.
    pub stopped_arrivals: usize,
}

/// Handle `mdkb code impact` - impact analysis from a symbol.
///
/// Splits the radius the way [`handle_code_callers`] counts it, for the same
/// reason: a symbol no rule placed any better than the name it wrote is in the
/// list on the strength of that name alone. Over a traversal it matters more —
/// the walk refuses to continue through such an arrival, so the answer is
/// incomplete in a way only [`ImpactReport::stopped_arrivals`] records.
pub fn handle_code_impact(
    root: &Path,
    name: &str,
    depth: usize,
) -> Result<(crate::code::symbol::Symbol, ImpactReport)> {
    let facade = open_code_read_only(root)?;

    let symbol = facade
        .get_symbol_by_name(name)
        .ok_or_else(|| Error::other(format!("Symbol '{}' not found", name)))?;

    let radius = facade.get_impact_by_tier(symbol.id, depth);
    let (ambiguous, impacted): (Vec<_>, Vec<_>) = radius
        .reached
        .into_iter()
        .partition(|(_, tier)| *tier == crate::code::storage::TIER_UNPLACED);

    Ok((
        symbol,
        ImpactReport {
            impacted: impacted.into_iter().map(|(s, _)| s).collect(),
            ambiguous: ambiguous.into_iter().map(|(s, _)| s).collect(),
            stopped_arrivals: radius.stopped_arrivals,
        },
    ))
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
/// Result of `code find` command: the capped matches plus the match count
/// before truncation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodeFindResult {
    pub symbols: Vec<crate::code::symbol::Symbol>,
    pub total: usize,
}
/// Result of `code info` command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodeInfoResult {
    pub symbols: u64,
    pub files: u64,
    pub relationships: usize,
}
