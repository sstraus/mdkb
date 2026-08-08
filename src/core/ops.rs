//! Store operations that are neither indexing, memory, nor the code graph:
//! embeddings, evaluation, experiments, metrics, journal import and session
//! retention.
//!
//! Grouped by what they are not, deliberately: each is small, each mutates or
//! reads the store, and none of them parses an argument or formats output —
//! which is the only property that decides whether something belongs in the
//! command-line adapter.

use std::path::Path;

use crate::cli::journal::{self, JournalImportResult};
use crate::config::Config;
use crate::core::Context;
use crate::core::graph::resolve_document_id;
use crate::core::indexing::{apply_conventions, bootstrap_code_index};
use crate::core::memory_sync::gitignore_shadow;
use crate::domain::{Document, SearchQuery, SearchResult};
use crate::error::{Error, ErrorKind, Result};
use crate::store::evolution::RelationshipType;
use crate::store::{collections, documents, evolution, memory, search, stats, vectors};
use std::collections::HashSet;
use walkdir::WalkDir;

/// Generate embeddings for documents that don't have them (the `has_embedding`
/// gate is the hash gate: content changes invalidate the old embedding, so only
/// new/changed docs are re-embedded; unchanged docs are skipped).
///
/// `collection = Some(name)` embeds only that collection — the explicit path for
/// large transcript collections. `None` embeds every collection except
/// `claude_sessions` (unless `[search] auto_embed_sessions`).
///
/// Uses batch content retrieval to avoid N+1 queries and the cached model to
/// avoid 2-5 second load time per request. Per-doc timing over 1s is logged to
/// `hook-slow.jsonl`.
pub fn handle_embed(ctx: &Context, collection: Option<&str>) -> Result<EmbedResult> {
    let mut result = EmbedResult::default();

    // Use cached service to avoid reloading
    let service = crate::llm::get_cached_service()?;

    // Load config once (chunking + session-embed policy)
    let config = crate::config::Config::load_or_default(&ctx.config_path);
    let chunking_config = config.chunking.clone();
    let include_sessions = config.search.auto_embed_sessions;
    let mdkb_dir = ctx.db_path.parent().map(|p| p.to_path_buf());

    // Get all documents
    let all_collections = collections::list_collections(&ctx.conn)?;

    // Which docs already have an embedding — fetched once as a set instead of one
    // `has_embedding` query per document (PERF-1: this path runs on every flush).
    let embedded_ids = vectors::embedded_document_ids(&ctx.conn)?;

    for coll in &all_collections {
        if !should_embed_collection(&coll.name, collection, include_sessions) {
            continue;
        }
        let docs = documents::list_documents(&ctx.conn, &coll.name)?;

        // Filter to docs that need embedding
        let mut docs_needing_embedding = Vec::new();
        for doc in docs {
            if embedded_ids.contains(&doc.id) {
                result.skipped += 1;
            } else {
                docs_needing_embedding.push(doc);
            }
        }

        if docs_needing_embedding.is_empty() {
            continue;
        }

        // Batch retrieve all content in a single query (fixes N+1 query pattern)
        let hashes: Vec<&str> = docs_needing_embedding
            .iter()
            .map(|d| d.hash.as_str())
            .collect();
        let content_map = documents::get_content_batch(&ctx.conn, &hashes)?;

        // Single-chunk docs (the common case for memory/small docs) are collected
        // and embedded in batches via embed_documents instead of one embed_query
        // call each (PERF-G2). Multi-chunk docs are embedded per-doc as before.
        let mut singles: Vec<(i64, &str)> = Vec::new();

        for doc in &docs_needing_embedding {
            // Get content from batch result
            let Some(content) = content_map.get(&doc.hash) else {
                result.errors.push(format!("No content for doc {}", doc.id));
                continue;
            };

            // Split into chunks using configured strategy
            let chunks = crate::store::chunks::split(content, &chunking_config);

            if chunks.len() <= 1 {
                // Small doc: defer to the batched pass below.
                singles.push((doc.id, content.as_str()));
                continue;
            }

            // Multi-chunk doc: embed each chunk (already a batched call).
            let embed_start = std::time::Instant::now();
            let texts: Vec<&str> = chunks.iter().map(|c| c.content.as_str()).collect();
            match service.embed_documents(&texts) {
                Ok(embeddings) => {
                    vectors::store_chunk_embeddings(
                        &ctx.conn,
                        doc.id,
                        &chunks,
                        &embeddings,
                        crate::llm::embeddings::MODEL_NAME,
                    )?;
                    result.generated += chunks.len();
                }
                Err(e) => {
                    result.errors.push(format!(
                        "Failed to embed doc {} ({}, {} chunks): {}",
                        doc.id,
                        doc.relative_path,
                        chunks.len(),
                        e
                    ));
                }
            }

            let elapsed_ms = embed_start.elapsed().as_millis() as u64;
            if elapsed_ms > EMBED_SLOW_MS {
                if let Some(dir) = &mdkb_dir {
                    log_slow_embed(dir, &doc.relative_path, elapsed_ms);
                }
            }
        }

        // Batched embedding for all single-chunk docs in this collection. Grouped
        // to bound the peak result vector; embed_documents also batches to the
        // model internally. Results are distributed back to docs by index.
        for batch in singles.chunks(SINGLE_DOC_EMBED_BATCH) {
            let texts: Vec<&str> = batch.iter().map(|(_, text)| *text).collect();
            match service.embed_documents(&texts) {
                Ok(embeddings) => {
                    for ((doc_id, _), embedding) in batch.iter().zip(embeddings) {
                        vectors::store_embedding(
                            &ctx.conn,
                            *doc_id,
                            &embedding,
                            crate::llm::embeddings::MODEL_NAME,
                        )?;
                        result.generated += 1;
                    }
                }
                Err(e) => {
                    for (doc_id, _) in batch {
                        result
                            .errors
                            .push(format!("Failed to embed doc {doc_id}: {e}"));
                    }
                }
            }
        }
    }

    Ok(result)
}
/// Result of embedding generation.
#[derive(Debug, Clone, Default)]
pub struct EmbedResult {
    /// Number of embeddings generated.
    pub generated: usize,
    /// Number of documents skipped (already have embeddings).
    pub skipped: usize,
    /// Errors encountered.
    pub errors: Vec<String>,
}
/// Handle `mdkb eval recall` — seed a fixture DB and score recall@k / MRR.
pub fn handle_eval_recall(
    fixture: Option<&std::path::Path>,
    k: usize,
) -> Result<crate::eval::recall::RecallReport> {
    let fx = match fixture {
        Some(p) => crate::eval::fixture::Fixture::load(p)?,
        None => crate::eval::fixture::Fixture::bundled()?,
    };
    let conn = fx.seed_db()?;
    crate::eval::recall::run_recall(&conn, &fx.recall_cases(), k)
}
/// Handle `mdkb eval judge` — seed a fixture DB and score answer support.
pub fn handle_eval_judge(
    fixture: Option<&std::path::Path>,
    k: usize,
) -> Result<crate::eval::judge::JudgeReport> {
    let fx = match fixture {
        Some(p) => crate::eval::fixture::Fixture::load(p)?,
        None => crate::eval::fixture::Fixture::bundled()?,
    };
    let conn = fx.seed_db()?;
    crate::eval::judge::run_judge(
        &conn,
        &fx.judge_cases(),
        k,
        &crate::eval::judge::SubstringJudge,
    )
}
/// Handle `mdkb history` command - show evolution chain.
pub fn handle_history(ctx: &Context, path_or_id: &str) -> Result<Vec<EvolutionHistoryEntry>> {
    let doc_id = resolve_document_id(ctx, path_or_id)?;

    // Get what this document supersedes/extends
    let forward = evolution::get_evolution_chain(&ctx.conn, doc_id)?;

    // Get what supersedes/extends this document
    let backward = evolution::get_superseded_by(&ctx.conn, doc_id)?;

    let mut history = Vec::new();

    // Add backward relationships (what superseded this)
    for evo in backward {
        if let Some(doc) = documents::get_document(&ctx.conn, evo.source_doc_id)? {
            history.push(EvolutionHistoryEntry {
                doc_id: doc.id,
                path: doc.relative_path,
                title: doc.title,
                relationship: format!("{} (newer)", evo.relationship),
                scope: evo.scope,
                reason: evo.reason,
            });
        }
    }

    // Add forward relationships (what this supersedes)
    for evo in forward {
        if let Some(doc) = documents::get_document(&ctx.conn, evo.target_doc_id)? {
            history.push(EvolutionHistoryEntry {
                doc_id: doc.id,
                path: doc.relative_path,
                title: doc.title,
                relationship: format!("{} (older)", evo.relationship),
                scope: evo.scope,
                reason: evo.reason,
            });
        }
    }

    Ok(history)
}
/// Handle `mdkb current` command - find current version of superseded doc.
pub fn handle_current(ctx: &Context, path_or_id: &str) -> Result<Option<Document>> {
    let doc_id = resolve_document_id(ctx, path_or_id)?;

    // Follow the supersession chain until we find a current document
    let mut current_id = doc_id;
    let mut visited = std::collections::HashSet::new();

    loop {
        if visited.contains(&current_id) {
            // Cycle detected, stop
            break;
        }
        visited.insert(current_id);

        // Check if current document is superseded
        let superseded_by = evolution::get_superseded_by(&ctx.conn, current_id)?;

        // Find a supersedes relationship (not just updates/corrects)
        let supersession = superseded_by
            .iter()
            .find(|e| e.relationship == RelationshipType::Supersedes);

        if let Some(evo) = supersession {
            current_id = evo.source_doc_id;
        } else {
            // No supersession, this is the current version
            break;
        }
    }

    documents::get_document(&ctx.conn, current_id)
}
/// Evolution history entry for display.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EvolutionHistoryEntry {
    pub doc_id: i64,
    pub path: String,
    pub title: Option<String>,
    pub relationship: String,
    pub scope: Option<String>,
    pub reason: Option<String>,
}
/// Handle `mdkb metrics show` command.
pub fn handle_metrics_show(ctx: &Context, period: u32) -> Result<stats::QueryMetricsSummary> {
    stats::get_query_metrics(&ctx.conn, period)
}
/// Handle `mdkb metrics latency` command.
pub fn handle_metrics_latency(ctx: &Context) -> Result<Vec<stats::QueryLatencyStats>> {
    stats::get_query_latency_stats(&ctx.conn)
}
/// Handle `mdkb metrics export` command.
pub fn handle_metrics_export(ctx: &Context, period: u32) -> Result<Vec<stats::QueryEvent>> {
    stats::export_query_events(&ctx.conn, period)
}
/// Handle `mdkb experiment create`.
pub fn handle_experiment_create(
    ctx: &Context,
    name: &str,
    description: Option<&str>,
    config_a: &str,
    config_b: &str,
    split: f64,
    min_samples: i64,
) -> Result<ExperimentCreateResult> {
    // Validate name length and format
    if name.is_empty() || name.len() > MAX_NAME_LENGTH {
        return Err(Error::from(ErrorKind::Config(format!(
            "Experiment name must be 1-{} characters",
            MAX_NAME_LENGTH
        ))));
    }
    if !name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(Error::from(ErrorKind::Config(
            "Experiment name must contain only alphanumeric characters, hyphens, and underscores"
                .to_string(),
        )));
    }

    // Validate config sizes before parsing
    if config_a.len() > MAX_CONFIG_SIZE {
        return Err(Error::from(ErrorKind::Config(format!(
            "config-a exceeds maximum size of {} bytes",
            MAX_CONFIG_SIZE
        ))));
    }
    if config_b.len() > MAX_CONFIG_SIZE {
        return Err(Error::from(ErrorKind::Config(format!(
            "config-b exceeds maximum size of {} bytes",
            MAX_CONFIG_SIZE
        ))));
    }

    // Validate JSON configs
    serde_json::from_str::<serde_json::Value>(config_a)
        .map_err(|e| Error::from(ErrorKind::Config(format!("Invalid config-a JSON: {e}"))))?;
    serde_json::from_str::<serde_json::Value>(config_b)
        .map_err(|e| Error::from(ErrorKind::Config(format!("Invalid config-b JSON: {e}"))))?;

    // Validate split
    if !(0.0..=1.0).contains(&split) {
        return Err(Error::from(ErrorKind::Config(
            "Traffic split must be between 0.0 and 1.0".to_string(),
        )));
    }

    // Validate min_samples
    if !(1..=10_000).contains(&min_samples) {
        return Err(Error::from(ErrorKind::Config(
            "min_samples must be between 1 and 10,000".to_string(),
        )));
    }

    // Initialize schema if needed
    stats::init_experiments_schema(&ctx.conn)?;

    let id = stats::create_experiment(
        &ctx.conn,
        name,
        description,
        config_a,
        config_b,
        split,
        min_samples,
    )?;

    Ok(ExperimentCreateResult {
        id,
        name: name.to_string(),
    })
}
/// Handle `mdkb experiment status`.
pub fn handle_experiment_status(
    ctx: &Context,
    name: &str,
) -> Result<Option<stats::ExperimentStatusReport>> {
    stats::get_experiment_status(&ctx.conn, name)
}
/// Handle `mdkb experiment end`.
pub fn handle_experiment_end(
    ctx: &Context,
    name: &str,
    winner: Option<&str>,
) -> Result<Option<String>> {
    stats::init_experiments_schema(&ctx.conn)?;

    // If winner not specified, try to auto-determine from significance
    let actual_winner = if winner.is_some() {
        winner.map(|s| s.to_string())
    } else {
        // Get experiment status and check for significant winner
        match stats::get_experiment_status(&ctx.conn, name)? {
            Some(status) => {
                if let Some(sig) = status.significance {
                    sig.winner
                } else {
                    None
                }
            }
            None => {
                return Err(Error::from(ErrorKind::Config(format!(
                    "Experiment '{name}' not found"
                ))));
            }
        }
    };

    stats::end_experiment(&ctx.conn, name, actual_winner.as_deref())?;

    Ok(actual_winner)
}
/// Handle `mdkb experiment cancel`.
pub fn handle_experiment_cancel(ctx: &Context, name: &str) -> Result<()> {
    stats::init_experiments_schema(&ctx.conn)?;
    stats::cancel_experiment(&ctx.conn, name)
}
/// Handle `mdkb experiment list`.
pub fn handle_experiment_list(ctx: &Context, running_only: bool) -> Result<Vec<stats::Experiment>> {
    if running_only {
        stats::get_active_experiments(&ctx.conn)
    } else {
        stats::list_experiments(&ctx.conn)
    }
}
/// Result of creating an experiment.
#[derive(Debug, Clone)]
pub struct ExperimentCreateResult {
    pub id: i64,
    pub name: String,
}
/// Handle `mdkb journal import`.
pub fn handle_journal_import(
    ctx: &Context,
    path: &Path,
    dry_run: bool,
) -> Result<JournalImportResult> {
    handle_journal_import_from(ctx, path, path, dry_run)
}

