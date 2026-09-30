//! Data aggregator for `mdkb stats`.
//!
//! `collect_report` gathers all data into a `StatsReport` without rendering;
//! rendering lives in `stats_render`.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::core::Context;
use crate::domain::IndexStatus;
use crate::error::Result;
use crate::store::{collections, memory, search, stats};

// ── Public data model ────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct StatsReport {
    pub header: HeaderInfo,
    pub index: IndexHealth,
    pub collections: CollectionsSummary,
    pub memory: MemorySummary,
    pub code: CodeSummary,
    pub sessions: SessionsSummary,
    pub hooks: HooksSummary,
    /// Outstanding autoheal quarantines (a corrupt index was rebuilt). Non-empty
    /// until `store::heal::sweep_expired_quarantines` retires the `*.corrupt-*`
    /// copy — a persistent, loud data-loss warning for as long as it lives.
    pub quarantine: Vec<crate::store::heal::QuarantineReport>,
    /// What recall offered and what became of it (the recall ledger).
    pub recall: RecallReport,
    /// The cheap `mdkb doctor` findings. Here as well as in `mdkb doctor`
    /// because `stats` always exits 0, and a consumer that discards the
    /// output of a failing command would never see an error finding.
    pub doctor: Vec<crate::domain::doctor::Finding>,
}

/// Labelled candidates needed before a ratio is reported. Below it, one
/// session's luck reads as a property of the floor.
pub const RECALL_MIN_LABELLED: u32 = 30;

#[derive(Debug, Default, Serialize)]
pub struct RecallReport {
    pub prompts_by_mode: std::collections::BTreeMap<String, u32>,
    /// Non-holdout candidates by cosine band and entry type.
    pub bands: Vec<RecallBand>,
    /// Holdout candidates, kept out of `bands`: they were injected on
    /// purpose, and pooling them would bias precision toward the floor.
    pub holdout: Vec<RecallBand>,
    /// Labelled non-holdout candidates.
    pub labelled: u32,
    /// `labelled` is below [`RECALL_MIN_LABELLED`]; no `precision` is given.
    pub insufficient_data: bool,
    /// The documents leg, counted apart from the memory ledger: documents are
    /// admitted by their own floor (`hooks.recall_docs_min_cosine`) and carry
    /// no outcome labels.
    pub docs: DocsAdmissions,
}

/// Prompts over the hook-log window, and how many of them injected documents.
#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct DocsAdmissions {
    pub prompts: u32,
    pub with_docs: u32,
}

/// Read the documents leg off the hook log: a `user_prompt_submit` row whose
/// `payload_blocks` names `recall_docs` injected at least one document.
pub fn docs_admissions(events: &[serde_json::Value]) -> DocsAdmissions {
    let mut docs = DocsAdmissions::default();
    for v in events
        .iter()
        .filter(|v| v.get("event").and_then(|e| e.as_str()) == Some("user_prompt_submit"))
    {
        docs.prompts += 1;
        if v.pointer("/payload_blocks/recall_docs")
            .and_then(|b| b.as_u64())
            .is_some_and(|bytes| bytes > 0)
        {
            docs.with_docs += 1;
        }
    }
    docs
}

#[derive(Debug, Serialize)]
pub struct RecallBand {
    #[serde(flatten)]
    pub counts: crate::store::recall_ledger::BandCounts,
    /// `positive / (positive + negative)` over injected labelled candidates.
    pub precision: Option<f64>,
}

/// Group ledger counts into the report. Unlabelled candidates count as
/// offered and never as negatives.
pub fn build_recall_report(
    counts: Vec<crate::store::recall_ledger::BandCounts>,
    prompts_by_mode: std::collections::BTreeMap<String, u32>,
) -> RecallReport {
    let labelled: u32 = counts
        .iter()
        .filter(|c| !c.holdout)
        .map(|c| c.labelled)
        .sum();
    let insufficient_data = labelled < RECALL_MIN_LABELLED;
    let band = |counts: crate::store::recall_ledger::BandCounts| {
        let judged = counts.positive + counts.negative;
        let precision = (!insufficient_data && judged > 0)
            .then(|| f64::from(counts.positive) / f64::from(judged));
        RecallBand { counts, precision }
    };
    let (holdout, bands): (Vec<_>, Vec<_>) = counts.into_iter().partition(|c| c.holdout);
    RecallReport {
        prompts_by_mode,
        bands: bands.into_iter().map(band).collect(),
        holdout: holdout.into_iter().map(band).collect(),
        labelled,
        insufficient_data,
        docs: DocsAdmissions::default(),
    }
}

