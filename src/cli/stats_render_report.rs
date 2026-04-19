//! ASCII renderer for `StatsReport`. Takes data, produces a `String`.
//!
//! Rendering and data collection are separate so tests can snapshot
//! the output without touching the database.

use std::fmt::Write as FmtWrite;

use crate::cli::stats_render::{bar, frame};
use crate::cli::stats_report::{
    CodeSummary, CollectionsSummary, HeaderInfo, HooksSummary, IndexHealth, MemorySummary,
    SessionsSummary, StatsReport,
};

const WIDTH: usize = 72;

/// Render a full `StatsReport` to a UTF-8 string.
///
/// Pass `color = true` to emit ANSI escape sequences; callers decide based
/// on tty detection and the `--no-color` flag.
pub fn render(report: &StatsReport, _color: bool) -> String {
    let mut out = String::with_capacity(4096);
    render_header(&mut out, &report.header);
    render_index(&mut out, &report.index);
    render_collections(&mut out, &report.collections);
    render_memory(&mut out, &report.memory);
    render_code(&mut out, &report.code);
    render_sessions(&mut out, &report.sessions);
    render_hooks(&mut out, &report.hooks);
    out
}

// ── Sections ─────────────────────────────────────────────────────────────────

fn render_header(out: &mut String, h: &HeaderInfo) {
    let db_kb = h.db_size_bytes / 1024;
    let last = h.last_updated
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "never".to_string());

    let body = format!(
        "  repo    {}\n  version {}\n  db size {} KB\n  updated {}",
        h.repo, h.version, db_kb, last
    );
    out.push_str(&frame("mdkb", &body, WIDTH));
}

fn render_index(out: &mut String, h: &IndexHealth) {
    let bar_w = 20;
    let free_bar = bar((h.free_page_ratio * 100.0) as u64, 100, bar_w);
    let body = format!(
        "  documents  {:>6}\n  memory     {:>6}\n  free pages [{free_bar}] {:.0}%",
        h.document_count, h.memory_count, h.free_page_ratio * 100.0,
    );
    out.push_str(&frame("Index Health", &body, WIDTH));
}

fn render_collections(out: &mut String, c: &CollectionsSummary) {
    if c.collections.is_empty() {
        out.push_str(&frame("Collections", "  (none)", WIDTH));
        return;
    }
    let mut body = String::new();
    for col in &c.collections {
        let _ = writeln!(body, "  {:20} {:>5} docs  {}", col.name, col.doc_count, col.path);
    }
    out.push_str(&frame("Collections", body.trim_end(), WIDTH));
}

fn render_memory(out: &mut String, m: &MemorySummary) {
    let max = m.counts_by_type.values().copied().max().unwrap_or(0) as u64;
    let bar_w = 16;

    let mut lines = Vec::new();
    let mut types: Vec<(&String, &usize)> = m.counts_by_type.iter().collect();
    types.sort_by(|a, b| b.1.cmp(a.1));

    for (t, count) in &types {
        let b = bar(**count as u64, max.max(1), bar_w);
        lines.push(format!("  {:10} [{b}] {:>4}", t, count));
    }

    if m.reminders_due > 0 {
        lines.push(format!("  reminders DUE       {:>4}", m.reminders_due));
    }
    if m.reminders_upcoming_7d > 0 {
        lines.push(format!("  upcoming (7d)       {:>4}", m.reminders_upcoming_7d));
    }
    if lines.is_empty() {
        lines.push("  (no entries)".to_string());
    }

    out.push_str(&frame("Memory", &lines.join("\n"), WIDTH));
}

