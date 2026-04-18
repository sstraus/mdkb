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
}

#[derive(Debug, Serialize)]
pub struct CodeSummary {
    /// Number of symbols per language. Empty when code.sqlite absent.
    pub symbols_by_language: HashMap<String, usize>,
    /// Top files by estimated token count (up to 10). Empty when absent.
    pub top_files_by_tokens: Vec<FileTokenRow>,
}

#[derive(Debug, Serialize)]
pub struct FileTokenRow {
    pub path: String,
    pub token_count: i64,
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
    /// Lines pending in reindex-queue.jsonl.
    pub reindex_queue_pending: usize,
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
        hooks: collect_hooks(mdkb_dir),
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
    HeaderInfo { repo, version, db_size_bytes, last_updated: status.last_updated }
}

fn collect_index_health(ctx: &Context, status: &IndexStatus) -> Result<IndexHealth> {
    let document_count = status.documents;
    let memory_count = memory::count_active_entries(&ctx.conn)?;

    let (free_pages, total_pages): (i64, i64) = ctx.conn.query_row(
        "SELECT freelist_count, page_count FROM pragma_freelist_count(), pragma_page_count()",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).unwrap_or((0, 1));
    let free_page_ratio = if total_pages == 0 {
        0.0
    } else {
        free_pages as f64 / total_pages as f64
    };

    Ok(IndexHealth { document_count, memory_count, free_page_ratio })
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

    // SQL GROUP BY is O(N) server-side; no need to hydrate every row (PERF).
    let mut counts_by_type: HashMap<String, usize> = HashMap::new();
    let mut stmt = ctx
        .conn
        .prepare("SELECT entry_type, COUNT(*) FROM memory_entries GROUP BY entry_type")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
    for row in rows {
        let (ty, count) = row?;
        counts_by_type.insert(ty, count as usize);
    }

    let reminders_due: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM memory_entries
         WHERE entry_type = 'reminder' AND due_at IS NOT NULL AND due_at <= ?1",
        [now],
        |r| r.get(0),
    ).unwrap_or(0);

    let reminders_upcoming_7d: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM memory_entries
         WHERE entry_type = 'reminder' AND due_at IS NOT NULL
           AND due_at > ?1 AND due_at <= ?2",
        [now, week],
        |r| r.get(0),
    ).unwrap_or(0);

    Ok(MemorySummary {
        active_count,
        counts_by_type,
        reminders_due: reminders_due as usize,
        reminders_upcoming_7d: reminders_upcoming_7d as usize,
    })
}

fn collect_code(mdkb_dir: &Path) -> CodeSummary {
    let code_path = mdkb_dir.join("code.sqlite");
    if !code_path.exists() {
        return CodeSummary {
            symbols_by_language: HashMap::new(),
            top_files_by_tokens: vec![],
        };
    }

    let Ok(conn) = rusqlite::Connection::open_with_flags(
        &code_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return CodeSummary { symbols_by_language: HashMap::new(), top_files_by_tokens: vec![] };
    };

    let mut symbols_by_language: HashMap<String, usize> = HashMap::new();
    match conn.prepare(
        "SELECT language, COUNT(*) FROM symbols GROUP BY language ORDER BY COUNT(*) DESC",
    ) {
        Ok(mut stmt) => match stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, usize>(1)?)))
        {
            Ok(rows) => {
                for row in rows {
                    match row {
                        Ok((lang, count)) => {
                            symbols_by_language.insert(lang, count);
                        }
                        Err(e) => tracing::warn!(error = %e, "code.sqlite symbols row"),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "code.sqlite symbols query"),
        },
        Err(e) => tracing::warn!(error = %e, "code.sqlite symbols prepare"),
    }

    let mut top_files_by_tokens: Vec<FileTokenRow> = Vec::new();
    match conn.prepare(
        "SELECT path, token_count FROM files WHERE token_count IS NOT NULL ORDER BY token_count DESC LIMIT 10",
    ) {
        Ok(mut stmt) => match stmt.query_map([], |row| {
            Ok(FileTokenRow { path: row.get(0)?, token_count: row.get(1)? })
        }) {
            Ok(rows) => {
                for row in rows {
                    match row {
                        Ok(file) => top_files_by_tokens.push(file),
                        Err(e) => tracing::warn!(error = %e, "code.sqlite files row"),
                    }
                }
            }
            Err(e) => tracing::warn!(error = %e, "code.sqlite files query"),
        },
        Err(e) => tracing::warn!(error = %e, "code.sqlite files prepare"),
    }

    CodeSummary { symbols_by_language, top_files_by_tokens }
}

