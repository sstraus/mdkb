//! Hook dispatch handlers for Claude Code / Codex lifecycle events.
//!
//! Contract: every handler reads event JSON from stdin (best-effort, may be empty
//! or malformed), writes a JSON object to stdout, and the process exits 0.
//! The host CLI must never be blocked by mdkb — internal failures are logged
//! to stderr and swallowed. Actual recall / warmup / reindex logic lands in
//! the event-specific stories (015 SessionStart, 016 UserPromptSubmit, 017 PostToolUse).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde_json::{Value, json};

use crate::cli::HookEvent;
use crate::cli::handlers::Context;
use crate::config::{Config, HooksConfig};
use crate::store::memory::{get_warmup_index, search_entries_fts};

/// Entry point for `mdkb hook <event>`.
///
/// Always returns after writing a JSON object to stdout and exits 0-equivalent
/// (caller in `main` propagates the `Ok(())`).
pub fn dispatch(event: HookEvent) {
    let input = read_stdin_best_effort();
    let event_json = parse_event(&input);

    let response = match event {
        HookEvent::SessionStart => handle_session_start(event_json),
        HookEvent::UserPromptSubmit => handle_user_prompt_submit(event_json),
        HookEvent::PostToolUse => handle_post_tool_use(event_json),
    };

    emit_response(&response);
}

fn read_stdin_best_effort() -> String {
    let mut buf = String::new();
    let _ = io::stdin().read_to_string(&mut buf);
    buf
}

fn parse_event(input: &str) -> Value {
    if input.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(input).unwrap_or(Value::Null)
}

fn emit_response(value: &Value) {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    let stdout = io::stdout();
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{}", serialized);
    let _ = handle.flush();
}

/// Walk ancestors looking for `.mdkbignore-hooks` marker. Stops at the user's
/// home directory (never walks above it) to avoid picking up unrelated markers.
fn mdkbignore_hooks_present(start: &Path) -> bool {
    let home: Option<PathBuf> = std::env::var_os("HOME").map(PathBuf::from);
    let mut current: Option<&Path> = Some(start);
    while let Some(dir) = current {
        if dir.join(".mdkbignore-hooks").exists() {
            return true;
        }
        if let Some(h) = home.as_deref() {
            if dir == h {
                return false;
            }
        }
        current = dir.parent();
    }
    false
}

fn hooks_config(root: &Path) -> HooksConfig {
    let cfg_path = root.join(".mdkb").join("config.toml");
    Config::load_or_default(&cfg_path).hooks
}

fn log_slow_hook(root: &Path, event: &str, elapsed_ms: u128, budget_ms: u64) {
    let log_path = root.join(".mdkb").join("hook-slow.jsonl");
    let line = json!({
        "event": event,
        "elapsed_ms": elapsed_ms,
        "budget_ms": budget_ms,
        "ts": chrono::Utc::now().timestamp(),
    });
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(mut f) => {
            let _ = writeln!(f, "{}", line);
        }
        Err(e) => {
            tracing::warn!("log_slow_hook: failed to open {:?}: {}", log_path, e);
        }
    }
}

fn handle_session_start(_event: Value) -> Value {
    let start = Instant::now();
    let cwd: PathBuf = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return json!({}),
    };

    if mdkbignore_hooks_present(&cwd) {
        return json!({});
    }

    let mdkb_dir = cwd.join(".mdkb");
    if !mdkb_dir.is_dir() {
        return json!({});
    }

    let cfg = hooks_config(&cwd);
    if !cfg.session_start_enabled {
        return json!({});
    }

    let ctx = match Context::open(&cwd) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("session-start hook: Context::open failed: {e}");
            return json!({});
        }
    };

    let limit = cfg.warmup_limit.max(1);
    let lines = match get_warmup_index(&ctx.conn, limit) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("session-start hook: get_warmup_index failed: {e}");
            return json!({});
        }
    };

    if lines.is_empty() {
        return json!({});
    }

    let mut body = String::from("## mdkb memory warmup\n\n");
    for line in &lines {
        body.push_str("- ");
        body.push_str(line);
        body.push('\n');
    }

    let elapsed_ms = start.elapsed().as_millis();
    if elapsed_ms > cfg.latency_budget_ms as u128 {
        log_slow_hook(&cwd, "session-start", elapsed_ms, cfg.latency_budget_ms);
        body.push_str(
            "\n_(truncated: hook exceeded latency budget — run `mdkb memory list` for full view)_\n",
        );
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "SessionStart",
            "additionalContext": body,
        }
    })
}

/// Markers that indicate the user is wrapping up / clearing context.
/// Recall injection would be wasteful and disruptive at these points.
const WRAPUP_MARKERS: &[&str] = &["/wrapup", "/clear", "/compact", "/exit", "/quit"];

