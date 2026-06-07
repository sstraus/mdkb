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

// ── Call-graph prompt detection ──────────────────────────────────────────────

/// Patterns that signal the user is asking about call relationships.
/// Returns true when the prompt matches a call-graph query.
pub fn prompt_wants_call_graph(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    const PATTERNS: &[&str] = &[
        "who calls",
        "what calls",
        "callers of",
        "callees of",
        "impact of",
        "impact analysis",
        "call graph",
        "call chain",
        "chi chiama",
        "cosa chiama",
        "chi usa",
        "where is",
        "used by",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
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

/// Minimal shell tokenizer: splits on whitespace, respecting single and double
/// quotes. Not a full shell parser — just enough to pull the search pattern out
/// of a `grep`/`rg` invocation. Quotes are stripped from the resulting tokens.
fn tokenize_shell(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                    started = true;
                } else if c.is_whitespace() {
                    if started {
                        out.push(std::mem::take(&mut cur));
                        started = false;
                    }
                } else {
                    cur.push(c);
                    started = true;
                }
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// Short flags that consume the following token as their value (e.g. `grep -A 3`,
/// `rg -g '*.rs'`). Used to skip over flag values when locating the pattern.
const VALUE_TAKING_SHORT: &[char] = &['A', 'B', 'C', 'm', 'f', 'd', 'g', 't'];

/// Extract the search pattern (and optional path) from a tokenized `grep`/`rg`
/// command. Returns `None` when the command is not a filesystem search — most
/// importantly a bare `grep PATTERN` with no path and no `-r`, which reads stdin
/// and is a pipe filter, not a codebase search mdkb could replace.
fn extract_grep_pattern(tokens: &[String]) -> Option<(String, Option<String>)> {
    let cmd = tokens.first()?;
    let base = cmd.rsplit('/').next().unwrap_or(cmd);
    let is_rg = match base {
        "rg" => true,
        "grep" | "egrep" | "fgrep" => false,
        _ => return None,
    };

    let mut explicit_pattern: Option<String> = None;
    let mut positionals: Vec<String> = Vec::new();
    let mut recursive = is_rg; // ripgrep searches the tree by default
    let mut i = 1;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "-e" || t == "--regexp" {
            explicit_pattern = tokens.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if let Some(v) = t.strip_prefix("--regexp=") {
            explicit_pattern = Some(v.to_string());
            i += 1;
            continue;
        }
        if let Some(long) = t.strip_prefix("--") {
            if long == "recursive" {
                recursive = true;
            }
            // Long flags that take a separate value token (no `=` form present).
            let takes_value = matches!(
                long,
                "include"
                    | "exclude"
                    | "glob"
                    | "type"
                    | "file"
                    | "max-count"
                    | "context"
                    | "after-context"
                    | "before-context"
            ) && !long.contains('=');
            i += if takes_value { 2 } else { 1 };
            continue;
        }
        if t.len() > 1 && t.starts_with('-') {
            if t.contains('r') || t.contains('R') {
                recursive = true;
            }
            let last = t.chars().last().unwrap();
            i += if VALUE_TAKING_SHORT.contains(&last) {
                2
            } else {
                1
            };
            continue;
        }
        positionals.push(t.clone());
        i += 1;
    }

    let (pattern, path) = if let Some(p) = explicit_pattern {
        (p, positionals.into_iter().next())
    } else {
        let mut it = positionals.into_iter();
        (it.next()?, it.next())
    };

    // grep without -r and without a path target reads stdin — it's a stdout
    // filter, not a codebase search. ripgrep defaults to a recursive tree scan.
    if !recursive && path.is_none() {
        return None;
    }
    Some((pattern, path))
}

/// Split a shell command into its filesystem-reading "source" commands,
/// respecting quotes so that operators inside a pattern (e.g. the `|` in
/// `grep "a|b"`) are not treated as shell separators.
///
/// A segment is a source when it begins the command or follows `;`, `&&`, or
/// `||`. A segment following a single `|` is a stdin filter, not a file search,
/// and is excluded.
fn pipeline_sources(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments: Vec<(String, bool)> = Vec::new();
    let mut cur = String::new();
    let mut cur_is_source = true;
    let mut quote: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            cur.push(c);
            i += 1;
            continue;
        }
        let next = chars.get(i + 1).copied();
        if c == ';' {
            segments.push((std::mem::take(&mut cur), cur_is_source));
            cur_is_source = true;
            i += 1;
        } else if (c == '&' && next == Some('&')) || (c == '|' && next == Some('|')) {
            // `&&` and `||` both end a statement and start a fresh source.
            segments.push((std::mem::take(&mut cur), cur_is_source));
            cur_is_source = true;
            i += 2;
        } else if c == '|' {
            segments.push((std::mem::take(&mut cur), cur_is_source));
            cur_is_source = false; // next stage reads stdin
            i += 1;
        } else {
            cur.push(c);
            i += 1;
        }
    }
    segments.push((cur, cur_is_source));
    segments
        .into_iter()
        .filter_map(|(s, is_source)| is_source.then_some(s))
        .collect()
}