fn collect_sessions(ctx: &Context) -> Result<SessionsSummary> {
    stats::init_stats_schema(&ctx.conn)?;
    let agg = stats::get_aggregate_stats(&ctx.conn)?;
    let tool_rows: Vec<ToolRow> = stats::get_aggregate_tool_usage(&ctx.conn)?
        .into_iter()
        .take(10)
        .map(|t| ToolRow { tool_name: t.tool_name, call_count: t.call_count })
        .collect();

    Ok(SessionsSummary {
        total_sessions: agg.total_sessions,
        total_calls: agg.total_calls,
        top_tools: tool_rows,
    })
}

fn collect_hooks(mdkb_dir: &Path) -> HooksSummary {
    let cutoff = chrono::Utc::now().timestamp() - 7 * 86_400;

    let slow_events_7d = count_slow_events(mdkb_dir, cutoff);
    let reindex_queue_pending = count_jsonl_lines(&mdkb_dir.join("reindex-queue.jsonl"));

    HooksSummary { slow_events_7d, reindex_queue_pending }
}

fn count_slow_events(mdkb_dir: &Path, since_ts: i64) -> usize {
    let path = mdkb_dir.join("hook-slow.jsonl");
    let Ok(content) = std::fs::read_to_string(&path) else { return 0 };
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

fn count_jsonl_lines(path: &Path) -> usize {
    let Ok(content) = std::fs::read_to_string(path) else { return 0 };
    content.lines().filter(|l| !l.trim().is_empty()).count()
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
            add_entry(&self.ctx.conn, &MemoryEntry {
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
            }).expect("add_entry");
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
        assert_eq!(report.memory.counts_by_type.get("topic").copied().unwrap_or(0), 2);
        assert_eq!(report.memory.counts_by_type.get("problem").copied().unwrap_or(0), 1);
        assert_eq!(report.memory.counts_by_type.get("decision").copied().unwrap_or(0), 1);
        assert_eq!(report.memory.active_count, 4);
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

        let line_old = format!("{{\"event\":\"session-start\",\"elapsed_ms\":400,\"ts\":{}}}\n", now - 8 * 86_400);
        let line_new = format!("{{\"event\":\"session-start\",\"elapsed_ms\":400,\"ts\":{}}}\n", now - 3600);
        std::fs::write(mdkb_dir.join("hook-slow.jsonl"), line_old + &line_new).unwrap();

        let report = collect_report(&env.ctx).expect("collect");
        assert_eq!(report.hooks.slow_events_7d, 1, "only recent event counted");
    }

    #[test]
    fn count_slow_events_handles_malformed_and_missing_ts() {
        let dir = tempfile::tempdir().unwrap();
        let cutoff = 100;
        let content = [
            "not json",                            // parse error → skipped
            "{\"event\":\"x\"}",                   // missing ts → skipped
            "{\"ts\":50}",                         // below cutoff → skipped
            "{\"ts\":100}",                        // at cutoff → counted
            "{\"ts\":200}",                        // above cutoff → counted
            "{\"ts\":\"string\"}",                 // non-integer ts → skipped
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
        assert!(out.contains("(no entries)"), "empty memory must render placeholder");
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
                repo: "t".into(), version: "0.0.0".into(), db_size_bytes: 0, last_updated: None,
            },
            index: IndexHealth { document_count: 0, memory_count: 0, free_page_ratio: 0.0 },
            collections: CollectionsSummary { collections: vec![] },
            memory: MemorySummary {
                active_count: 0,
                counts_by_type: HashMap::new(),
                reminders_due: 0,
                reminders_upcoming_7d: 0,
            },
            code: CodeSummary {
                symbols_by_language: HashMap::new(),
                top_files_by_tokens: vec![],
            },
            sessions: SessionsSummary {
                total_sessions: 0,
                total_calls: 0,
                top_tools: vec![],
            },
            hooks: HooksSummary { slow_events_7d: 0, reindex_queue_pending: 0 },
        }
    }

    #[test]
    fn hooks_summary_counts_reindex_queue() {
        let env = Env::new();
        let mdkb_dir = env.ctx.db_path.parent().unwrap();
        std::fs::write(
            mdkb_dir.join("reindex-queue.jsonl"),
            "{\"path\":\"a.rs\"}\n{\"path\":\"b.rs\"}\n",
        ).unwrap();

        let report = collect_report(&env.ctx).expect("collect");
        assert_eq!(report.hooks.reindex_queue_pending, 2);
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