#[derive(Debug, Serialize)]
pub struct HeaderInfo {
    /// Basename of the project root directory.
    pub repo: String,
    /// mdkb version from Cargo.toml.
    pub version: String,
    /// index.sqlite size in bytes.
    pub db_size_bytes: u64,
    /// Unix timestamp of the most-recently-indexed document.
    pub last_updated: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct IndexHealth {
    pub document_count: usize,
    pub memory_count: usize,
    /// Ratio 0.0–1.0 of free pages relative to total pages (compaction hint).
    pub free_page_ratio: f64,
}

#[derive(Debug, Serialize)]
pub struct CollectionsSummary {
    pub collections: Vec<CollectionRow>,
}

#[derive(Debug, Serialize)]
pub struct CollectionRow {
    pub name: String,
    pub path: String,
    pub pattern: String,
    pub doc_count: i64,
}

#[derive(Debug, Serialize)]
pub struct MemorySummary {
    /// Total active (non-expired, non-future-reminder) entries.
    pub active_count: usize,
    /// Counts keyed by entry_type string ("topic", "problem", etc.).
    pub counts_by_type: HashMap<String, usize>,
    /// Reminders whose due_at <= now.
    pub reminders_due: usize,
    /// Reminders due within the next 7 days (but not yet due).
    pub reminders_upcoming_7d: usize,
    /// Entries missing a stored embedding (hybrid search degrades to BM25 for
    /// these). Cleared by `mdkb update`'s backfill.
    pub pending_embeddings: usize,
    /// Entry files reconciliation refuses to absorb (merge markers, bad
    /// frontmatter, id/filename mismatch, failed validation). Inert: never
    /// imported, never searched, and they do not self-heal.
    pub files_unreadable: usize,
    /// Non-archived entries with no file on disk — present only in a database
    /// that is deliberately untracked.
    pub entries_unprojected: usize,
}

#[derive(Debug, Serialize)]
pub struct CodeSummary {
    /// Total indexed files. Zero means code.sqlite absent or empty.
    pub files: usize,
    /// Total symbols across all indexed files.
    pub symbols: usize,
    /// Total edges in the symbol graph (calls, uses, defines, implements, …).
    pub relations: usize,
    /// Unix timestamp of the most-recent `code_files.indexed_at`.
    pub last_indexed: Option<i64>,
    /// Per-language breakdown, sorted by symbol count descending.
    pub languages: Vec<LanguageRow>,
    /// Symbol counts per `kind` (Function, Method, Struct, …), desc.
    pub symbols_by_kind: Vec<KindCount>,
    /// Relation counts per `kind` (Calls, Uses, Defines, …), desc.
    pub relations_by_kind: Vec<KindCount>,
    /// Files with the most symbols (up to 8), desc.
    pub top_files: Vec<FileSymbolRow>,
}

#[derive(Debug, Serialize)]
pub struct LanguageRow {
    pub language: String,
    pub files: usize,
    pub symbols: usize,
}

#[derive(Debug, Serialize)]
pub struct KindCount {
    pub kind: String,
    pub count: usize,
}

#[derive(Debug, Serialize)]
pub struct FileSymbolRow {
    pub path: String,
    pub symbols: usize,
}

#[derive(Debug, Serialize)]
pub struct SessionsSummary {
    pub total_sessions: i64,
    pub total_calls: i64,
    /// Tool call counts for the top 10 tools (all sessions, aggregate).
    pub top_tools: Vec<ToolRow>,
}

#[derive(Debug, Serialize)]
pub struct ToolRow {
    pub tool_name: String,
    pub call_count: i64,
}

#[derive(Debug, Serialize)]
pub struct HooksSummary {
    /// Slow hook events in the last 7 days (from hook-slow.jsonl).
    pub slow_events_7d: usize,
    /// Per-event invocation stats (last 7 days, from hook-events.jsonl).
    pub events: Vec<HookEventStats>,
    /// Divergence of the live settings.json hook registrations from the
    /// canonical mdkb set (duplicated → double-fire; missing → never runs).
    pub drift: crate::cli::setup::HookDrift,
    /// Behavioral-prior mining status (Stop-hook distillation).
    pub mining: MiningStatus,
}

/// Whether prior mining is active, and why not when it isn't. Makes an
/// otherwise-invisible pipeline observable to operators.
#[derive(Debug, Serialize)]
pub struct MiningStatus {
    pub enabled: bool,
    pub reason: String,
    /// Total distilled candidates recorded so far.
    pub candidate_count: i64,
    /// What the last 7 days of mining runs actually did, most frequent first.
    /// `enabled` only says the switch is on; this says whether anything came of
    /// it. Empty when nothing has been mined in the window.
    pub outcomes_7d: Vec<MiningOutcomeCount>,
}

/// The `event` name `mine_episode` writes to `hook-events.jsonl`. Kept next to
/// the reader so the two cannot drift; the writer is `mcp::dispatch`.
pub const MINING_EVENT: &str = "prior_mining";

/// One `outcome` value from the `prior_mining` event stream, with its count and
/// the most recent reason recorded for it — a `failed` count is not actionable
/// without the error text that produced it.
#[derive(Debug, Serialize)]
pub struct MiningOutcomeCount {
    pub outcome: String,
    pub count: usize,
    pub last_reason: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HookEventStats {
    pub event: String,
    pub invocations: usize,
    pub fired: usize,
    /// Count of `mdkb_invocation` outcomes: Bash calls that actually ran mdkb.
    /// For `pre_tool_use`, `converted` vs `fired` is the redirect's hit rate.
    pub converted: usize,
    pub avg_ms: u64,
    pub p95_ms: u64,
    /// Runs cut short by the hook deadline (`outcome = "deadline"`).
    pub deadline_hits: usize,
    /// Sum of context bytes returned to the host in this log window.
    pub payload_bytes: u64,
    /// Bytes attributed to each contributing hook block.
    pub payload_blocks: std::collections::BTreeMap<String, u64>,
}

// ── collect_report ───────────────────────────────────────────────────────────

pub fn collect_report(ctx: &Context) -> Result<StatsReport> {
    let mdkb_dir = ctx.db_path.parent().expect("db_path has parent");
    let root = mdkb_dir.parent().expect("mdkb_dir has parent");
    let status = search::get_status(&ctx.conn)?;

    Ok(StatsReport {
        header: collect_header(ctx, root, &status),
        index: collect_index_health(ctx, &status)?,
        collections: collect_collections(ctx)?,
        memory: collect_memory(ctx)?,
        code: collect_code(mdkb_dir),
        sessions: collect_sessions(ctx)?,
        hooks: collect_hooks(mdkb_dir, root, collect_mining(ctx)),
        quarantine: crate::store::heal::quarantine_reports(mdkb_dir),
        recall: collect_recall(ctx, mdkb_dir),
        doctor: crate::domain::doctor::findings(&crate::cli::doctor::collect(
            root,
            Some(ctx),
            false,
        )),
    })
}

/// The recall section. A store that cannot answer (an old read-only copy
/// without the ledger) reports an empty section rather than failing `stats`.
fn collect_recall(ctx: &Context, mdkb_dir: &Path) -> RecallReport {
    use crate::store::recall_ledger::{band_counts, prompts_by_mode};
    match (band_counts(&ctx.conn), prompts_by_mode(&ctx.conn)) {
        (Ok(counts), Ok(prompts)) => {
            let mut report = build_recall_report(counts, prompts);
            report.docs = docs_admissions(&read_hook_events(mdkb_dir, hook_window_start()));
            report
        }
        (Err(error), _) | (_, Err(error)) => {
            tracing::debug!("recall ledger unavailable: {error}");
            RecallReport::default()
        }
    }
}

fn collect_header(ctx: &Context, root: &Path, status: &IndexStatus) -> HeaderInfo {
    let repo = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let db_size_bytes = ctx.db_path.metadata().map(|m| m.len()).unwrap_or(0);
    HeaderInfo {
        repo,
        version,
        db_size_bytes,
        last_updated: status.last_updated,
    }
}

fn collect_index_health(ctx: &Context, status: &IndexStatus) -> Result<IndexHealth> {
    let document_count = status.documents;
    let memory_count = memory::count_active_entries(&ctx.conn)?;

    let (free_pages, total_pages): (i64, i64) = ctx
        .conn
        .query_row(
            "SELECT freelist_count, page_count FROM pragma_freelist_count(), pragma_page_count()",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 1));
    let free_page_ratio = if total_pages == 0 {
        0.0
    } else {
        free_pages as f64 / total_pages as f64
    };