/// Unwrap a `sh|bash|zsh -lc "…"` wrapper to the inner script. Codex runs every
/// shell command this way (`/bin/zsh -lc "rg …"`), so without unwrapping the
/// classifier would only ever see the shell binary, never the `grep`/`rg`.
/// Returns `None` when the command is not such a wrapper.
fn unwrap_shell_c(command: &str) -> Option<String> {
    let (shell, mut rest) = command.trim_start().split_once(char::is_whitespace)?;
    let base = shell.rsplit('/').next().unwrap_or(shell);
    if !matches!(base, "sh" | "bash" | "zsh" | "dash" | "ksh") {
        return None;
    }
    // Skip flag tokens until the one carrying `c` (e.g. `-c`, `-lc`, `-lic`).
    loop {
        rest = rest.trim_start();
        if !rest.starts_with('-') {
            return None;
        }
        let (flag, after) = rest.split_once(char::is_whitespace)?;
        rest = after;
        if flag.contains('c') {
            break;
        }
    }
    let inner = rest.trim();
    let unquoted = match inner.chars().next() {
        Some(q @ ('"' | '\'')) if inner.len() >= 2 && inner.ends_with(q) => {
            &inner[1..inner.len() - 1]
        }
        _ => inner,
    };
    Some(unquoted.to_string())
}

/// Classify a `Bash` command that runs `grep`/`rg` against the filesystem,
/// reusing the same redirection logic as a `Grep` tool call. Returns
/// `Some(suggestion)` only for the SOURCE stage of a pipeline — `… | grep x`
/// filters stdout and is left alone, since mdkb is not a substitute for it.
pub fn classify_bash_search(command: &str, bin: &str) -> Option<String> {
    let unwrapped = unwrap_shell_c(command);
    let command = unwrapped.as_deref().unwrap_or(command);
    for source in pipeline_sources(command) {
        let source = source.trim();
        if source.is_empty() {
            continue;
        }
        let mut tokens = tokenize_shell(source);
        // Drop a leading `rtk` proxy prefix (e.g. `rtk rg pattern`).
        if tokens.first().map(String::as_str) == Some("rtk") && tokens.len() > 1 {
            tokens.remove(0);
        }
        let Some((pattern, path)) = extract_grep_pattern(&tokens) else {
            continue;
        };
        if let Some(text) = classify_definition_search(&pattern, bin)
            .or_else(|| classify_grep_pattern(&pattern, path.as_deref(), bin))
        {
            return Some(text);
        }
    }
    None
}

