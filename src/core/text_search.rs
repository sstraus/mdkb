//! Literal lookup in the code index. No source scan or write occurs on search.
use std::path::Path;

use serde::Serialize;
use std::collections::HashSet;

use crate::code::indexing::walker::{self, WalkOptions};
use crate::config::Config;

use crate::code::storage::CodeDb;
use crate::error::Result;

#[derive(Debug, Clone, Serialize)]
pub struct TextHit {
    pub file: String,
    /// One-based display line (stored AST locations remain zero-based).
    pub line: u32,
    pub column: u32,
    pub content: String,
    pub enclosing_symbol: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct TextSearchResult {
    pub hits: Vec<TextHit>,
    pub truncated: bool,
    /// Oldest indexed file timestamp: a conservative indication of index age.
    pub indexed_at: Option<i64>,
}

/// `None` means this repo has no literal-capable code index yet. Default search
/// must still work in document-only repos and on indexes made by older binaries.
pub fn search(
    root: &Path,
    query: &str,
    ignore_case: bool,
    file: Option<&str>,
    limit: usize,
) -> Result<Option<TextSearchResult>> {
    let path = root.join(".mdkb/code.sqlite");
    if !path.exists() {
        return Ok(None);
    }
    let db = CodeDb::open_read_only(path)?;
    let available: bool = db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'code_literals')",
        [], |row| row.get(0),
    )?;
    if !available {
        return Ok(None);
    }
    let indexed_at = db
        .conn()
        .query_row("SELECT MIN(indexed_at) FROM code_files", [], |row| {
            row.get(0)
        })?;
    let mut result = TextSearchResult {
        indexed_at,
        ..Default::default()
    };
    if query.is_empty() || limit == 0 {
        return Ok(Some(result));
    }
    // instr is literal, unlike LIKE or MATCH. For Unicode case-insensitive
    // matching use Rust's lowercasing: SQLite lower() only folds ASCII.
    let mut statement = db.conn().prepare(
        "SELECT f.rel_path, l.line, l.col, l.content, l.enclosing_symbol
         FROM code_literals l JOIN code_files f ON f.id = l.file_id
         WHERE (?1 IS NULL OR instr(f.rel_path, ?1) > 0)
           AND (?2 OR instr(l.content, ?3) > 0)
         ORDER BY f.rel_path, l.line, l.col, l.id",
    )?;
    let rows = statement.query_map(rusqlite::params![file, ignore_case, query], |row| {
        Ok(TextHit {
            file: row.get(0)?,
            line: row.get::<_, u32>(1)?.saturating_add(1),
            column: row.get::<_, u32>(2)?.saturating_add(1),
            content: row.get(3)?,
            enclosing_symbol: row.get(4)?,
        })
    })?;
    let folded = ignore_case.then(|| query.to_lowercase());
    for row in rows {
        let hit = row?;
        if folded
            .as_ref()
            .is_some_and(|query| !hit.content.to_lowercase().contains(query))
        {
            continue;
        }
        if result.hits.len() == limit {
            result.truncated = true;
            break;
        }
        result.hits.push(hit);
    }
    Ok(Some(result))
}

/// A canonical parsed-value result. The legacy `TextSearchResult` wire shape
/// above stays unchanged for `--scope text|strings` callers.
#[derive(Debug, Serialize)]
pub struct LiteralSearchResult {
    pub scope: &'static str,
    pub unit: &'static str,
    pub status: &'static str,
    pub hits: Vec<LiteralHit>,
    pub truncated: bool,
    pub coverage: LiteralCoverage,
    pub freshness: LiteralFreshness,
}

#[derive(Debug, Serialize)]
pub struct LiteralHit {
    pub repo: String,
    #[serde(flatten)]
    pub location: TextHit,
    pub matched_patterns: Vec<usize>,
}