    Ok(IndexHealth {
        document_count,
        memory_count,
        free_page_ratio,
    })
}

fn collect_collections(ctx: &Context) -> Result<CollectionsSummary> {
    let coll_list = collections::list_collections(&ctx.conn)?;

    // Single GROUP BY roll-up instead of one query per collection (PERF).
    let mut counts: HashMap<String, i64> = HashMap::new();
    let mut stmt = ctx
        .conn
        .prepare("SELECT collection, COUNT(*) FROM documents GROUP BY collection")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (name, count) = row?;
        counts.insert(name, count);
    }

    let rows = coll_list
        .iter()
        .map(|c| CollectionRow {
            name: c.name.clone(),
            path: c.path.clone(),
            pattern: c.pattern.clone(),
            doc_count: counts.get(&c.name).copied().unwrap_or(0),
        })
        .collect();
    Ok(CollectionsSummary { collections: rows })
}

fn collect_memory(ctx: &Context) -> Result<MemorySummary> {
    let now = chrono::Utc::now().timestamp();
    let week = now + 7 * 86_400;
    let active_count = memory::count_active_entries(&ctx.conn)?;

    // Must match count_active_entries criteria: exclude expired, non-due reminders, and priors.
    let mut counts_by_type: HashMap<String, usize> = HashMap::new();
    let mut stmt = ctx.conn.prepare(
        "SELECT entry_type, COUNT(*) FROM memory_entries
         WHERE status = 'active'
           AND (expires_at IS NULL OR expires_at > ?1)
           AND NOT (entry_type = 'reminder' AND (due_at IS NULL OR due_at > ?1))
           AND entry_type != 'prior'
         GROUP BY entry_type",
    )?;
    let rows = stmt.query_map([now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (ty, count) = row?;
        counts_by_type.insert(ty, count as usize);
    }

    let reminders_due: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM memory_entries
         WHERE status = 'active' AND entry_type = 'reminder' AND due_at IS NOT NULL AND due_at <= ?1",
        [now],
        |r| r.get(0),
    ).unwrap_or(0);

    let reminders_upcoming_7d: i64 = ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_entries
         WHERE status = 'active' AND entry_type = 'reminder' AND due_at IS NOT NULL
           AND due_at > ?1 AND due_at <= ?2",
            [now, week],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let pending_embeddings = memory::count_pending_embeddings(&ctx.conn).unwrap_or(0);

    // Drift is read-only here on purpose: `stats` reports, `update` repairs.
    // A failure to measure must not fail the whole report — a store too broken
    // to walk its own projection still needs its other numbers.
    let drift = crate::core::memory_sync::projection_drift(ctx).unwrap_or_default();

    Ok(MemorySummary {
        active_count,
        counts_by_type,
        reminders_due: reminders_due as usize,
        reminders_upcoming_7d: reminders_upcoming_7d as usize,
        pending_embeddings,
        files_unreadable: drift.files_unreadable,
        entries_unprojected: drift.entries_unprojected,
    })
}

fn empty_code_summary() -> CodeSummary {
    CodeSummary {
        files: 0,
        symbols: 0,
        relations: 0,
        last_indexed: None,
        languages: vec![],
        symbols_by_kind: vec![],
        relations_by_kind: vec![],
        top_files: vec![],
    }
}

fn collect_code(mdkb_dir: &Path) -> CodeSummary {
    let code_path = mdkb_dir.join("code.sqlite");
    if !code_path.exists() {
        return empty_code_summary();
    }

    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &code_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return empty_code_summary();
    };

    let files = scalar_count(&conn, "SELECT COUNT(*) FROM code_files");
    let symbols = scalar_count(&conn, "SELECT COUNT(*) FROM code_symbols");
    let relations = scalar_count(&conn, "SELECT COUNT(*) FROM code_relationships");
    let last_indexed = crate::code::storage::schema::last_index_scan_at(&conn).unwrap_or(None);

    let languages = query_rows(
        &conn,
        "SELECT COALESCE(f.language, 'unknown'),
                COUNT(DISTINCT f.id),
                COUNT(s.id)
         FROM code_files f
         LEFT JOIN code_symbols s ON s.file_id = f.id
         GROUP BY f.language
         ORDER BY COUNT(s.id) DESC",
        |row| {
            Ok(LanguageRow {
                language: row.get(0)?,
                files: row.get::<_, i64>(1)? as usize,
                symbols: row.get::<_, i64>(2)? as usize,
            })
        },
    );

    let symbols_by_kind = query_rows(
        &conn,
        "SELECT COALESCE(kind, 'unknown'), COUNT(*) FROM code_symbols GROUP BY kind ORDER BY COUNT(*) DESC",
        |row| {
            Ok(KindCount {
                kind: row.get(0)?,
                count: row.get::<_, i64>(1)? as usize,
            })
        },
    );

    let relations_by_kind = query_rows(
        &conn,
        "SELECT COALESCE(kind, 'unknown'), COUNT(*) FROM code_relationships GROUP BY kind ORDER BY COUNT(*) DESC",
        |row| {
            Ok(KindCount {
                kind: row.get(0)?,
                count: row.get::<_, i64>(1)? as usize,
            })
        },
    );

    let top_files = query_rows(
        &conn,
        "SELECT f.rel_path, COUNT(s.id)
         FROM code_files f
         LEFT JOIN code_symbols s ON s.file_id = f.id
         GROUP BY f.id
         ORDER BY COUNT(s.id) DESC
         LIMIT 8",
        |row| {
            Ok(FileSymbolRow {
                path: row.get(0)?,
                symbols: row.get::<_, i64>(1)? as usize,
            })
        },
    );

    CodeSummary {
        files,
        symbols,
        relations,
        last_indexed,
        languages,
        symbols_by_kind,
        relations_by_kind,
        top_files,
    }
}

fn scalar_count(conn: &rusqlite::Connection, sql: &str) -> usize {
    conn.query_row(sql, [], |r| r.get::<_, i64>(0))
        .map(|n| n as usize)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, sql, "code.sqlite scalar query failed");
            0
        })
}

