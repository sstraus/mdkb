//! Transport-independent search capability validation.
//! Adapters keep the spelling and output of legacy requests; this validator
//! rejects only combinations introduced by the canonical contract.

use std::path::{Path, PathBuf};

use crate::core::Context;
use crate::core::memory::handle_memory_search;
use crate::core::search::handle_hybrid_search;
use crate::error::{Error, Result};

/// Search each selected indexed store once, then apply a single global limit.
/// Each hit keeps its repository because document IDs and memory IDs are local
/// to a store. Indexed readers do not establish working-tree freshness.
#[allow(clippy::too_many_arguments)]
pub fn search_indexed_roots<F>(
    roots: &[PathBuf],
    excluded_by_scope: usize,
    scope: &str,
    query: &str,
    limit: usize,
    collection: Option<&str>,
    include_superseded: bool,
    entry_type: Option<&str>,
    min_confidence: Option<f64>,
    mut open: F,
) -> serde_json::Value
where
    F: FnMut(&Path) -> std::result::Result<Context, String>,
{
    let mut hits = Vec::new();
    let mut coverage = Vec::new();
    let mut searched = 0;
    let mut truncated = false;
    let fetch_limit = limit.saturating_add(1);
    for root in roots {
        let result = open(root).and_then(|ctx| {
            let docs = if scope != "memory" {
                handle_hybrid_search(&ctx, query, fetch_limit, collection, include_superseded)
                    .map_err(|error| error.to_string())?
            } else {
                Vec::new()
            };
            let memories = if scope != "docs" {
                let mut candidate_limit = fetch_limit;
                let total = if min_confidence.is_some_and(|min| min > 0.0) {
                    crate::store::memory::count_entries(&ctx.conn)
                        .map_err(|error| error.to_string())?
                } else {
                    candidate_limit
                };
                loop {
                    let candidates = handle_memory_search(&ctx, query, candidate_limit, entry_type)
                        .map_err(|error| error.to_string())?;
                    let exhausted = candidate_limit >= total;
                    let eligible = candidates
                        .into_iter()
                        .filter(|entry| min_confidence.is_none_or(|min| entry.confidence() >= min))
                        .collect::<Vec<_>>();
                    if eligible.len() >= fetch_limit || exhausted {
                        break eligible;
                    }
                    candidate_limit = candidate_limit.saturating_mul(2).min(total);
                }
            } else {
                Vec::new()
            };
            Ok((docs, memories))
        });
        let repo = root.display().to_string();
        match result {
            Ok((docs, memories)) => {
                searched += 1;
                coverage.push(serde_json::json!({
                    "root": repo, "status": "partial", "freshness": {"state": "unknown"},
                    "reason": "indexed corpus freshness not verified",
                }));
                truncated |= docs.len().saturating_add(memories.len()) > fetch_limit - 1;
                for doc in docs {
                    if hits.len() > limit {
                        break;
                    }
                    let mut hit = serde_json::json!({"repo": repo, "value": doc});
                    if scope == "knowledge" {
                        hit["corpus"] = serde_json::json!("docs");
                    }
                    hits.push(hit);
                }
                for entry in memories {
                    if hits.len() > limit {
                        break;
                    }
                    let mut hit = serde_json::json!({"repo": repo, "value": entry});
                    if scope == "knowledge" {
                        hit["corpus"] = serde_json::json!("memory");
                    }
                    hits.push(hit);
                }
            }
            Err(reason) => coverage.push(serde_json::json!({
                "root": repo, "status": "unavailable", "reason": reason,
            })),
        }
    }
    truncated |= hits.len() > limit;
    hits.truncate(limit);
    let unit = match scope {
        "docs" => "document",
        "memory" => "memory_entry",
        _ => "knowledge_result",
    };
    serde_json::json!({
        "scope": scope, "unit": unit,
        "status": if searched == 0 { "unavailable" } else { "partial" },
        "hits": hits, "truncated": truncated,
        "coverage": {"roots": coverage, "excluded_by_scope": excluded_by_scope},
        "freshness": {"state": "unknown"},
    })
}

