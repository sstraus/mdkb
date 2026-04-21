//! Pure hook logic: classifier functions, query builder, path utilities.
//!
//! These are extracted from hook dispatch so they can be used by both the
//! in-process path and daemon-side dispatch methods without importing hooks.rs.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

// ── Stdin / JSON helpers ──────────────────────────────────────────────────────

pub fn read_stdin_best_effort() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

pub fn parse_event(input: &str) -> Value {
    if input.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(input).unwrap_or(Value::Null)
}

// ── Wrapup detection ─────────────────────────────────────────────────────────

/// Markers that indicate the user is wrapping up / clearing context.
/// Recall injection would be wasteful and disruptive at these points.
pub const WRAPUP_MARKERS: &[&str] = &["/wrapup", "/clear", "/compact", "/exit", "/quit"];

pub fn prompt_is_wrapup(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    WRAPUP_MARKERS
        .iter()
        .any(|m| trimmed.starts_with(m) || trimmed.eq_ignore_ascii_case(m.trim_start_matches('/')))
}

// ── FTS recall query ──────────────────────────────────────────────────────────

/// Common English/Italian stopwords stripped before FTS matching.
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
pub fn build_recall_query(prompt: &str) -> Option<String> {
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

// ── Path utilities ────────────────────────────────────────────────────────────

/// Tool names whose output may modify on-disk files we want to reindex.
pub const REINDEX_TOOLS: &[&str] = &["Edit", "Write", "NotebookEdit", "MultiEdit"];

/// Extract a file path from a tool_input blob. Handles the common cases:
/// - `file_path` (Edit/Write/MultiEdit)
/// - `notebook_path` (NotebookEdit)
pub fn tool_input_path(tool_input: &Value) -> Option<String> {
    for key in &["file_path", "notebook_path"] {
        if let Some(s) = tool_input.get(*key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
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
pub fn canonicalize_under_cwd(base: &Path, raw: &str) -> Option<String> {
    let base_canonical = std::fs::canonicalize(base).ok()?;

    let joined = if Path::new(raw).is_absolute() {
        // For absolute paths, canonicalize the parent to resolve symlinks
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

// ── .mdkbignore-hooks marker ──────────────────────────────────────────────────

/// Walk ancestors looking for `.mdkbignore-hooks` marker. Stops at the user's
/// home directory (never walks above it) to avoid picking up unrelated markers.
pub fn mdkbignore_hooks_present(start: &Path) -> bool {
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

// ── PreToolUse Grep classifiers ───────────────────────────────────────────────

/// Classify whether a Grep pattern looks like a symbol name that mdkb
/// could answer more efficiently via `search(scope="symbols")` or `code_graph`.
///
/// Returns `Some(suggestion_text)` when the pattern is redirectable, `None` otherwise.
pub fn classify_grep_pattern(pattern: &str, path: Option<&str>, bin: &str) -> Option<String> {
    if pattern.len() < 3 {
        return None;
    }

    // Single-file target: Grep is the right tool, don't suggest alternatives.
    if let Some(p) = path {
        let p = p.trim_end_matches('/');
        if p.contains('.') && !p.ends_with('/') && !p.is_empty() {
            let looks_like_file = std::path::Path::new(p).extension().is_some();
            if looks_like_file {
                return None;
            }
        }
    }

    // Complex regex: Grep is the right tool (unless it's a callsite pattern).
    let regex_meta = [
        '*', '+', '?', '{', '}', '[', ']', '(', ')', '^', '$', '|', '.',
    ];
    let has_regex = pattern.chars().any(|c| regex_meta.contains(&c));
    if has_regex {
        return classify_callsite_pattern(pattern, bin);
    }

    // Pure identifier: snake_case, CamelCase, SCREAMING_SNAKE, with optional :: separators.
    let is_ident = pattern
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':');
    if is_ident {
        return Some(format!(
            "Use `{} search --scope symbols \"{}\"` or `{} code callers {}` via Bash.",
            bin, pattern, bin, pattern
        ));
    }

    None
}

/// Check for callsite patterns like `function_name\(` or `ClassName\.method`.
pub fn classify_callsite_pattern(pattern: &str, bin: &str) -> Option<String> {
    let stripped = pattern
        .trim_end_matches("\\(")
        .trim_end_matches("\\.")
        .trim_end_matches('(');

    if stripped.len() < 3 {
        return None;
    }

    let is_ident = stripped
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':');

    if is_ident && stripped.len() < pattern.len() {
        return Some(format!("Use `{} code callers {}` via Bash.", bin, stripped));
    }

    None
}

/// Definition-search patterns: `fn X`, `struct X`, `class X`, etc.
pub const DEF_KEYWORDS: &[&str] = &[
    "fn ",
    "func ",
    "def ",
    "class ",
    "struct ",
    "impl ",
    "trait ",
    "type ",
    "enum ",
    "interface ",
    "const ",
    "let ",
    "var ",
    "pub fn ",
    "pub struct ",
    "pub enum ",
    "pub trait ",
    "async fn ",
    "pub async fn ",
];

pub fn classify_definition_search(pattern: &str, bin: &str) -> Option<String> {
    let lower = pattern.to_lowercase();
    for kw in DEF_KEYWORDS {
        if lower.starts_with(kw) {
            let symbol = pattern[kw.len()..].trim();
            if symbol.len() >= 2 && symbol.chars().all(|c| c.is_alphanumeric() || c == '_') {
                return Some(format!(
                    "Use `{} search --scope symbols \"{}\"` via Bash.",
                    bin, symbol
                ));
            }
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_base() -> PathBuf {
        std::env::temp_dir().join("mdkb_hook_logic_tests")
    }

    // ── canonicalize_under_cwd ─────────────────────────────────────────────

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
        assert!(
            result.unwrap().contains("mdkb_hook_logic_tests"),
            "result should be under base"
        );
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

    // ── classify_grep_pattern ──────────────────────────────────────────────

    #[test]
    fn classify_symbol_name() {
        let r = classify_grep_pattern("handle_post_tool_use", None, "mdkb");
        assert!(r.is_some(), "snake_case identifier must be classified");
        assert!(r.unwrap().contains("mdkb search"));
    }

    #[test]
    fn classify_camel_case_symbol() {
        let r = classify_grep_pattern("MemoryEntry", None, "mdkb");
        assert!(r.is_some(), "CamelCase identifier must be classified");
    }

    #[test]
    fn classify_scoped_symbol() {
        let r = classify_grep_pattern("hooks::dispatch", None, "mdkb");
        assert!(r.is_some(), "scoped identifier must be classified");
    }

    #[test]
    fn classify_short_pattern_skipped() {
        assert!(classify_grep_pattern("fn", None, "mdkb").is_none());
        assert!(classify_grep_pattern("ab", None, "mdkb").is_none());
    }

    #[test]
    fn classify_regex_not_permutable() {
        assert!(classify_grep_pattern("log.*Error", None, "mdkb").is_none());
        assert!(classify_grep_pattern("fn\\s+\\w+", None, "mdkb").is_none());
        assert!(classify_grep_pattern("[A-Z]+_CONFIG", None, "mdkb").is_none());
    }

    #[test]
    fn classify_callsite_pattern_detected() {
        let r = classify_grep_pattern("dispatch\\(", None, "mdkb");
        assert!(r.is_some(), "callsite pattern must be classified");
        assert!(r.unwrap().contains("code callers"));
    }

    #[test]
    fn classify_single_file_path_skipped() {
        let r = classify_grep_pattern("MemoryEntry", Some("src/store/memory.rs"), "mdkb");
        assert!(r.is_none(), "single-file grep should not be redirected");
    }

    #[test]
    fn classify_directory_path_not_skipped() {
        let r = classify_grep_pattern("MemoryEntry", Some("src/"), "mdkb");
        assert!(r.is_some(), "directory grep should suggest alternative");
    }

    #[test]
    fn classify_definition_search_detected() {
        let r = classify_definition_search("fn dispatch", "mdkb");
        assert!(r.is_some());
        assert!(r.unwrap().contains("mdkb search"));
    }

    #[test]
    fn classify_definition_pub_fn() {
        assert!(classify_definition_search("pub fn handle_update", "mdkb").is_some());
    }

    #[test]
    fn classify_definition_struct() {
        assert!(classify_definition_search("struct HooksConfig", "mdkb").is_some());
    }

    #[test]
    fn classify_non_definition_pattern() {
        assert!(classify_definition_search("random text here", "mdkb").is_none());
    }
}
