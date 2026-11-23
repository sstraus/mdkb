//! The symbol half of `memory audit` (story 081-1642): entries that cite a
//! function or method the code no longer has.
//!
//! A cited symbol is a backticked `Type::method`, `crate::path::item` or
//! `function()`. A bare backticked word is not one: `SQLite` or `TUICommander`
//! in prose would be reported as dead code every pass.
//!
//! "Dead" needs two answers, not one. The code index must no longer define the
//! name, AND `git grep` must find it nowhere in the tracked source. The index
//! alone would call every standard-library method dead (`unwrap()` is used
//! everywhere and defined nowhere in the repo); git alone cannot tell a
//! definition from a mention. A renamed function fails both.
//!
//! The pass refuses to answer instead of answering wrong: no code index, an
//! index older than HEAD, or no git repository skip it, and the outcome says so.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::store::memory_audit::AuditRow;

/// Backticked identifier path, optionally followed by `()`.
fn symbol_ref_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"`((?:[A-Za-z_][A-Za-z0-9_]*::)*[A-Za-z_][A-Za-z0-9_]*)(\(\))?`").unwrap()
    })
}

/// The name to look up for one backticked token, or `None` when the token is
/// not shaped like a code symbol of this repository.
fn lookup_name(path: &str, called: bool) -> Option<&str> {
    let segments: Vec<&str> = path.split("::").collect();
    let last = *segments.last()?;
    if segments.len() == 1 {
        return called.then_some(last);
    }
    // `std::fs::write` and `chrono::Utc::now` name other crates; only a type
    // path or this crate's own path is checked.
    let first = segments[0];
    let local = first.starts_with(|c: char| c.is_ascii_uppercase())
        || matches!(first, "crate" | "self" | "super");
    local.then_some(last)
}

/// Every (entry id, cited token, name to look up), in entry order.
pub(crate) fn collect(rows: &[AuditRow]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for row in rows {
        for cap in symbol_ref_regex().captures_iter(&row.content) {
            let path = &cap[1];
            let called = cap.get(2).is_some();
            if let Some(name) = lookup_name(path, called) {
                let token = format!("{path}{}", if called { "()" } else { "" });
                if !out
                    .iter()
                    .any(|(id, t, _): &(String, String, String)| *id == row.id && *t == token)
                {
                    out.push((row.id.clone(), token, name.to_string()));
                }
            }
        }
    }
    out
}

/// What one name turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolStatus {
    Live,
    Dead,
    CheckFailed,
}

/// The pass result: per-name status, or why the pass did not run.
pub(crate) enum SymbolPass {
    Checked(BTreeMap<String, SymbolStatus>),
    Skipped(String),
}

/// Resolve every distinct name once.
pub(crate) fn check(root: &Path, names: &[&str]) -> SymbolPass {
    if names.is_empty() {
        return SymbolPass::Checked(BTreeMap::new());
    }
    let facade = match crate::core::code::open_code_read_only(root) {
        Ok(f) => f,
        Err(_) => return SymbolPass::Skipped("no code index (run `mdkb update --code`)".into()),
    };
    let Some(head_at) = crate::git::head_commit_time(root) else {
        return SymbolPass::Skipped("not a git repository with a commit".into());
    };
    match facade.last_index_scan_at() {
        Ok(Some(scan_at)) if scan_at >= head_at => {}
        Ok(_) => {
            return SymbolPass::Skipped(
                "the code index is older than HEAD (run `mdkb update --code`)".into(),
            );
        }
        Err(e) => return SymbolPass::Skipped(format!("the code index cannot be read: {e}")),
    }
    let mut statuses = BTreeMap::new();
    for name in names {
        if statuses.contains_key(*name) {
            continue;
        }
        let status = match facade.defines_symbol(name) {
            Ok(true) => SymbolStatus::Live,
            Ok(false) => match source_mentions(root, name) {
                Some(true) => SymbolStatus::Live,
                Some(false) => SymbolStatus::Dead,
                None => SymbolStatus::CheckFailed,
            },
            Err(_) => SymbolStatus::CheckFailed,
        };
        statuses.insert((*name).to_string(), status);
    }
    SymbolPass::Checked(statuses)
}

/// Does tracked source outside `.mdkb/` still mention `name` as a word?
/// `None` when git could not answer.
fn source_mentions(root: &Path, name: &str) -> Option<bool> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "grep",
            "-q",
            "-w",
            "-F",
            "-e",
            name,
            "--",
            ".",
            ":(exclude).mdkb",
        ])
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches prose and other crates' paths read as this repo's symbols.
    #[test]
    fn only_local_paths_and_calls_are_symbol_references() {
        assert_eq!(
            lookup_name("RepoMap::prune_gone", false),
            Some("prune_gone")
        );
        assert_eq!(
            lookup_name("crate::git::head_commit_time", false),
            Some("head_commit_time")
        );
        assert_eq!(lookup_name("classify", true), Some("classify"));
        assert_eq!(lookup_name("SQLite", false), None);
        assert_eq!(lookup_name("std::fs::write", false), None);
        assert_eq!(lookup_name("chrono::Utc::now", true), None);
    }
}