/// Opt-in wire envelope for indexed scopes shared by CLI and MCP adapters.
/// Neither adapter has proved repository-wide indexed freshness here.
pub fn indexed_envelope<T: serde::Serialize>(
    scope: &str,
    unit: &str,
    hits: &[T],
    limit: usize,
    matched_count: usize,
) -> serde_json::Value {
    serde_json::json!({
        "scope": scope,
        "unit": unit,
        "status": "partial",
        "hits": &hits[..hits.len().min(limit)],
        "truncated": matched_count > limit,
        "coverage": {"reason": "indexed corpus completeness not verified"},
        "freshness": {"state": "unknown"}
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    LegacyDefault,
    LegacyCode,
    LegacyText,
    Coupling,
    Docs,
    Memory,
    Symbols,
    Duplicates,
    Source,
    Literals,
    SemanticCode,
    Knowledge,
}

#[derive(Debug)]
pub struct Request<'a> {
    pub scope: Option<&'a str>,
    pub query: &'a str,
    pub patterns: Option<&'a [String]>,
    pub path: Option<&'a str>,
    pub file: Option<&'a str>,
    pub regex: bool,
    pub ignore_case: bool,
    pub contract: Option<&'a str>,
}

pub fn validate(request: Request<'_>) -> Result<Purpose> {
    let purpose = match request.scope {
        None => Purpose::LegacyDefault,
        Some("text" | "strings") => Purpose::LegacyText,
        Some("code") => Purpose::LegacyCode,
        Some("coupling") => Purpose::Coupling,
        Some("docs") => Purpose::Docs,
        Some("memory") => Purpose::Memory,
        Some("symbols") => Purpose::Symbols,
        Some("duplicates") => Purpose::Duplicates,
        Some("source") => Purpose::Source,
        Some("literals") => Purpose::Literals,
        Some("semantic-code") => Purpose::SemanticCode,
        Some("knowledge") => Purpose::Knowledge,
        Some(other) => return Err(Error::other(format!("Invalid scope: {other}"))),
    };
    let alternatives = request.patterns.unwrap_or_default();
    if request.patterns.is_some() && alternatives.is_empty() {
        return Err(Error::other("patterns must be nonempty when supplied"));
    }
    if !alternatives.is_empty() {
        if !matches!(purpose, Purpose::Source | Purpose::Literals) || !request.query.is_empty() {
            return Err(Error::other(
                "patterns require source/literals and exclude query",
            ));
        }
        if alternatives.len() > 64 || alternatives.iter().any(|pattern| pattern.trim().is_empty()) {
            return Err(Error::other("patterns need 1..64 nonempty alternatives"));
        }
    }
    if matches!(
        purpose,
        Purpose::Source | Purpose::Literals | Purpose::SemanticCode | Purpose::Knowledge
    ) && alternatives.is_empty()
        && request.query.trim().is_empty()
    {
        return Err(Error::other("canonical search needs query or patterns"));
    }
    if request.regex && purpose != Purpose::Source {
        return Err(Error::other("regex is for source only"));
    }
    if request.path.is_some()
        && !matches!(
            purpose,
            Purpose::Source | Purpose::Literals | Purpose::Symbols | Purpose::Duplicates
        )
    {
        return Err(Error::other("path is not supported for this corpus"));
    }
    if request.path.is_some() && request.file.is_some() {
        return Err(Error::other("choose path or legacy file, not both"));
    }
    if purpose == Purpose::Source && request.file.is_some() {
        return Err(Error::other("source uses path, not legacy file"));
    }
    if request.ignore_case
        && !matches!(
            purpose,
            Purpose::Source | Purpose::Literals | Purpose::LegacyText | Purpose::LegacyDefault
        )
    {
        // CLI's old -i was a no-op outside literals; it must stay a no-op on
        // previously accepted requests, while new canonical forms are strict.
        if matches!(purpose, Purpose::SemanticCode | Purpose::Knowledge) {
            return Err(Error::other("ignore_case is for source/literals only"));
        }
    }
    if let Some(version) = request.contract {
        if version != "v2" || !matches!(purpose, Purpose::Symbols | Purpose::Docs | Purpose::Memory)
        {
            return Err(Error::other(
                "contract=v2 applies only to symbols/docs/memory",
            ));
        }
    }
    Ok(purpose)
}