#[derive(Debug, Serialize)]
pub struct LiteralCoverage {
    pub indexed_files: usize,
    pub checked_files: usize,
    pub stale_files: Vec<String>,
    pub failed_files: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct LiteralFreshness {
    pub state: &'static str,
}

/// Search indexed AST values, with one OR pass and a direct working-tree
/// reconciliation. A failed or bounded reconciliation is `unknown`, never a
/// clean negative. The old literal search remains a separate compatibility path.
pub fn search_canonical(
    root: &Path,
    patterns: &[String],
    ignore_case: bool,
    path: Option<&str>,
    limit: usize,
) -> Result<LiteralSearchResult> {
    if patterns.is_empty() || patterns.len() > 64 || patterns.iter().any(|p| p.trim().is_empty()) {
        return Err(crate::error::Error::other(
            "literals need 1..64 nonempty patterns",
        ));
    }
    if limit == 0 || limit > 1_000 {
        return Err(crate::error::Error::other("literal limit must be 1..=1000"));
    }
    let root = crate::domain::canonicalize_plain(root)?;
    let selected = if let Some(path) = path {
        let given = Path::new(path);
        let target = crate::domain::canonicalize_plain(&if given.is_absolute() {
            given.to_path_buf()
        } else {
            root.join(given)
        })?;
        if !target.starts_with(&root) {
            return Err(crate::error::Error::other(
                "literal path escapes repository root",
            ));
        }
        Some(
            target
                .strip_prefix(&root)
                .unwrap_or(Path::new(""))
                .to_path_buf(),
        )
    } else {
        None
    };
    let mut result = LiteralSearchResult {
        scope: "literals",
        unit: "ast_literal",
        status: "unavailable",
        hits: Vec::new(),
        truncated: false,
        coverage: LiteralCoverage {
            indexed_files: 0,
            checked_files: 0,
            stale_files: Vec::new(),
            failed_files: Vec::new(),
        },
        freshness: LiteralFreshness { state: "unknown" },
    };
    let db_path = root.join(".mdkb/code.sqlite");
    if !db_path.exists() {
        return Ok(result);
    }
    let db = CodeDb::open_read_only(db_path)?;
    let available: bool = db.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'code_literals')",
        [], |row| row.get(0),
    )?;
    if !available {
        return Ok(result);
    }
    let config = Config::load_or_default(&root.join(".mdkb/config.toml"));
    let hashes = db.get_file_hashes()?;
    result.coverage.indexed_files = hashes.len();
    let mut observed = HashSet::new();
    let mut bytes_checked = 0_u64;
    let mut entries_seen = 0_usize;
    let options = || WalkOptions {
        root: &root,
        ignore_patterns: &config.code.indexing.ignore_patterns,
        respect_gitignore: config.code.indexing.respect_gitignore,
    };
    let mut walks = vec![walker::walk_builder(options()).build()];
    if config.code.indexing.respect_gitignore {
        let forced = walker::parse_mdkb_index_annotations(&root);
        if !forced.is_empty() {
            match walker::forced_walk_builder(&root, &forced) {
                Some(builder) => walks.push(builder.build()),
                None => result
                    .coverage
                    .failed_files
                    .push("invalid force-include policy".into()),
            }
        }
    }
    'walks: for walk in walks {
        for entry in walk {
            entries_seen += 1;
            if entries_seen > 100_000 {
                result
                    .coverage
                    .failed_files
                    .push("index freshness walk exceeded 100000 entries".into());
                break 'walks;
            }
            let file = match entry {
                Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                    entry.into_path()
                }
                Ok(_) => continue,
                Err(error) => {
                    result.coverage.failed_files.push(error.to_string());
                    continue;
                }
            };
            if crate::code::parsing::language::Language::from_path(&file).is_none() {
                continue;
            }
            let rel = crate::domain::rel_key(file.strip_prefix(&root).unwrap_or(&file));
            if selected
                .as_ref()
                .is_some_and(|prefix| !Path::new(&rel).starts_with(prefix))
            {
                continue;
            }
            if !observed.insert(rel.clone()) {
                continue;
            }
            let bytes = match std::fs::read(&file) {
                Ok(bytes) => bytes,
                Err(error) => {
                    result.coverage.failed_files.push(format!("{rel}: {error}"));
                    continue;
                }
            };
            bytes_checked += bytes.len() as u64;
            if bytes_checked > 64 * 1024 * 1024 {
                result
                    .coverage
                    .failed_files
                    .push("index freshness check exceeded 64 MiB".into());
                break 'walks;
            }
            let content = match std::str::from_utf8(&bytes) {
                Ok(content) => content,
                Err(_) => {
                    result
                        .coverage
                        .failed_files
                        .push(format!("{rel}: invalid UTF-8"));
                    continue;
                }
            };
            result.coverage.checked_files += 1;
            if hashes
                .get(&rel)
                .is_none_or(|hash| *hash != crate::code::indexing::hasher::content_hash(content))
            {
                result.coverage.stale_files.push(rel);
            }
        }
    }
    for indexed in hashes.keys() {
        if selected
            .as_ref()
            .is_none_or(|prefix| Path::new(indexed).starts_with(prefix))
            && !observed.contains(indexed)
        {
            result.coverage.stale_files.push(indexed.clone());
        }
    }
    result.freshness.state = if !result.coverage.failed_files.is_empty() {
        "unknown"
    } else if result.coverage.stale_files.is_empty() {
        "current"
    } else {
        "stale"
    };
    result.status = if result.freshness.state == "current" {
        "complete"
    } else {
        "partial"
    };
    let folded = ignore_case.then(|| {
        patterns
            .iter()
            .map(|pattern| pattern.to_lowercase())
            .collect::<Vec<_>>()
    });
    let mut statement = db.conn().prepare(
        "SELECT f.rel_path, l.line, l.col, l.content, l.enclosing_symbol
         FROM code_literals l JOIN code_files f ON f.id = l.file_id
         ORDER BY f.rel_path, l.line, l.col, l.id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(TextHit {
            file: row.get(0)?,
            line: row.get::<_, u32>(1)?.saturating_add(1),
            column: row.get::<_, u32>(2)?.saturating_add(1),
            content: row.get(3)?,
            enclosing_symbol: row.get(4)?,
        })
    })?;
    for row in rows {
        let hit = row?;
        if selected
            .as_ref()
            .is_some_and(|prefix| !Path::new(&hit.file).starts_with(prefix))
        {
            continue;
        }
        let value = if ignore_case {
            hit.content.to_lowercase()
        } else {
            hit.content.clone()
        };
        let matched: Vec<usize> = patterns
            .iter()
            .enumerate()
            .filter_map(|(i, pattern)| {
                let needle = folded
                    .as_ref()
                    .map_or(pattern.as_str(), |folded| folded[i].as_str());
                value.contains(needle).then_some(i)
            })
            .collect();
        if matched.is_empty() {
            continue;
        }
        if result.hits.len() == limit {
            result.truncated = true;
            break;
        }
        result.hits.push(LiteralHit {
            repo: root.display().to_string(),
            location: hit,
            matched_patterns: matched,
        });
    }
    Ok(result)
}