fn query_rows<T, F>(conn: &rusqlite::Connection, sql: &str, mut map: F) -> Vec<T>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut out = Vec::new();
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, sql, "code.sqlite prepare failed");
            return out;
        }
    };
    let rows = match stmt.query_map([], |r| map(r)) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(error = %e, sql, "code.sqlite query_map failed");
            return out;
        }
    };
    for row in rows {
        match row {
            Ok(v) => out.push(v),
            Err(e) => tracing::warn!(error = %e, sql, "code.sqlite row decode failed"),
        }
    }
    out
}

fn collect_sessions(ctx: &Context) -> Result<SessionsSummary> {
    let agg = stats::get_aggregate_stats(&ctx.conn)?;
    let tool_rows: Vec<ToolRow> = stats::get_aggregate_tool_usage(&ctx.conn)?
        .into_iter()
        .take(10)
        .map(|t| ToolRow {
            tool_name: t.tool_name,
            call_count: t.call_count,
        })
        .collect();

    Ok(SessionsSummary {
        total_sessions: agg.total_sessions,
        total_calls: agg.total_calls,
        top_tools: tool_rows,
    })
}

fn collect_hooks(mdkb_dir: &Path, repo_root: &Path, mut mining: MiningStatus) -> HooksSummary {
    let cutoff = hook_window_start();

    let slow_events_7d = count_slow_events(mdkb_dir, cutoff);
    let recent = read_hook_events(mdkb_dir, cutoff);
    let events = collect_hook_event_stats(&recent);
    mining.outcomes_7d = collect_mining_outcomes(&recent);
    let drift = crate::cli::setup::detect_hook_drift_for_repo(repo_root, None);

    HooksSummary {
        slow_events_7d,
        events,
        drift,
        mining,
    }
}

/// Start of the 7-day window the hook statistics cover.
fn hook_window_start() -> i64 {
    chrono::Utc::now().timestamp() - 7 * 86_400
}

/// Every parseable `hook-events.jsonl` line at or after `since_ts`.
fn read_hook_events(mdkb_dir: &Path, since_ts: i64) -> Vec<serde_json::Value> {
    let Ok(content) = std::fs::read_to_string(mdkb_dir.join("hook-events.jsonl")) else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|v| v.get("ts").and_then(|t| t.as_i64()).unwrap_or(0) >= since_ts)
        .collect()
}

/// Per-outcome counts for the `prior_mining` stream, most frequent first.
fn collect_mining_outcomes(events: &[serde_json::Value]) -> Vec<MiningOutcomeCount> {
    let mut counts: HashMap<String, (usize, Option<String>)> = HashMap::new();
    for v in events {
        if v.get("event").and_then(|e| e.as_str()) != Some(MINING_EVENT) {
            continue;
        }
        let outcome = v
            .get("outcome")
            .and_then(|o| o.as_str())
            .unwrap_or("?")
            .to_string();
        let entry = counts.entry(outcome).or_insert((0, None));
        entry.0 += 1;
        // Lines are appended in time order, so the last one wins: the reason an
        // operator needs is the one from the most recent run, not the first.
        if let Some(reason) = v.get("reason").and_then(|r| r.as_str()) {
            entry.1 = Some(reason.to_string());
        }
    }

    let mut out: Vec<MiningOutcomeCount> = counts
        .into_iter()
        .map(|(outcome, (count, last_reason))| MiningOutcomeCount {
            outcome,
            count,
            last_reason,
        })
        .collect();
    out.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.outcome.cmp(&b.outcome))
    });
    out
}

/// Behavioral-prior mining status: on only when the master switch AND a distiller
/// are both configured. The reason makes the common "silently off" states
/// (no distiller, kill-switch off) visible instead of leaving operators guessing.
fn collect_mining(ctx: &Context) -> MiningStatus {
    // Report the EFFECTIVE priors the daemon actually mines with: the global
    // daemon.toml layer merged under any per-repo override — the same merge
    // `RepoHandle::open` performs. Reading only the repo config would show
    // "disabled" even when daemon.toml turned mining on.
    let priors = crate::config::effective_priors(&ctx.config_path);

    let candidate_count = ctx
        .conn
        .query_row("SELECT COUNT(*) FROM prior_candidates", [], |r| r.get(0))
        .unwrap_or(0);

    let (enabled, reason) = if !priors.mining_enabled {
        (false, "mining_enabled = false".to_string())
    } else if priors.distiller_program.is_none() {
        (false, "no distiller_program configured".to_string())
    } else {
        (
            true,
            format!(
                "active (distiller: {})",
                priors.distiller_program.as_deref().unwrap_or("?")
            ),
        )
    };

    MiningStatus {
        enabled,
        reason,
        candidate_count,
        outcomes_7d: Vec::new(),
    }
}

/// Per entry: (fired, converted, elapsed_ms, payload_bytes, block_bytes).
type HookEventEntry = (
    bool,
    bool,
    u64,
    u64,
    serde_json::Map<String, serde_json::Value>,
);