/// Import a journal read from `path` while preserving the caller-visible path
/// in entry provenance and structured output.
pub fn handle_journal_import_from(
    ctx: &Context,
    path: &Path,
    source_path: &Path,
    dry_run: bool,
) -> Result<JournalImportResult> {
    // Read journal file
    let content = std::fs::read_to_string(path).map_err(|e| Error::from(ErrorKind::IoError(e)))?;

    // Parse journal
    let parsed = journal::parse_journal(&content);

    // Generate base ID from filename
    let base_id = journal::path_to_base_id(source_path);

    // Convert to memory entries
    let entries = journal::journal_to_memory_entries(&parsed, source_path, &base_id);

    let mut result = JournalImportResult {
        source_path: source_path.to_string_lossy().to_string(),
        ..Default::default()
    };

    if entries.is_empty() {
        result
            .skipped
            .push(("No content".to_string(), "entire file".to_string()));
        return Ok(result);
    }

    for entry in entries {
        if dry_run {
            result.created.push(entry.id);
        } else {
            // Check if entry with this ID already exists
            if memory::get_entry_without_tracking(&ctx.conn, &entry.id)?.is_some() {
                result
                    .skipped
                    .push(("Already exists".to_string(), entry.id));
                continue;
            }

            memory::add_entry(&ctx.conn, &entry)?;
            result.created.push(entry.id);
        }
    }

    Ok(result)
}
/// Handle `mdkb journal import-all`.
pub fn handle_journal_import_all(
    ctx: &Context,
    dir: &Path,
    dry_run: bool,
    skip_existing: bool,
) -> Result<Vec<JournalImportResult>> {
    handle_journal_import_all_from(ctx, dir, dir, dry_run, skip_existing)
}