fn prompt_is_wrapup(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    WRAPUP_MARKERS
        .iter()
        .any(|m| trimmed.starts_with(m) || trimmed.eq_ignore_ascii_case(m.trim_start_matches('/')))
}

/// Common English/Italian stopwords stripped before FTS matching.
/// Conversational prompts contain many of these; leaving them in breaks
/// AND-based FTS match because content/title entries don't include them.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "or", "but", "the", "of", "to", "in", "on", "at", "by", "for", "with", "as",
    "is", "are", "was", "were", "be", "been", "being", "have", "has", "had", "do", "does", "did",
    "will", "would", "could", "should", "may", "might", "can", "shall", "we", "you", "i", "he",
    "she", "it", "they", "them", "us", "my", "your", "our", "their", "this", "that", "these",
    "those", "how", "what", "why", "when", "where", "who", "which", "so", "if", "then", "than",
    "about", "into", "from", "up", "down", "out", "over", "under", "not", "no", "yes", "il", "la",
    "le", "lo", "gli", "un", "uno", "una", "di", "da", "del", "della", "che", "e", "o", "ma", "se",
    "ci", "si", "mi", "ti", "per", "con", "su", "come", "quando", "perche", "cosa", "dove", "chi",
    "quale", "non", "sono", "era", "stato",
];

/// Build an FTS5 query string from a natural-language prompt by stripping
/// stopwords and keeping alphanumeric tokens ≥ 3 chars. Returns None when
/// the filtered query would be empty or too narrow to produce useful recall.
fn build_recall_query(prompt: &str) -> Option<String> {
    let tokens: Vec<String> = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|tok| {
            let t = tok.to_lowercase();
            if t.len() < 3 {
                return None;
            }
            if STOPWORDS.contains(&t.as_str()) {
                return None;
            }
            Some(t)
        })
        .collect();

    if tokens.is_empty() {
        return None;
    }

    // Join with OR so a conversational prompt matches on any keyword.
    // Wrap each token in quotes to neutralize FTS operators inside.
    let query = tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    Some(query)
}

fn handle_user_prompt_submit(event: Value) -> Value {
    let start = Instant::now();
    let cwd: PathBuf = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return json!({}),
    };

    let prompt = match event.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.to_string(),
        _ => return json!({}),
    };

    if prompt_is_wrapup(&prompt) {
        return json!({});
    }

    if mdkbignore_hooks_present(&cwd) {
        return json!({});
    }

    if !cwd.join(".mdkb").is_dir() {
        return json!({});
    }

    let cfg = hooks_config(&cwd);
    if !cfg.user_prompt_submit_enabled {
        return json!({});
    }

    let ctx = match Context::open(&cwd) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("user-prompt-submit hook: Context::open failed: {e}");
            return json!({});
        }
    };

    let fts_query = match build_recall_query(&prompt) {
        Some(q) => q,
        None => return json!({}),
    };

    let limit = cfg.recall_limit.max(1);
    let results = match search_entries_fts(&ctx.conn, &fts_query, limit) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("user-prompt-submit hook: search_entries_fts failed: {e}");
            return json!({});
        }
    };

    if results.is_empty() {
        return json!({});
    }

    let mut body = String::from("## mdkb: relevant context\n\n");
    for entry in &results {
        let snippet_raw = entry.content.trim().replace('\n', " ");
        let snippet: String = snippet_raw.chars().take(160).collect();
        body.push_str(&format!("- [{}] {} — {}\n", entry.id, entry.title, snippet));
    }

    let elapsed_ms = start.elapsed().as_millis();
    if elapsed_ms > cfg.latency_budget_ms as u128 {
        log_slow_hook(
            &cwd,
            "user-prompt-submit",
            elapsed_ms,
            cfg.latency_budget_ms,
        );
    }

    json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": body,
        }
    })
}

/// Resolve `raw` relative to `base`, canonicalize, and return the canonical
/// path only if it is strictly under `base`. Returns `None` for traversal
/// attempts or paths that cannot be resolved on disk.
///
/// Strategy: canonicalize `base` first (resolves platform symlinks like
/// `/tmp` -> `/private/tmp` on macOS). Then join `raw` onto the canonical
/// base and collapse `..`/`.` lexically. This avoids hitting the FS for the
/// target file (it may not exist yet for Write/Edit on new files) while
/// still producing a path that is comparable to `base_canonical`.
fn canonicalize_under_cwd(base: &Path, raw: &str) -> Option<String> {
    // Canonicalize the base directory. If it doesn't exist we can't proceed.
    let base_canonical = std::fs::canonicalize(base).ok()?;

    let joined = if Path::new(raw).is_absolute() {
        // For absolute paths, try to canonicalize the parent to resolve symlinks
        // (e.g. /tmp → /private/tmp on macOS), then re-append the filename.
        let abs = PathBuf::from(raw);
        if let Some(parent) = abs.parent() {
            if let Ok(canon_parent) = std::fs::canonicalize(parent) {
                if let Some(fname) = abs.file_name() {
                    canon_parent.join(fname)
                } else {
                    canon_parent
                }
            } else {
                abs
            }
        } else {
            abs
        }
    } else {
        base_canonical.join(raw)
    };

    // Prefer fs::canonicalize (resolves symlinks in the target); fall back to
    // lexical normalization for files that don't exist yet (new Write targets).
    let canonical = std::fs::canonicalize(&joined).unwrap_or_else(|_| {
        // Lexical collapse of `..` and `.` components. Because `joined` is
        // already rooted at the canonical base, the result is canonical too.
        let mut out = PathBuf::new();
        for comp in joined.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                c => out.push(c),
            }
        }
        out
    });

    if canonical.starts_with(&base_canonical) {
        Some(canonical.to_string_lossy().into_owned())
    } else {
        None
    }
}