/// Detect whether a `Bash` command invokes mdkb itself (`mdkb …`, an absolute
/// `…/mdkb …`, or the `mdkb-bridge.js` proxy). Used as a conversion signal: a
/// PreToolUse fire followed by an mdkb invocation means the redirect worked.
pub fn is_mdkb_invocation(command: &str) -> bool {
    let unwrapped = unwrap_shell_c(command);
    let command = unwrapped.as_deref().unwrap_or(command);
    for source in pipeline_sources(command) {
        let mut tokens = tokenize_shell(source.trim());
        if tokens.first().map(String::as_str) == Some("rtk") && tokens.len() > 1 {
            tokens.remove(0);
        }
        if let Some(first) = tokens.first() {
            if first.rsplit('/').next().unwrap_or(first) == "mdkb" {
                return true;
            }
        }
        if tokens.iter().any(|t| t.ends_with("mdkb-bridge.js")) {
            return true;
        }
    }
    false
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

    // ── prompt_wants_call_graph ───────────────────────────────────────────

    #[test]
    fn call_graph_english_patterns() {
        assert!(prompt_wants_call_graph("who calls dispatch?"));
        assert!(prompt_wants_call_graph("What calls handle_update?"));
        assert!(prompt_wants_call_graph("callers of write_single_memory"));
        assert!(prompt_wants_call_graph("show me the impact of this change"));
        assert!(prompt_wants_call_graph("I need a call graph for this"));
    }

    #[test]
    fn call_graph_italian_patterns() {
        assert!(prompt_wants_call_graph("chi chiama dispatch?"));
        assert!(prompt_wants_call_graph("cosa chiama questa funzione?"));
        assert!(prompt_wants_call_graph("chi usa memory_write?"));
    }

    #[test]
    fn call_graph_negative() {
        assert!(!prompt_wants_call_graph("fix the bug in dispatch"));
        assert!(!prompt_wants_call_graph("add error handling"));
        assert!(!prompt_wants_call_graph("refactor this function"));
    }

    // ── classify_bash_search ──────────────────────────────────────────────

    #[test]
    fn bash_rg_identifier_fires() {
        // ripgrep searches the tree by default — a bare identifier is redirectable.
        assert!(classify_bash_search("rg handle_session_start", "mdkb").is_some());
    }

    #[test]
    fn bash_grep_recursive_identifier_fires() {
        assert!(classify_bash_search("grep -rn dispatch_call src/", "mdkb").is_some());
    }

    #[test]
    fn bash_grep_definition_fires() {
        // `grep -r "fn handle_update"` → definition search redirect.
        assert!(classify_bash_search("grep -rn \"fn handle_update\" src", "mdkb").is_some());
    }

    #[test]
    fn bash_pipe_filter_silent() {
        // `… | grep x` filters stdout — mdkb is not a substitute.
        assert!(classify_bash_search("cargo test 2>&1 | grep handle_update", "mdkb").is_none());
    }

    #[test]
    fn bash_bare_grep_stdin_silent() {
        // grep without -r and without a path reads stdin — not a codebase search.
        assert!(classify_bash_search("grep handle_update", "mdkb").is_none());
    }

    #[test]
    fn bash_single_file_grep_silent() {
        // Single-file target: Grep is the right tool (handled by classify_grep_pattern).
        assert!(classify_bash_search("grep handle_update src/lib.rs", "mdkb").is_none());
    }

    #[test]
    fn bash_regex_pattern_silent() {
        // Complex regex is grep's job, not mdkb's.
        assert!(classify_bash_search("rg \"handle_.*_start\\(\"", "mdkb").is_none());
    }

    #[test]
    fn bash_rtk_prefix_fires() {
        // RTK proxy prefix is stripped before classification.
        assert!(classify_bash_search("rtk rg handle_session_start", "mdkb").is_some());
    }

    #[test]
    fn bash_grep_in_second_statement_fires() {
        // `cd foo && grep -r X` — the grep is the source stage of the 2nd statement.
        assert!(classify_bash_search("cd src && grep -rn dispatch_call .", "mdkb").is_some());
    }

    #[test]
    fn bash_non_search_command_silent() {
        assert!(classify_bash_search("cargo build --release", "mdkb").is_none());
        assert!(classify_bash_search("ls -la && cat README.md", "mdkb").is_none());
    }

    #[test]
    fn bash_rg_with_glob_value_flag_fires() {
        // `-g '*.rs'` consumes its value; the identifier after it is the pattern.
        assert!(classify_bash_search("rg -g '*.rs' handle_update", "mdkb").is_some());
    }

    #[test]
    fn bash_grep_e_flag_pattern_fires() {
        // `-e PATTERN` carries the pattern explicitly.
        assert!(classify_bash_search("grep -rn -e dispatch_call src/", "mdkb").is_some());
    }

    #[test]
    fn bash_quoted_alternation_silent() {
        // A `|` alternation INSIDE quotes is a multi-term regex grep does best —
        // the pipe must not be treated as a shell separator that truncates it.
        assert!(
            classify_bash_search(
                "grep -rniE \"resolved|GetInvolvedTicket|jira\" internal",
                "mdkb"
            )
            .is_none()
        );
    }

    #[test]
    fn bash_codex_zsh_wrapper_fires() {
        // Codex wraps commands as `/bin/zsh -lc "…"` — unwrap before classifying.
        assert!(
            classify_bash_search("/bin/zsh -lc \"rg -n handle_session_start src\"", "mdkb")
                .is_some()
        );
    }

    #[test]
    fn bash_codex_wrapper_alternation_silent() {
        // Most Codex greps are multi-term alternation — still correctly skipped.
        assert!(classify_bash_search("/bin/zsh -lc \"rg -n 'foo|bar|baz' src\"", "mdkb").is_none());
    }

    #[test]
    fn bash_quoted_alternation_then_real_pipe_filter_silent() {
        // Quoted alternation in the source AND a real `| grep` filter downstream.
        assert!(
            classify_bash_search("grep -rn \"a_thing|b_thing\" src/ | grep -v test", "mdkb")
                .is_none()
        );
    }

    // ── is_mdkb_invocation (conversion signal) ────────────────────────────

    #[test]
    fn mdkb_invocation_detected() {
        assert!(is_mdkb_invocation("mdkb search --scope symbols foo"));
        assert!(is_mdkb_invocation(
            "/Users/x/target/release/mdkb code callers bar"
        ));
        assert!(is_mdkb_invocation("wiz-run mdkb-bridge.js search foo"));
        assert!(is_mdkb_invocation("node /path/mdkb-bridge.js callers baz"));
        // Through a Codex-style wrapper.
        assert!(is_mdkb_invocation(
            "/bin/zsh -lc \"mdkb code-find handleAuth\""
        ));
    }

    #[test]
    fn mdkb_invocation_negative() {
        assert!(!is_mdkb_invocation("grep -rn foo src/"));
        assert!(!is_mdkb_invocation("cargo build"));
        // Substring, not the command — must not false-positive.
        assert!(!is_mdkb_invocation("cat mdkb-notes.txt"));
        assert!(!is_mdkb_invocation("echo mdkb"));
    }
}
