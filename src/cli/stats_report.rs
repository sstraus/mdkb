//! Data aggregator for `mdkb stats`.
//!
//! `collect_report` gathers all data into a `StatsReport` without rendering;
//! rendering lives in `stats_render`.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::cli::handlers::Context;
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
    /// until the operator removes the `*.corrupt-*` files — a persistent, loud
    /// data-loss warning.
    pub quarantine: Vec<crate::store::heal::QuarantineReport>,
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
    })
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

    Ok(MemorySummary {
        active_count,
        counts_by_type,
        reminders_due: reminders_due as usize,
        reminders_upcoming_7d: reminders_upcoming_7d as usize,
        pending_embeddings,
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

    // `run_repairs` DELETEs, so this connection must announce itself: a
    // quarantine renaming the path under it recycles the path onto a fresh
    // database and these frames would land in the replacement's WAL. The lock
    // is shared, so this only ever waits for the brief exclusive window a
    // quarantine holds while renaming; `ok()` covers the sidecar being
    // unopenable, and then we skip the repairs — a pure reader injects no
    // frames.
    let live = crate::store::mutation_lock::acquire_live_shared(&code_path).ok();
    let conn = match rusqlite::Connection::open(&code_path) {
        Ok(c) => {
            if live.is_some() {
                crate::code::storage::repair::run_repairs(&c);
            }
            c
        }
        Err(_) => {
            match rusqlite::Connection::open_with_flags(
                &code_path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                Ok(c) => c,
                Err(_) => return empty_code_summary(),
            }
        }
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
    stats::init_stats_schema(&ctx.conn)?;
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

fn collect_hooks(mdkb_dir: &Path, repo_root: &Path, mining: MiningStatus) -> HooksSummary {
    let cutoff = chrono::Utc::now().timestamp() - 7 * 86_400;

    let slow_events_7d = count_slow_events(mdkb_dir, cutoff);
    let events = collect_hook_event_stats(mdkb_dir, cutoff);
    let drift = crate::cli::setup::detect_hook_drift_for_repo(repo_root, None);

    HooksSummary {
        slow_events_7d,
        events,
        drift,
        mining,
    }
}

/// Behavioral-prior mining status: on only when the master switch AND a distiller
/// are both configured. The reason makes the common "silently off" states
/// (no distiller, kill-switch off) visible instead of leaving operators guessing.
fn collect_mining(ctx: &Context) -> MiningStatus {
    // Report the EFFECTIVE priors the daemon actually mines with: the global
    // daemon.toml layer merged under any per-repo override — the same merge
    // `RepoHandle::open` performs. Reading only the repo config would show
    // "disabled" even when daemon.toml turned mining on.
    let global = match crate::daemon::config::DaemonConfig::load_or_default(
        &crate::daemon::config::DaemonConfig::config_path(),
    ) {
        Ok(c) => c.priors,
        Err(e) => {
            // A corrupt daemon.toml would otherwise silently read as "mining
            // disabled" — surface the real cause instead of a misleading status
            // (FAIL-1).
            tracing::warn!("stats: daemon.toml failed to load, priors defaulted: {e}");
            toml::Table::new()
        }
    };
    let repo = crate::config::raw_priors_layer(&ctx.config_path);
    let priors = crate::config::merge_priors(&global, repo.as_ref());

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
    }
}

fn collect_hook_event_stats(mdkb_dir: &Path, since_ts: i64) -> Vec<HookEventStats> {
    let path = mdkb_dir.join("hook-events.jsonl");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    // Per entry: (fired, converted, elapsed_ms).
    let mut buckets: HashMap<String, Vec<(bool, bool, u64)>> = HashMap::new();

    for line in content.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = v.get("ts").and_then(|t| t.as_i64()).unwrap_or(0);
        if ts < since_ts {
            continue;
        }
        let event = v
            .get("event")
            .and_then(|e| e.as_str())
            .unwrap_or("?")
            .to_string();
        let outcome = v
            .get("outcome")
            .and_then(|o| o.as_str())
            .unwrap_or("skipped");
        let elapsed = v.get("elapsed_ms").and_then(|e| e.as_u64()).unwrap_or(0);
        buckets.entry(event).or_default().push((
            outcome == "fired",
            outcome == "mdkb_invocation",
            elapsed,
        ));
    }

    let mut stats: Vec<HookEventStats> = buckets
        .into_iter()
        .map(|(event, entries)| {
            let invocations = entries.len();
            let fired = entries.iter().filter(|(f, _, _)| *f).count();
            let converted = entries.iter().filter(|(_, c, _)| *c).count();
            let mut latencies: Vec<u64> = entries.iter().map(|(_, _, ms)| *ms).collect();
            latencies.sort_unstable();
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
            HookEventStats {
                event,
                invocations,
                fired,
                converted,
                avg_ms,
                p95_ms,
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
                    last_confirmed_at: None,
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
                },
            },
            quarantine: vec![],
        }
    }

    #[test]
    fn hooks_summary_aggregates_event_stats() {
        let env = Env::new();
        let mdkb_dir = env.ctx.db_path.parent().unwrap();
        let now = chrono::Utc::now().timestamp();
        let lines = [
            format!(
                r#"{{"event":"PreToolUse","outcome":"fired","elapsed_ms":5,"ts":{}}}"#,
                now
            ),
            format!(
                r#"{{"event":"PreToolUse","outcome":"skipped","elapsed_ms":2,"ts":{}}}"#,
                now
            ),
            format!(
                r#"{{"event":"PreToolUse","outcome":"fired","elapsed_ms":10,"ts":{}}}"#,
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

        let post = report
            .hooks
            .events
            .iter()
            .find(|e| e.event == "PostToolUse")
            .unwrap();
        assert_eq!(post.invocations, 1);
        assert_eq!(post.fired, 1);
    }

    /// `collect_code` writes (`run_repairs` DELETEs), so it must hold the live
    /// lock for the life of its connection. Renaming `code.sqlite` under an open
    /// connection is what seeded the repeated corruptions: SQLite binds the
    /// connection to the inode but derives `-wal`/`-shm` from the path, so a
    /// survivor lands its frames in the replacement's WAL. Holding the lock
    /// exclusively must therefore make `collect_code` wait.
    #[test]
    fn collect_code_waits_for_the_live_lock_before_opening() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("code.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        crate::code::storage::schema::init_schema(&conn).unwrap();
        drop(conn);

        let blocker = crate::store::mutation_lock::try_acquire_live_exclusive(&db_path)
            .unwrap()
            .expect("nothing else holds the live lock yet");

        let (tx, rx) = mpsc::channel();
        let root = dir.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            let summary = collect_code(&root);
            tx.send(summary).unwrap();
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "collect_code opened code.sqlite without announcing itself on the live lock"
        );

        drop(blocker);
        let summary = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("collect_code must proceed once the live lock is free");
        worker.join().unwrap();

        assert_eq!(summary.files, 0, "empty index counted correctly");
        assert!(
            crate::store::mutation_lock::try_acquire_live_exclusive(&db_path)
                .unwrap()
                .is_some(),
            "collect_code leaked its live guard — quarantine would be vetoed forever"
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
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        }
    }
}