/// Tool names whose output may modify on-disk files we want to reindex.
const REINDEX_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];

/// Extract a file path from a tool_input blob. Handles the common cases:
/// - `file_path` (Edit/Write/MultiEdit)
/// - `notebook_path` (NotebookEdit)
fn tool_input_path(tool_input: &Value) -> Option<String> {
    for key in &["file_path", "notebook_path"] {
        if let Some(s) = tool_input.get(*key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

fn handle_post_tool_use(event: Value) -> Value {
    let cwd: PathBuf = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return json!({}),
    };

    if mdkbignore_hooks_present(&cwd) {
        return json!({});
    }

    let mdkb_dir = cwd.join(".mdkb");
    if !mdkb_dir.is_dir() {
        return json!({});
    }

    let cfg = hooks_config(&cwd);
    if !cfg.post_tool_use_enabled {
        return json!({});
    }

    let tool_name = match event.get("tool_name").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return json!({}),
    };
    if !REINDEX_TOOLS.contains(&tool_name) {
        return json!({});
    }

    let raw_path = match event.get("tool_input").and_then(tool_input_path) {
        Some(p) => p,
        None => return json!({}),
    };

    // Security: canonicalize and verify the path is under cwd before enqueuing.
    // This prevents path traversal (e.g. "../../etc/passwd") from reaching the queue.
    let path = match canonicalize_under_cwd(&cwd, &raw_path) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "post-tool-use hook: rejected path outside cwd: {}",
                raw_path
            );
            return json!({});
        }
    };

    let queue_path = mdkb_dir.join("reindex-queue.jsonl");
    let line = json!({
        "path": path,
        "tool": tool_name,
        "at": chrono::Utc::now().timestamp(),
    });

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&queue_path)
    {
        let _ = writeln!(f, "{}", line);
    } else {
        tracing::warn!(
            "post-tool-use hook: failed to open reindex queue {}",
            queue_path.display()
        );
    }

    json!({})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base() -> PathBuf {
        std::env::temp_dir().join("mdkb_hook_tests")
    }

    #[test]
    fn path_traversal_rejected() {
        let base = tmp_base();
        std::fs::create_dir_all(&base).unwrap();
        assert!(
            canonicalize_under_cwd(&base, "../../etc/passwd").is_none(),
            "traversal via ../../etc/passwd must be rejected"
        );
    }

    #[test]
    fn path_traversal_absolute_outside_rejected() {
        let base = tmp_base();
        std::fs::create_dir_all(&base).unwrap();
        assert!(
            canonicalize_under_cwd(&base, "/etc/passwd").is_none(),
            "absolute path outside cwd must be rejected"
        );
    }

    #[test]
    fn valid_relative_path_accepted() {
        let base = tmp_base();
        std::fs::create_dir_all(&base).unwrap();
        let result = canonicalize_under_cwd(&base, "foo/bar.md");
        assert!(
            result.is_some(),
            "relative path inside cwd must be accepted"
        );
        let s = result.unwrap();
        assert!(s.contains("mdkb_hook_tests"), "result should be under base");
    }

    #[test]
    fn dot_dot_in_middle_rejected() {
        let base = tmp_base();
        std::fs::create_dir_all(&base).unwrap();
        assert!(
            canonicalize_under_cwd(&base, "subdir/../../etc/shadow").is_none(),
            "mid-path traversal must be rejected"
        );
    }

    #[test]
    fn absolute_path_inside_cwd_accepted() {
        let base = tmp_base();
        std::fs::create_dir_all(&base).unwrap();
        let base_real = std::fs::canonicalize(&base).unwrap();
        let inside = base_real
            .join("notes/foo.md")
            .to_string_lossy()
            .into_owned();
        let result = canonicalize_under_cwd(&base, &inside);
        assert!(
            result.is_some(),
            "absolute path inside cwd must be accepted"
        );
    }
}