/// Import a directory while retaining the path spelling supplied by the CLI.
pub fn handle_journal_import_all_from(
    ctx: &Context,
    dir: &Path,
    source_dir: &Path,
    dry_run: bool,
    skip_existing: bool,
) -> Result<Vec<JournalImportResult>> {
    let mut results = Vec::new();

    // Get list of already imported source paths if skip_existing
    let existing_sources: HashSet<String> = if skip_existing && !dry_run {
        memory::list_entries(&ctx.conn, 10000, None)?
            .into_iter()
            .filter_map(|e| e.source_path)
            .collect()
    } else {
        HashSet::new()
    };

    // Walk journal directory
    for entry in WalkDir::new(dir)
        .max_depth(2)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "md"))
    {
        let path = entry.path();
        let source_path = path
            .strip_prefix(dir)
            .map(|relative| source_dir.join(relative))
            .unwrap_or_else(|_| path.to_path_buf());
        let path_str = source_path.to_string_lossy().to_string();

        // Skip if already imported
        if skip_existing && existing_sources.contains(&path_str) {
            results.push(JournalImportResult {
                source_path: path_str.clone(),
                skipped: vec![("Already imported".to_string(), path_str)],
                ..Default::default()
            });
            continue;
        }

        match handle_journal_import_from(ctx, path, &source_path, dry_run) {
            Ok(result) => results.push(result),
            Err(e) => {
                results.push(JournalImportResult {
                    source_path: path_str.clone(),
                    skipped: vec![(format!("Error: {}", e), path_str)],
                    ..Default::default()
                });
            }
        }
    }

    Ok(results)
}
/// Hard-delete archived session documents whose `indexed_at <= cutoff_ts`.
///
/// When `export_dir` is set, each transcript is written as markdown BEFORE it
/// is deleted, so mdkb never silently drops the only remaining copy. Only
/// `archived` sessions (source jsonl already gone) are eligible — current
/// transcripts are untouched.
pub fn handle_prune_sessions(
    ctx: &Context,
    cutoff_ts: i64,
    export_dir: Option<&Path>,
) -> Result<PruneSessionsSummary> {
    let candidates = documents::list_prunable_sessions(&ctx.conn, cutoff_ts)?;
    let mut summary = PruneSessionsSummary {
        pruned: 0,
        exported: 0,
        export_dir: export_dir.map(|p| p.display().to_string()),
    };

    if let Some(dir) = export_dir {
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::from(ErrorKind::Io {
                path: dir.to_path_buf(),
                operation: format!("create export dir: {e}"),
            })
        })?;
    }

    let hashes: Vec<&str> = candidates.iter().map(|c| c.hash.as_str()).collect();
    let content_map = documents::get_content_batch(&ctx.conn, &hashes)?;

    for c in &candidates {
        // Export first — a delete before a successful write would lose the only
        // copy. If `--export` is requested but the content body is missing, SKIP
        // the delete (leave the candidate for a future run) rather than silently
        // destroying an unexported transcript (DATA-1).
        if let Some(dir) = export_dir {
            let Some(body) = content_map.get(&c.hash) else {
                tracing::warn!(
                    "prune-sessions: content missing for '{}' (id {}) — skipping delete \
                     to avoid destroying the only copy",
                    c.relative_path,
                    c.id
                );
                continue;
            };
            let safe: String = c
                .relative_path
                .chars()
                .map(|ch| {
                    if ch.is_alphanumeric() || ch == '-' || ch == '_' {
                        ch
                    } else {
                        '_'
                    }
                })
                .collect();
            // Collision-proof: the lossy sanitizer above collapses distinct paths
            // (`a/b.md`, `a_b.md`, `a:b.md`) to the same stem. Suffix with the unique
            // document id (plus a hash prefix for human legibility) so two candidates
            // can never overwrite each other's export — another silent-loss path.
            let hash8 = c.hash.get(..8).unwrap_or(c.hash.as_str());
            let fname = format!("{safe}-{}-{hash8}.md", c.id);
            let path = dir.join(&fname);
            let title = c.title.clone().unwrap_or_else(|| c.relative_path.clone());
            let md = format!("# {title}\n\n{body}\n");
            std::fs::write(&path, md).map_err(|e| {
                Error::from(ErrorKind::Io {
                    path: path.clone(),
                    operation: format!("write export: {e}"),
                })
            })?;
            summary.exported += 1;
        }
        if documents::delete_document(&ctx.conn, c.id)? {
            summary.pruned += 1;
        }
    }
    Ok(summary)
}
/// Outcome of a `mdkb compact --prune-sessions` run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PruneSessionsSummary {
    pub pruned: usize,
    pub exported: usize,
    pub export_dir: Option<String>,
}
/// Parse a retention duration like `90d`, `12h`, or `2w` into seconds.
pub fn parse_retention_secs(s: &str) -> Result<i64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_alphabetic()).ok_or_else(|| {
        Error::other(format!(
            "Invalid duration '{s}': expected <N><unit> like 90d"
        ))
    })?;
    let (num, unit) = s.split_at(split);
    let n: i64 = num
        .trim()
        .parse()
        .map_err(|_| Error::other(format!("Invalid duration number in '{s}'")))?;
    if n < 0 {
        return Err(Error::other(format!(
            "Duration must be non-negative: '{s}'"
        )));
    }
    let mult = match unit.trim() {
        "d" => 86_400,
        "h" => 3_600,
        "w" => 604_800,
        other => {
            return Err(Error::other(format!(
                "Invalid duration unit '{other}' in '{s}': use d, h, or w"
            )));
        }
    };
    // Checked: a fat-fingered oversized value (extra digits) must NOT wrap to a
    // small/negative number in release builds — that would corrupt the prune
    // cutoff and make a destructive `--prune-sessions` delete far more than
    // intended (SEC-1).
    n.checked_mul(mult).ok_or_else(|| {
        Error::other(format!(
            "Duration '{s}' is too large (overflows). Use a smaller value."
        ))
    })
}
/// Handle `mdkb search` command.
pub fn handle_search(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
) -> Result<Vec<crate::domain::SearchResult>> {
    let query = SearchQuery {
        text: query_text.to_string(),
        limit,
        collection: collection.map(String::from),
        tags: vec![],
        include_superseded: false,
    };

    search::search(&ctx.conn, &query)
}
/// Handle `mdkb vsearch` command - vector semantic search.
///
/// Uses batch document retrieval to avoid N+1 queries.
/// Uses cached model to avoid 2-5 second load time per request.
pub fn handle_vsearch(
    ctx: &Context,
    query_text: &str,
    limit: usize,
    collection: Option<&str>,
) -> Result<Vec<SearchResult>> {
    // Use cached service to avoid reloading
    let service = crate::llm::get_cached_service()?;

    // Generate query embedding
    let query_embedding = service.embed_query(query_text)?;

    // Perform vector search - get more results to account for collection filtering
    let fetch_limit = if collection.is_some() {
        limit * 2
    } else {
        limit
    };
    let vector_results = vectors::vector_search(&ctx.conn, &query_embedding, fetch_limit)?;

    if vector_results.is_empty() {
        return Ok(Vec::new());
    }

    // Batch retrieve all documents in a single query (fixes N+1 query pattern)
    let doc_ids: Vec<i64> = vector_results.iter().map(|(id, _)| *id).collect();
    let docs = documents::get_documents_batch(&ctx.conn, &doc_ids)?;

    // Build a map for quick lookup
    let doc_map: std::collections::HashMap<i64, _> = docs.into_iter().map(|d| (d.id, d)).collect();

    // Convert to SearchResult format, preserving order from vector search
    let mut results = Vec::new();
    for (doc_id, distance) in vector_results {
        if let Some(doc) = doc_map.get(&doc_id) {
            // Filter by collection if specified; exclude claude_sessions from default search
            if let Some(coll) = collection {
                if doc.collection != coll {
                    continue;
                }
            } else if doc.collection == crate::domain::COLLECTION_CLAUDE_SESSIONS {
                continue;
            }

            results.push(SearchResult {
                id: doc.id,
                collection: doc.collection.clone(),
                path: doc.relative_path.clone(),
                title: doc.title.clone(),
                score: 1.0 - f64::from(distance), // Convert distance to similarity
                snippets: vec![],
                status: None,
                superseded_by: None,
                repo_root: None,
            });

            // Stop once we have enough results
            if results.len() >= limit {
                break;
            }
        }
    }

    Ok(results)
}
/// Handle `mdkb get` command.
///
/// Resolution order: numeric ID → file path → memory slug → fallback.
pub fn handle_get(ctx: &Context, id_or_path: &str, lines: Option<&str>) -> Result<GetResult> {
    // Try numeric ID first
    if let Ok(id) = id_or_path.parse::<i64>() {
        if let Some(doc) = documents::get_document(&ctx.conn, id)? {
            let content = get_document_content(ctx, &doc, lines)?;
            return Ok(GetResult::Document(doc, content));
        }
    }

    // Path resolution across collections. The path-like branch and the old
    // fallback both scanned every collection with the same call, so a not-found
    // path-like id (contains '/' or '.') scanned twice (BUG-E2). Run the scan
    // exactly once: for path-like ids here, for the rest in the fallback below.
    let path_like = id_or_path.contains('/') || id_or_path.contains('.');
    if path_like {
        let all_colls = collections::list_collections(&ctx.conn)?;
        for coll in &all_colls {
            if let Some(doc) = documents::get_document_by_path(&ctx.conn, &coll.name, id_or_path)? {
                let content = get_document_content(ctx, &doc, lines)?;
                return Ok(GetResult::Document(doc, content));
            }
        }
    }

    // Try memory slug
    if let Some(entry) = crate::store::memory::get_entry_without_tracking(&ctx.conn, id_or_path)? {
        return Ok(GetResult::Memory(entry));
    }

    // Fallback for non-path-like ids: try all collections without a path hint.
    if !path_like {
        let all_colls = collections::list_collections(&ctx.conn)?;
        for coll in &all_colls {
            if let Some(doc) = documents::get_document_by_path(&ctx.conn, &coll.name, id_or_path)? {
                let content = get_document_content(ctx, &doc, lines)?;
                return Ok(GetResult::Document(doc, content));
            }
        }
    }

    Err(Error::from(ErrorKind::DocumentNotFound {
        id: id_or_path.to_string(),
    }))
}
/// Result of a `get` command: document or memory entry.
#[derive(Debug)]
pub enum GetResult {
    /// A document with its content.
    Document(crate::domain::Document, String),
    /// A memory entry.
    Memory(crate::store::memory::MemoryEntry),
}
/// Handle `mdkb init` command.
pub fn handle_init(root: impl AsRef<Path>) -> Result<()> {
    let root = root.as_ref();
    let mdkb_dir = root.join(".mdkb");

    if mdkb_dir.exists() {
        return Err(Error::other(format!(
            "mdkb already initialized at {}",
            mdkb_dir.display()
        )));
    }

    let ctx = Context::init(root)?;
    apply_conventions(&ctx, root)?;

    // The store's own .gitignore is inert under a blanket `.mdkb/` rule above
    // it, and that failure is silent — say so at the one moment the user is
    // looking at setup output.
    if let Some(warning) = gitignore_shadow(root, &ctx.memory_dir().join("entries")) {
        eprintln!("Warning: {warning}");
    }

    let config = Config::load_or_default(mdkb_dir.join("config.toml"));
    if config.code.enabled {
        bootstrap_code_index(root);
    }

    Ok(())
}
/// Get document content, applying optional line range.
fn get_document_content(
    ctx: &Context,
    doc: &crate::domain::Document,
    lines: Option<&str>,
) -> Result<String> {
    let content = documents::get_content(&ctx.conn, &doc.hash)?.ok_or_else(|| {
        Error::from(ErrorKind::DocumentNotFound {
            id: doc.id.to_string(),
        })
    })?;

    if let Some(range) = lines {
        apply_line_range(&content, range)
    } else {
        Ok(content)
    }
}
/// Append a slow-embedding record to `.mdkb/hook-slow.jsonl` (best-effort,
/// size-rotated via [`crate::mcp::dispatch::append_hook_log`]).
pub(crate) fn log_slow_embed(mdkb_dir: &Path, doc_path: &str, elapsed_ms: u64) {
    let ts = chrono::Utc::now().timestamp();
    let mut line = serde_json::json!({
        "ts": ts,
        "event": "embed",
        "doc": doc_path,
        "elapsed_ms": elapsed_ms,
    })
    .to_string();
    line.push('\n');
    crate::mcp::dispatch::append_hook_log(&mdkb_dir.join("hook-slow.jsonl"), &line);
}
/// Whether a collection should be embedded given the `--collection` filter and
/// the `auto_embed_sessions` setting.
///
/// - `filter = Some(name)`: embed only that collection (the only way to embed
///   `claude_sessions`).
/// - `filter = None`: embed everything except `claude_sessions`, unless
///   `include_sessions` (from `[search] auto_embed_sessions`) is set.
pub(crate) fn should_embed_collection(
    coll_name: &str,
    filter: Option<&str>,
    include_sessions: bool,
) -> bool {
    match filter {
        Some(name) => coll_name == name,
        None => include_sessions || coll_name != crate::domain::COLLECTION_CLAUDE_SESSIONS,
    }
}
/// Apply line range (e.g., "10:50") to content.
pub(crate) fn apply_line_range(content: &str, range: &str) -> Result<String> {
    let parts: Vec<&str> = range.split(':').collect();
    if parts.len() != 2 {
        return Err(Error::other(format!(
            "Invalid line range format: '{}', expected 'start:end'",
            range
        )));
    }

    let start: usize = parts[0]
        .parse()
        .map_err(|_| Error::other(format!("Invalid start line: '{}'", parts[0])))?;
    let end: usize = parts[1]
        .parse()
        .map_err(|_| Error::other(format!("Invalid end line: '{}'", parts[1])))?;

    if start == 0 {
        return Err(Error::other("Line numbers start at 1"));
    }
    if end < start {
        return Err(Error::other(format!(
            "End line ({}) must be >= start line ({})",
            end, start
        )));
    }

    let lines: Vec<&str> = content.lines().collect();
    let start_idx = start.saturating_sub(1);
    let end_idx = end.min(lines.len());

    if start_idx >= lines.len() {
        return Ok(String::new());
    }

    Ok(lines[start_idx..end_idx].join("\n"))
}
/// A doc embedding taking longer than this is logged to `hook-slow.jsonl`.
const EMBED_SLOW_MS: u64 = 1000;
/// Maximum size for experiment JSON configs (10KB).
const MAX_CONFIG_SIZE: usize = 10_000;
/// Maximum length for experiment names.
const MAX_NAME_LENGTH: usize = 100;
/// Single-chunk documents are embedded in groups of this size (PERF-G2) to bound
/// the peak result vector; `embed_documents` also batches to the model internally.
const SINGLE_DOC_EMBED_BATCH: usize = 256;