fn collect_hook_event_stats(events: &[serde_json::Value]) -> Vec<HookEventStats> {
    let mut buckets: HashMap<String, Vec<HookEventEntry>> = HashMap::new();
    let mut deadline_hits: HashMap<String, usize> = HashMap::new();

    for v in events {
        let event = v
            .get("event")
            .and_then(|e| e.as_str())
            .unwrap_or("?")
            .to_string();
        // Mining has its own section. Its outcomes are gated/distilled/rejected/
        // failed, none of which is "fired", so this table would report it as a
        // hook that runs and never does anything.
        if event == MINING_EVENT {
            continue;
        }
        let outcome = v
            .get("outcome")
            .and_then(|o| o.as_str())
            .unwrap_or("skipped");
        if outcome == "deadline" {
            *deadline_hits.entry(event.clone()).or_default() += 1;
        }
        let elapsed = v.get("elapsed_ms").and_then(|e| e.as_u64()).unwrap_or(0);
        let payload_bytes = v.get("payload_bytes").and_then(|b| b.as_u64()).unwrap_or(0);
        buckets.entry(event).or_default().push((
            outcome == "fired" || payload_bytes > 0,
            outcome == "mdkb_invocation",
            elapsed,
            payload_bytes,
            v.get("payload_blocks")
                .and_then(|b| b.as_object())
                .cloned()
                .unwrap_or_default(),
        ));
    }

    let mut stats: Vec<HookEventStats> = buckets
        .into_iter()
        .map(|(event, entries)| {
            let invocations = entries.len();
            let fired = entries.iter().filter(|(f, _, _, _, _)| *f).count();
            let converted = entries.iter().filter(|(_, c, _, _, _)| *c).count();
            let mut latencies: Vec<u64> = entries.iter().map(|(_, _, ms, _, _)| *ms).collect();
            latencies.sort_unstable();
            let mut payload_bytes = 0u64;
            let mut payload_blocks = std::collections::BTreeMap::new();
            for (_, _, _, bytes, blocks) in &entries {
                payload_bytes = payload_bytes.saturating_add(*bytes);
                for (name, count) in blocks {
                    if let Some(count) = count.as_u64() {
                        let total: &mut u64 = payload_blocks.entry(name.clone()).or_insert(0);
                        *total = total.saturating_add(count);
                    }
                }
            }
            let avg_ms = if invocations > 0 {
                latencies.iter().sum::<u64>() / invocations as u64
            } else {
                0
            };
            let p95_ms = if invocations > 0 {
                latencies[(invocations as f64 * 0.95).ceil() as usize - 1]
            } else {
                0
            };
            let deadline_hits = deadline_hits.get(&event).copied().unwrap_or(0);
            HookEventStats {
                event,
                invocations,
                fired,
                converted,
                avg_ms,
                p95_ms,
                deadline_hits,
                payload_bytes,
                payload_blocks,
            }
        })
        .collect();

    stats.sort_by_key(|stat| std::cmp::Reverse(stat.invocations));
    stats
}