/// Merge per-root canonical results without treating an unreadable root as
/// an empty corpus. Both public adapters use this global limit and coverage.
pub fn aggregate_canonical_roots(
    outcomes: Vec<(
        std::path::PathBuf,
        std::result::Result<LiteralSearchResult, String>,
    )>,
    excluded_by_scope: usize,
    limit: usize,
) -> serde_json::Value {
    let mut hits = Vec::new();
    let mut roots = Vec::new();
    let mut any_available = false;
    let mut all_complete = excluded_by_scope == 0;
    let mut unknown = excluded_by_scope > 0;
    let mut stale = false;
    let mut truncated = false;
    for (root, outcome) in outcomes {
        let root = root.display().to_string();
        match outcome {
            Ok(result) => {
                any_available |= result.status != "unavailable";
                all_complete &= result.status == "complete";
                unknown |= result.freshness.state == "unknown";
                stale |= result.freshness.state == "stale";
                truncated |= result.truncated;
                hits.extend(result.hits.into_iter().map(|hit| serde_json::json!(hit)));
                roots.push(serde_json::json!({
                    "root": root, "status": result.status,
                    "freshness": result.freshness, "coverage": result.coverage,
                }));
            }
            Err(error) => {
                all_complete = false;
                unknown = true;
                roots.push(serde_json::json!({
                    "root": root, "status": "unavailable", "reason": error,
                }));
            }
        }
    }
    truncated |= hits.len() > limit;
    hits.truncate(limit);
    let status = if all_complete {
        "complete"
    } else if any_available {
        "partial"
    } else {
        "unavailable"
    };
    let freshness = if unknown {
        "unknown"
    } else if stale {
        "stale"
    } else {
        "current"
    };
    serde_json::json!({
        "scope": "literals", "unit": "ast_literal", "status": status,
        "hits": hits, "truncated": truncated,
        "coverage": {"roots": roots, "excluded_by_scope": excluded_by_scope},
        "freshness": {"state": freshness},
    })
}

/// Shared human output for CLI and MCP. JSON uses the typed result above.
pub fn render(result: &TextSearchResult) -> String {
    use std::fmt::Write;
    let mut output = String::new();
    for hit in &result.hits {
        let _ = write!(
            output,
            "{}:{}:{}: {}",
            hit.file,
            hit.line,
            hit.column,
            hit.content.replace(['\n', '\r'], " ")
        );
        if let Some(symbol) = &hit.enclosing_symbol {
            let _ = write!(output, " (inside {symbol})");
        }
        output.push('\n');
    }
    if result.truncated {
        output.push_str("More literal hits available; increase --limit or narrow --file.\n");
    }
    if let Some(timestamp) = result.indexed_at {
        let _ = writeln!(
            output,
            "Index age: oldest file indexed at {timestamp} (Unix seconds); run `mdkb code index` to refresh."
        );
    }
    output
}