fn render_code(out: &mut String, c: &CodeSummary) {
    if c.files == 0 {
        out.push_str(&frame("Code Index", "  (not indexed)", WIDTH));
        return;
    }

    let bar_w = 14;
    let mut lines = Vec::new();

    lines.push(format!(
        "  files {:>6}  symbols {:>6}  relations {:>6}",
        c.files, c.symbols, c.relations,
    ));
    if let Some(ts) = c.last_indexed
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
    {
        lines.push(format!("  indexed {}", ts.format("%Y-%m-%d %H:%M UTC")));
    }

    if !c.languages.is_empty() {
        let max = c.languages.iter().map(|l| l.symbols).max().unwrap_or(0) as u64;
        lines.push(String::new());
        lines.push("  Languages:".to_string());
        for l in &c.languages {
            let b = bar(l.symbols as u64, max.max(1), bar_w);
            lines.push(format!(
                "    {:10} [{b}] {:>4} files {:>5} syms",
                truncate(&l.language, 10), l.files, l.symbols,
            ));
        }
    }

    if !c.symbols_by_kind.is_empty() {
        let max = c.symbols_by_kind.iter().map(|k| k.count).max().unwrap_or(0) as u64;
        lines.push(String::new());
        lines.push("  Symbol kinds:".to_string());
        for k in &c.symbols_by_kind {
            let b = bar(k.count as u64, max.max(1), bar_w);
            lines.push(format!("    {:12} [{b}] {:>6}", truncate(&k.kind, 12), k.count));
        }
    }

    if !c.relations_by_kind.is_empty() {
        let max = c.relations_by_kind.iter().map(|k| k.count).max().unwrap_or(0) as u64;
        lines.push(String::new());
        lines.push("  Relations:".to_string());
        for k in &c.relations_by_kind {
            let b = bar(k.count as u64, max.max(1), bar_w);
            lines.push(format!("    {:12} [{b}] {:>6}", truncate(&k.kind, 12), k.count));
        }
    }

    if !c.top_files.is_empty() {
        lines.push(String::new());
        lines.push("  Top files by symbols:".to_string());
        for f in &c.top_files {
            lines.push(format!("    {:>5} syms  {}", f.symbols, truncate(&f.path, 48)));
        }
    }

    out.push_str(&frame("Code Index", &lines.join("\n"), WIDTH));
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn render_sessions(out: &mut String, s: &SessionsSummary) {
    let mut lines = Vec::new();
    lines.push(format!("  sessions {:>6}  calls {:>6}", s.total_sessions, s.total_calls));

    if !s.top_tools.is_empty() {
        let max_calls = s.top_tools.iter().map(|t| t.call_count).max().unwrap_or(1);
        let bar_w = 14;
        lines.push(String::new());
        lines.push("  Top tools:".to_string());
        for t in &s.top_tools {
            let b = bar(t.call_count as u64, max_calls as u64, bar_w);
            lines.push(format!("    {:22} [{b}] {:>5}", t.tool_name, t.call_count));
        }
    }

    out.push_str(&frame("Sessions", &lines.join("\n"), WIDTH));
}

fn render_hooks(out: &mut String, h: &HooksSummary) {
    let body = format!(
        "  slow events (7d)  {:>4}\n  reindex pending   {:>4}",
        h.slow_events_7d, h.reindex_queue_pending
    );
    out.push_str(&frame("Hooks", &body, WIDTH));
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::stats_report::*;
    use std::collections::HashMap;

    fn fixture_report() -> StatsReport {
        StatsReport {
            header: HeaderInfo {
                repo: "my-project".to_string(),
                version: "2.0.0".to_string(),
                db_size_bytes: 512 * 1024,
                last_updated: Some(1_700_000_000),
            },
            index: IndexHealth {
                document_count: 42,
                memory_count: 7,
                free_page_ratio: 0.1,
            },
            collections: CollectionsSummary {
                collections: vec![
                    CollectionRow {
                        name: "docs".to_string(),
                        path: "./docs".to_string(),
                        pattern: "**/*.md".to_string(),
                        doc_count: 42,
                    },
                ],
            },
            memory: MemorySummary {
                active_count: 7,
                counts_by_type: {
                    let mut m = HashMap::new();
                    m.insert("topic".to_string(), 4);
                    m.insert("decision".to_string(), 2);
                    m.insert("problem".to_string(), 1);
                    m
                },
                reminders_due: 1,
                reminders_upcoming_7d: 2,
            },
            code: CodeSummary {
                files: 12,
                symbols: 420,
                relations: 880,
                last_indexed: Some(1_700_000_000),
                languages: vec![
                    LanguageRow { language: "rust".to_string(), files: 8, symbols: 300 },
                    LanguageRow { language: "typescript".to_string(), files: 4, symbols: 120 },
                ],
                symbols_by_kind: vec![
                    KindCount { kind: "Function".to_string(), count: 210 },
                    KindCount { kind: "Struct".to_string(), count: 80 },
                ],
                relations_by_kind: vec![
                    KindCount { kind: "Calls".to_string(), count: 640 },
                    KindCount { kind: "Uses".to_string(), count: 240 },
                ],
                top_files: vec![
                    FileSymbolRow { path: "src/main.rs".to_string(), symbols: 120 },
                ],
            },
            sessions: SessionsSummary {
                total_sessions: 5,
                total_calls: 200,
                top_tools: vec![
                    ToolRow { tool_name: "search".to_string(), call_count: 80 },
                    ToolRow { tool_name: "memory_write".to_string(), call_count: 30 },
                ],
            },
            hooks: HooksSummary {
                slow_events_7d: 3,
                reindex_queue_pending: 0,
            },
        }
    }

    #[test]
    fn render_contains_all_section_titles() {
        let out = render(&fixture_report(), false);
        for title in &["mdkb", "Index Health", "Collections", "Memory", "Code Index", "Sessions", "Hooks"] {
            assert!(out.contains(title), "missing section '{title}'");
        }
    }

    #[test]
    fn render_header_shows_repo_and_version() {
        let out = render(&fixture_report(), false);
        assert!(out.contains("my-project"));
        assert!(out.contains("2.0.0"));
    }

    #[test]
    fn render_memory_shows_entry_types() {
        let out = render(&fixture_report(), false);
        assert!(out.contains("topic"));
        assert!(out.contains("decision"));
        assert!(out.contains("reminders DUE"));
    }

    #[test]
    fn render_code_shows_languages() {
        let out = render(&fixture_report(), false);
        assert!(out.contains("rust"));
        assert!(out.contains("typescript"));
    }

    #[test]
    fn render_sessions_shows_totals() {
        let out = render(&fixture_report(), false);
        assert!(out.contains('5'));   // sessions
        assert!(out.contains("200")); // calls
        assert!(out.contains("search"));
    }

    #[test]
    fn render_hooks_shows_slow_count() {
        let out = render(&fixture_report(), false);
        assert!(out.contains('3')); // slow_events_7d
    }

    #[test]
    fn render_empty_collections_shows_none() {
        let mut r = fixture_report();
        r.collections.collections.clear();
        let out = render(&r, false);
        assert!(out.contains("(none)"));
    }

    #[test]
    fn render_no_code_shows_not_indexed() {
        let mut r = fixture_report();
        r.code.files = 0;
        r.code.symbols = 0;
        r.code.relations = 0;
        r.code.languages.clear();
        r.code.symbols_by_kind.clear();
        r.code.relations_by_kind.clear();
        r.code.top_files.clear();
        let out = render(&r, false);
        assert!(out.contains("(not indexed)"));
    }

    #[test]
    fn render_code_shows_totals_and_sections() {
        let out = render(&fixture_report(), false);
        assert!(out.contains("files"));
        assert!(out.contains("symbols"));
        assert!(out.contains("relations"));
        assert!(out.contains("Languages:"));
        assert!(out.contains("Symbol kinds:"));
        assert!(out.contains("Relations:"));
        assert!(out.contains("Top files by symbols:"));
        assert!(out.contains("Function"));
        assert!(out.contains("Calls"));
        assert!(out.contains("src/main.rs"));
    }

    #[test]
    fn render_lines_all_fit_width() {
        let out = render(&fixture_report(), false);
        for line in out.lines() {
            let len = line.chars().count();
            assert!(
                len <= WIDTH,
                "line too wide ({len} > {WIDTH}): {line:?}"
            );
        }
    }
}