fn count_slow_events(mdkb_dir: &Path, since_ts: i64) -> usize {
    let path = mdkb_dir.join("hook-slow.jsonl");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return 0;
    };
    content
        .lines()
        .filter(|line| {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            value
                .get("ts")
                .and_then(|v| v.as_i64())
                .is_some_and(|ts| ts >= since_ts)
        })
        .count()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::handlers::handle_init;
    use crate::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
    use tempfile::TempDir;

    struct Env {
        _dir: TempDir,
        ctx: Context,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path();
            handle_init(root).expect("init");
            let ctx = Context::open(root).expect("open");
            Self { _dir: dir, ctx }
        }

        fn add_memory(&self, id: &str, entry_type: EntryType) {
            let now = chrono::Utc::now().timestamp();
            add_entry(
                &self.ctx.conn,
                &MemoryEntry {
                    triggers: Vec::new(),
                    id: id.to_string(),
                    title: id.to_string(),
                    content: "test".to_string(),
                    entry_type,
                    tags: vec![],
                    status: EntryStatus::Active,
                    created_at: now,
                    updated_at: now,
                    superseded_by: None,
                    access_count: 0,
                    last_accessed: None,
                    source_path: None,
                    confirmations: 0,
                    corrections: 0,
                    last_confirmed_at: None,
                    last_refuted_at: None,
                    source_type: SourceType::UserStatement,
                    expires_at: None,
                    due_at: None,
                },
            )
            .expect("add_entry");
        }
    }

    #[test]
    fn collect_report_on_empty_db() {
        let env = Env::new();
        let report = collect_report(&env.ctx).expect("collect");
        assert_eq!(report.index.document_count, 0);
        assert_eq!(report.index.memory_count, 0);
        assert!(report.collections.collections.is_empty());
        assert_eq!(report.memory.active_count, 0);
        assert_eq!(report.sessions.total_sessions, 0);
    }

    #[test]
    fn memory_counts_by_type_correct() {
        let env = Env::new();
        env.add_memory("t1", EntryType::Topic);
        env.add_memory("t2", EntryType::Topic);
        env.add_memory("p1", EntryType::Problem);
        env.add_memory("d1", EntryType::Decision);

        let report = collect_report(&env.ctx).expect("collect");
        assert_eq!(
            report
                .memory
                .counts_by_type
                .get("topic")
                .copied()
                .unwrap_or(0),
            2
        );
        assert_eq!(
            report
                .memory
                .counts_by_type
                .get("problem")
                .copied()
                .unwrap_or(0),
            1
        );
        assert_eq!(
            report
                .memory
                .counts_by_type
                .get("decision")
                .copied()
                .unwrap_or(0),
            1
        );
        assert_eq!(report.memory.active_count, 4);
    }

    #[test]
    fn archived_entries_excluded_from_stats() {
        let env = Env::new();
        let now = chrono::Utc::now().timestamp();

        // Active topic — should be counted
        env.add_memory("active-t", EntryType::Topic);

        // Archived topic — must NOT appear in counts_by_type or active_count
        let mut archived = make_entry("archived-t", EntryType::Topic);
        archived.status = EntryStatus::Archived;
        add_entry(&env.ctx.conn, &archived).expect("add archived");

        // Superseded topic — must NOT appear either
        let mut superseded = make_entry("superseded-t", EntryType::Topic);
        superseded.status = EntryStatus::Superseded;
        add_entry(&env.ctx.conn, &superseded).expect("add superseded");

        // Archived reminder with due_at in the past — must NOT count as due
        let mut archived_due = make_entry("archived-r", EntryType::Reminder);
        archived_due.status = EntryStatus::Archived;
        archived_due.due_at = Some(now - 60);
        add_entry(&env.ctx.conn, &archived_due).expect("add archived reminder");

        // Archived reminder upcoming — must NOT count in upcoming_7d
        let mut archived_upcoming = make_entry("archived-upcoming-r", EntryType::Reminder);
        archived_upcoming.status = EntryStatus::Archived;
        archived_upcoming.due_at = Some(now + 2 * 86_400);
        add_entry(&env.ctx.conn, &archived_upcoming).expect("add archived upcoming");

        let report = collect_report(&env.ctx).expect("collect");
        // Only the one active topic
        assert_eq!(
            report
                .memory
                .counts_by_type
                .get("topic")
                .copied()
                .unwrap_or(0),
            1
        );
        assert_eq!(report.memory.active_count, 1);
        // No reminders are active
        assert_eq!(report.memory.reminders_due, 0);
        assert_eq!(report.memory.reminders_upcoming_7d, 0);
    }

    #[test]
    fn counts_by_type_excludes_priors_and_expired() {
        let env = Env::new();
        let now = chrono::Utc::now().timestamp();

        // Active topic — counted
        env.add_memory("t1", EntryType::Topic);

        // Active prior — excluded (entry_type != 'prior')
        let mut prior = make_entry("p1", EntryType::Prior);
        prior.expires_at = Some(now + 86_400);
        add_entry(&env.ctx.conn, &prior).expect("add prior");

        // Expired topic — excluded (expires_at < now)
        let mut expired = make_entry("t-expired", EntryType::Topic);
        expired.expires_at = Some(now - 60);
        add_entry(&env.ctx.conn, &expired).expect("add expired");

        let report = collect_report(&env.ctx).expect("collect");
        let type_sum: usize = report.memory.counts_by_type.values().sum();
        assert_eq!(
            type_sum, report.memory.active_count,
            "sum of counts_by_type must equal active_count"
        );
        assert_eq!(
            report
                .memory
                .counts_by_type
                .get("prior")
                .copied()
                .unwrap_or(0),
            0,
            "priors excluded from counts_by_type"
        );
    }

    #[test]
    fn reminder_due_counted() {
        let env = Env::new();
        let now = chrono::Utc::now().timestamp();

        let mut due = make_entry("due-r", EntryType::Reminder);
        due.due_at = Some(now - 60); // past
        add_entry(&env.ctx.conn, &due).expect("add");

        let mut upcoming = make_entry("upcoming-r", EntryType::Reminder);
        upcoming.due_at = Some(now + 2 * 86_400); // 2 days from now
        add_entry(&env.ctx.conn, &upcoming).expect("add");

        let mut far = make_entry("far-r", EntryType::Reminder);
        far.due_at = Some(now + 10 * 86_400); // 10 days
        add_entry(&env.ctx.conn, &far).expect("add");

        let report = collect_report(&env.ctx).expect("collect");
        assert_eq!(report.memory.reminders_due, 1);
        assert_eq!(report.memory.reminders_upcoming_7d, 1);
    }

    #[test]
    fn header_version_non_empty() {
        let env = Env::new();
        let report = collect_report(&env.ctx).expect("collect");
        assert!(!report.header.version.is_empty());
    }

    #[test]
    fn hooks_summary_counts_slow_events() {
        let env = Env::new();
        let mdkb_dir = env.ctx.db_path.parent().unwrap();
        let now = chrono::Utc::now().timestamp();

        let line_old = format!(
            "{{\"event\":\"session-start\",\"elapsed_ms\":400,\"ts\":{}}}\n",
            now - 8 * 86_400
        );
        let line_new = format!(
            "{{\"event\":\"session-start\",\"elapsed_ms\":400,\"ts\":{}}}\n",
            now - 3600
        );
        std::fs::write(mdkb_dir.join("hook-slow.jsonl"), line_old + &line_new).unwrap();

        let report = collect_report(&env.ctx).expect("collect");
        assert_eq!(report.hooks.slow_events_7d, 1, "only recent event counted");
    }

    #[test]
    fn count_slow_events_handles_malformed_and_missing_ts() {
        let dir = tempfile::tempdir().unwrap();
        let cutoff = 100;
        let content = [
            "not json",            // parse error → skipped
            "{\"event\":\"x\"}",   // missing ts → skipped
            "{\"ts\":50}",         // below cutoff → skipped
            "{\"ts\":100}",        // at cutoff → counted
            "{\"ts\":200}",        // above cutoff → counted
            "{\"ts\":\"string\"}", // non-integer ts → skipped
        ]
        .join("\n");
        std::fs::write(dir.path().join("hook-slow.jsonl"), content).unwrap();

        assert_eq!(super::count_slow_events(dir.path(), cutoff), 2);
    }

    #[test]
    fn render_memory_empty_shows_placeholder() {
        use crate::cli::stats_render_report::render;
        let mut r = make_empty_report();
        r.memory.counts_by_type.clear();
        let out = render(&r, false);
        assert!(
            out.contains("(no entries)"),
            "empty memory must render placeholder"
        );
    }

    #[test]
    fn report_is_json_serializable() {
        let env = Env::new();
        let report = collect_report(&env.ctx).expect("collect");
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.starts_with('{'));
        // Round-trip via Value to confirm schema stability.
        let _v: serde_json::Value = serde_json::from_str(&json).expect("parse json");
    }

    fn counts(
        band: &str,
        holdout: bool,
        positive: u32,
        negative: u32,
        missed: u32,
    ) -> crate::store::recall_ledger::BandCounts {
        crate::store::recall_ledger::BandCounts {
            band: band.into(),
            entry_type: "decision".into(),
            holdout,
            offered: positive + negative + missed + 5,
            injected: positive + negative,
            labelled: positive + negative + missed,
            positive,
            negative,
            missed,
        }
    }

    #[test]
    fn recall_precision_is_reported_per_band_once_enough_is_labelled() {
        let report = build_recall_report(
            vec![
                counts("0.45-0.50", false, 3, 9, 4),
                counts("0.65+", false, 18, 2, 0),
            ],
            Default::default(),
        );
        assert_eq!(report.labelled, 36);
        assert!(!report.insufficient_data);
        let precision: Vec<(&str, Option<f64>)> = report
            .bands
            .iter()
            .map(|b| (b.counts.band.as_str(), b.precision))
            .collect();
        assert_eq!(
            precision,
            vec![("0.45-0.50", Some(0.25)), ("0.65+", Some(0.9))]
        );
    }

    #[test]
    fn too_few_labels_give_no_ratio_at_all() {
        let report =
            build_recall_report(vec![counts("0.65+", false, 20, 5, 0)], Default::default());
        assert_eq!(report.labelled, 25);
        assert!(report.insufficient_data);
        assert!(report.bands.iter().all(|b| b.precision.is_none()));
    }

    /// Docs are admitted by their own gate, so the Recall section counts them
    /// on their own: a prompt that injected memory only is a prompt without docs.
    #[test]
    fn docs_admissions_count_prompts_whose_block_carried_docs() {
        let events: Vec<serde_json::Value> = [
            r#"{"event":"user_prompt_submit","outcome":"fired","payload_blocks":{"recall_memory":90,"recall_docs":40}}"#,
            r#"{"event":"user_prompt_submit","outcome":"fired","payload_blocks":{"recall_memory":90}}"#,
            r#"{"event":"user_prompt_submit","outcome":"skipped"}"#,
            r#"{"event":"pre_tool_use","outcome":"fired","payload_blocks":{"recall_docs":7}}"#,
        ]
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
        let docs = docs_admissions(&events);
        assert_eq!((docs.prompts, docs.with_docs), (3, 1));
    }

    /// Holdouts were injected on purpose; pooling them would report the
    /// precision of the experiment, not of the floor.
    #[test]
    fn holdout_candidates_stay_out_of_the_headline() {
        let report = build_recall_report(
            vec![
                counts("0.65+", false, 30, 10, 0),
                counts("0.40-0.45", true, 0, 9, 0),
            ],
            Default::default(),
        );
        assert_eq!(report.labelled, 40, "holdout labels are not counted");
        assert_eq!(report.bands.len(), 1);
        assert_eq!(report.holdout.len(), 1);
        assert_eq!(report.bands[0].precision, Some(0.75));
    }

    fn make_empty_report() -> StatsReport {
        StatsReport {
            header: HeaderInfo {
                repo: "t".into(),
                version: "0.0.0".into(),
                db_size_bytes: 0,
                last_updated: None,
            },
            index: IndexHealth {
                document_count: 0,
                memory_count: 0,
                free_page_ratio: 0.0,
            },
            collections: CollectionsSummary {
                collections: vec![],
            },
            memory: MemorySummary {
                active_count: 0,
                counts_by_type: HashMap::new(),
                reminders_due: 0,
                reminders_upcoming_7d: 0,
                pending_embeddings: 0,
                files_unreadable: 0,
                entries_unprojected: 0,
            },
            code: empty_code_summary(),
            sessions: SessionsSummary {
                total_sessions: 0,
                total_calls: 0,
                top_tools: vec![],
            },
            hooks: HooksSummary {
                slow_events_7d: 0,
                events: vec![],
                drift: crate::cli::setup::HookDrift::default(),
                mining: MiningStatus {
                    enabled: false,
                    reason: "mining_enabled = false".to_string(),
                    candidate_count: 0,
                    outcomes_7d: Vec::new(),
                },
            },
            quarantine: vec![],
            recall: RecallReport::default(),
            doctor: Vec::new(),
        }
    }

    /// `mdkb stats` answers "is mining working?", not just "is it switched on?".
    ///
    /// `enabled` plus `candidates` cannot tell a distiller that never runs from
    /// one that runs and is rejected every time — for six weeks both looked
    /// identical, and the answer was "every call returns HTTP 400". The
    /// per-outcome counts are the difference.
    #[test]
    fn mining_status_reports_the_outcomes_of_the_last_seven_days() {
        let env = Env::new();
        let mdkb_dir = env.ctx.db_path.parent().unwrap();
        let now = chrono::Utc::now().timestamp();
        let event = |outcome: &str, ts: i64| {
            format!(
                r#"{{"event":"prior_mining","outcome":"{outcome}","elapsed_ms":900,"ts":{ts}}}"#
            )
        };
        let lines = [
            event("gated", now),
            event("gated", now),
            event("failed", now),
            event("distilled", now),
            event("rejected", now),
            // Outside the 7-day window: counted nowhere.
            event("distilled", now - 8 * 86_400),
            // A different event stream must not leak into the mining counts.
            format!(r#"{{"event":"PreToolUse","outcome":"fired","elapsed_ms":5,"ts":{now}}}"#),
        ];
        std::fs::write(mdkb_dir.join("hook-events.jsonl"), lines.join("\n") + "\n").unwrap();

        let report = collect_report(&env.ctx).expect("collect");
        let counts = &report.hooks.mining.outcomes_7d;
        let of = |name: &str| counts.iter().find(|o| o.outcome == name).map(|o| o.count);

        assert_eq!(of("gated"), Some(2), "counts: {counts:?}");
        assert_eq!(of("failed"), Some(1), "counts: {counts:?}");
        assert_eq!(
            of("distilled"),
            Some(1),
            "the 8-day-old run is out of window"
        );
        assert_eq!(of("rejected"), Some(1), "counts: {counts:?}");
        assert_eq!(counts.len(), 4, "no foreign event leaked in: {counts:?}");
    }

    #[test]
    fn hooks_summary_counts_deadline_hits_per_event() {
        let env = Env::new();
        let mdkb_dir = env.ctx.db_path.parent().unwrap();
        let now = chrono::Utc::now().timestamp();
        let row = |event: &str, outcome: &str| {
            format!(r#"{{"event":"{event}","outcome":"{outcome}","elapsed_ms":1500,"ts":{now}}}"#)
        };
        let lines = [
            row("user_prompt_submit", "deadline"),
            row("user_prompt_submit", "deadline"),
            row("user_prompt_submit", "fired"),
            row("pre_tool_use", "fired"),
        ];
        std::fs::write(mdkb_dir.join("hook-events.jsonl"), lines.join("\n") + "\n").unwrap();

        let report = collect_report(&env.ctx).expect("collect");
        let hits = |name: &str| {
            report
                .hooks
                .events
                .iter()
                .find(|e| e.event == name)
                .map(|e| e.deadline_hits)
        };
        assert_eq!(hits("user_prompt_submit"), Some(2));
        assert_eq!(hits("pre_tool_use"), Some(0));
    }

    #[test]
    fn hooks_summary_aggregates_event_stats() {
        let env = Env::new();
        let mdkb_dir = env.ctx.db_path.parent().unwrap();
        let now = chrono::Utc::now().timestamp();
        let lines = [
            format!(
                r#"{{"event":"PreToolUse","outcome":"fired","elapsed_ms":5,"payload_bytes":12,"payload_blocks":{{"search_redirect":12}},"ts":{}}}"#,
                now
            ),
            format!(
                r#"{{"event":"PreToolUse","outcome":"skipped","elapsed_ms":2,"ts":{}}}"#,
                now
            ),
            format!(
                r#"{{"event":"PreToolUse","outcome":"fired","elapsed_ms":10,"payload_bytes":20,"payload_blocks":{{"search_redirect":8,"prior":12}},"ts":{}}}"#,
                now
            ),
            // Conversion signal: Claude actually ran mdkb after a redirect.
            format!(
                r#"{{"event":"PreToolUse","outcome":"mdkb_invocation","elapsed_ms":1,"ts":{}}}"#,
                now
            ),
            format!(
                r#"{{"event":"PostToolUse","outcome":"fired","elapsed_ms":3,"ts":{}}}"#,
                now
            ),
            // Old event — should be excluded (8 days ago)
            format!(
                r#"{{"event":"PreToolUse","outcome":"fired","elapsed_ms":1,"ts":{}}}"#,
                now - 8 * 86_400
            ),
        ];
        std::fs::write(mdkb_dir.join("hook-events.jsonl"), lines.join("\n") + "\n").unwrap();

        let report = collect_report(&env.ctx).expect("collect");
        assert_eq!(report.hooks.events.len(), 2);

        let pre = report
            .hooks
            .events
            .iter()
            .find(|e| e.event == "PreToolUse")
            .unwrap();
        assert_eq!(pre.invocations, 4); // 2 fired + 1 skipped + 1 mdkb_invocation
        assert_eq!(pre.fired, 2);
        assert_eq!(pre.converted, 1); // the mdkb_invocation
        assert_eq!(pre.avg_ms, 4); // (5+2+10+1)/4 = 4
        assert_eq!(pre.p95_ms, 10); // sorted: [1,2,5,10], idx ceil(4*0.95)-1 = 3 → 10
        let pre_json = serde_json::to_value(pre).unwrap();
        assert_eq!(pre.deadline_hits, 0);
        assert_eq!(pre_json["payload_bytes"], 32);
        assert_eq!(pre_json["payload_blocks"]["search_redirect"], 20);
        assert_eq!(pre_json["payload_blocks"]["prior"], 12);

        let post = report
            .hooks
            .events
            .iter()
            .find(|e| e.event == "PostToolUse")
            .unwrap();
        assert_eq!(post.invocations, 1);
        assert_eq!(post.fired, 1);
    }

    /// Stats is a best-effort read. It must never join the live-lock protocol:
    /// that protocol protects mutation and recovery, while waiting here would
    /// turn an informational command into a blocker behind quarantine. A plain
    /// read-only SQLite snapshot may succeed while the exclusive advisory lock
    /// is held; if SQLite itself cannot provide one, an empty summary is safe.
    #[test]
    fn collect_code_does_not_wait_on_live_lock_or_mutate_the_index() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("code.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        crate::code::storage::schema::init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO code_files (path, rel_path, hash, language, indexed_at)
             VALUES ('/repo/src/lib.rs', 'src/lib.rs', 'hash', 'rust', 1)",
            [],
        )
        .unwrap();
        // `init_schema` deliberately enables WAL for writers. Collapse that
        // fixture state before testing the reader, so any sidecar observed
        // afterwards can only have been created by `collect_code`.
        conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        drop(conn);

        let before_db = std::fs::read(&db_path).unwrap();
        let wal_path = db_path.with_extension("sqlite-wal");
        let shm_path = db_path.with_extension("sqlite-shm");
        let mutation_path = crate::store::mutation_lock::lock_path(&db_path);
        assert!(!wal_path.exists() && !shm_path.exists() && !mutation_path.exists());

        let blocker = crate::store::mutation_lock::try_acquire_live_exclusive(&db_path)
            .unwrap()
            .expect("nothing else holds the live lock yet");
        let before_entries = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();

        let (tx, rx) = mpsc::channel();
        let root = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            let summary = collect_code(&root);
            tx.send(summary).unwrap();
        });

        let summary = rx.recv_timeout(Duration::from_secs(2));
        drop(blocker);
        worker.join().unwrap();
        let summary =
            summary.expect("collect_code must return while the exclusive live lock is still held");

        assert_eq!(summary.files, 1, "read-only snapshot remains available");
        assert_eq!(
            std::fs::read(&db_path).unwrap(),
            before_db,
            "collect_code must not repair or otherwise modify code.sqlite"
        );
        assert!(!wal_path.exists() && !shm_path.exists() && !mutation_path.exists());
        let after_entries = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            after_entries, before_entries,
            "collect_code created a sidecar"
        );
    }

    #[test]
    fn repair_removes_corrupt_rows_on_stats_collect() {
        use crate::code::storage::repair::run_repairs;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("code.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();

        // Legacy schema without NOT NULL constraints
        conn.execute_batch(
            "CREATE TABLE code_files (
                id INTEGER PRIMARY KEY, path TEXT, rel_path TEXT, hash TEXT, indexed_at INTEGER
             );
             CREATE TABLE code_symbols (
                id INTEGER PRIMARY KEY, name TEXT, kind TEXT, file_id INTEGER, file_path TEXT,
                line_start INTEGER, line_end INTEGER
             );
             CREATE TABLE code_relationships (
                id INTEGER PRIMARY KEY, from_symbol_id INTEGER, from_name TEXT,
                to_name TEXT, kind TEXT, file_id INTEGER
             );
             CREATE VIRTUAL TABLE code_symbols_fts USING fts5(
                name, doc_comment, signature, content=code_symbols, content_rowid=id,
                tokenize='trigram case_sensitive 0'
             );",
        )
        .unwrap();

        conn.execute_batch(
            "INSERT INTO code_files VALUES (1, 'a.rs', 'a.rs', 'h1', 1);
             INSERT INTO code_symbols VALUES (1, 'good', 'fn', 1, 'a.rs', 1, 2);
             INSERT INTO code_symbols VALUES (2, 'bad_null', NULL, 1, 'a.rs', 3, 4);
             INSERT INTO code_symbols VALUES (3, 'orphan', 'fn', 999, 'gone.rs', 1, 2);
             INSERT INTO code_relationships VALUES (1, 1, 'a', 'b', 'calls', 1);
             INSERT INTO code_relationships VALUES (2, 1, 'x', 'y', NULL, 1);
             INSERT INTO code_relationships VALUES (3, 1, 'c', 'd', 'calls', 999);
             INSERT INTO code_relationships VALUES (4, 999, 'e', 'f', 'calls', 1);",
        )
        .unwrap();

        let report = run_repairs(&conn);

        assert_eq!(report.null_kind_symbols, 1);
        assert_eq!(report.null_kind_relationships, 1);
        assert_eq!(report.orphaned_symbols, 1);
        assert_eq!(report.orphaned_relationships_file, 1);
        assert_eq!(report.orphaned_relationships_symbol, 1);

        let sym_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_symbols", [], |r| r.get(0))
            .unwrap();
        let rel_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM code_relationships", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sym_count, 1);
        assert_eq!(rel_count, 1);
    }

    fn make_entry(id: &str, entry_type: EntryType) -> MemoryEntry {
        let now = chrono::Utc::now().timestamp();
        MemoryEntry {
            triggers: Vec::new(),
            id: id.to_string(),
            title: id.to_string(),
            content: "test".to_string(),
            entry_type,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        }
    }
}
