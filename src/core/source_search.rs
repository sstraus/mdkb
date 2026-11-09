//! Read-only source occurrence search; independent of the code index.
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use regex::{Regex, RegexBuilder};
use serde::Serialize;

use crate::code::indexing::walker::{self, WalkOptions};
use crate::config::Config;
use crate::error::{Error, Result};

const MAX_ENTRIES: usize = 100_000;
const MAX_FILES: usize = 20_000;
const MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_LIMIT: usize = 1_000;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct SourceRequest<'a> {
    pub patterns: &'a [String],
    pub path: Option<&'a str>,
    pub regex: bool,
    pub ignore_case: bool,
    pub limit: usize,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SourceHit {
    pub repo: String,
    pub file: String,
    pub line: usize,
    /// One-based UTF-8 byte column in the live source line.
    pub column: usize,
    pub content: String,
    pub matched_patterns: Vec<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceCoverage {
    pub files_scanned: usize,
    /// The ignore walker omits excluded paths without enumerating their contents.
    /// `None` is unknown, not an assertion that no files were excluded.
    pub excluded_by_policy: Option<usize>,
    pub policy: Vec<&'static str>,
    pub failed_files: Vec<String>,
    pub entries_seen: usize,
    pub bytes_scanned: u64,
    pub output_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceFreshness {
    pub state: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceResult {
    pub scope: &'static str,
    pub unit: &'static str,
    pub status: &'static str,
    pub hits: Vec<SourceHit>,
    pub truncated: bool,
    pub coverage: SourceCoverage,
    pub freshness: SourceFreshness,
}

enum Matcher {
    Literal(String),
    Regex(Regex),
}

impl Matcher {
    fn find(&self, line: &str) -> Option<usize> {
        match self {
            Self::Literal(pattern) => line.find(pattern),
            Self::Regex(pattern) => pattern.find(line).map(|hit| hit.start()),
        }
    }
}

/// Scan the eligible working tree, not an indexed snapshot. The byte and entry
/// bounds protect the caller from a large or hostile repository; reaching one
/// returns `partial`, never a clean negative.
pub fn search(root: &Path, request: SourceRequest<'_>) -> Result<SourceResult> {
    if request.patterns.is_empty() || request.patterns.len() > 64 {
        return Err(Error::other("source search needs 1..64 patterns"));
    }
    if request.limit == 0 || request.limit > MAX_LIMIT {
        return Err(Error::other(format!(
            "source limit must be 1..={MAX_LIMIT}"
        )));
    }
    let mut matchers = Vec::with_capacity(request.patterns.len());
    for pattern in request.patterns {
        if pattern.trim().is_empty() || pattern.contains('\n') || pattern.contains('\r') {
            return Err(Error::other("source pattern must be nonempty and one line"));
        }
        if request.regex {
            let regex = RegexBuilder::new(pattern)
                .case_insensitive(request.ignore_case)
                .build()
                .map_err(|error| Error::other(format!("invalid source regex: {error}")))?;
            if regex.is_match("") {
                return Err(Error::other("source regex must not match an empty string"));
            }
            matchers.push(Matcher::Regex(regex));
        } else if request.ignore_case {
            let regex = RegexBuilder::new(&regex::escape(pattern))
                .case_insensitive(true)
                .build()
                .map_err(|error| {
                    Error::other(format!("invalid case-insensitive pattern: {error}"))
                })?;
            matchers.push(Matcher::Regex(regex));
        } else {
            matchers.push(Matcher::Literal(pattern.clone()));
        }
    }
    let root = crate::domain::canonicalize_plain(root)
        .map_err(|error| Error::other(format!("cannot resolve source root: {error}")))?;
    let selected = match request.path {
        None => None,
        Some(path) => {
            let path = Path::new(path);
            let target = if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            };
            let target = crate::domain::canonicalize_plain(&target)
                .map_err(|error| Error::other(format!("cannot resolve source path: {error}")))?;
            if !target.starts_with(&root) {
                return Err(Error::other("source path escapes repository root"));
            }
            Some(target)
        }
    };
    let config = Config::load_or_default(&root.join(".mdkb/config.toml"));
    let policy = &config.code.indexing;
    let options = || WalkOptions {
        root: &root,
        ignore_patterns: &policy.ignore_patterns,
        respect_gitignore: policy.respect_gitignore,
    };
    let mut result = SourceResult {
        scope: "source",
        unit: "source_line",
        status: "complete",
        hits: Vec::new(),
        truncated: false,
        coverage: SourceCoverage {
            files_scanned: 0,
            excluded_by_policy: None,
            policy: vec![
                if policy.respect_gitignore {
                    ".gitignore"
                } else {
                    ".mdkbignore"
                },
                "hidden",
                "nested-stores",
            ],
            failed_files: Vec::new(),
            entries_seen: 0,
            bytes_scanned: 0,
            output_bytes: 0,
        },
        freshness: SourceFreshness {
            state: "observed_live",
        },
    };
    let mut files = HashSet::<PathBuf>::new();
    collect_entries(
        walker::walk_builder(options()).build(),
        selected.as_deref(),
        &mut result,
        &mut files,
    );
    if policy.respect_gitignore && result.status == "complete" {
        let forced = walker::parse_mdkb_index_annotations(&root);
        if !forced.is_empty() {
            if let Some(builder) = walker::forced_walk_builder(&root, &forced) {
                collect_entries(
                    builder.build(),
                    selected.as_deref(),
                    &mut result,
                    &mut files,
                );
            } else {
                result.status = "partial";
                result
                    .coverage
                    .failed_files
                    .push("invalid force-include policy".into());
            }
        }
    }
    let mut files: Vec<PathBuf> = files.into_iter().collect();
    files.sort_by_key(|path| crate::domain::rel_key(path.strip_prefix(&root).unwrap_or(path)));
    for file in files {
        if result.coverage.bytes_scanned >= MAX_TOTAL_BYTES {
            result.status = "partial";
            result
                .coverage
                .failed_files
                .push("source byte bound reached".into());
            break;
        }
        scan_file(&file, &root, &matchers, request.limit, &mut result);
    }
    Ok(result)
}

fn collect_entries(
    walk: ignore::Walk,
    selected: Option<&Path>,
    result: &mut SourceResult,
    files: &mut HashSet<PathBuf>,
) {
    for entry in walk {
        result.coverage.entries_seen += 1;
        if result.coverage.entries_seen > MAX_ENTRIES || files.len() >= MAX_FILES {
            result.status = "partial";
            result
                .coverage
                .failed_files
                .push("source walk bound reached".into());
            break;
        }
        match entry {
            Ok(entry) if entry.file_type().is_some_and(|kind| kind.is_file()) => {
                let path = entry.path();
                if selected.is_none_or(|scope| path == scope || path.starts_with(scope)) {
                    files.insert(path.to_path_buf());
                }
            }
            Ok(_) => {}
            Err(error) => {
                result.status = "partial";
                result.coverage.failed_files.push(error.to_string());
            }
        }
    }
}

fn scan_file(
    path: &Path,
    root: &Path,
    matchers: &[Matcher],
    limit: usize,
    result: &mut SourceResult,
) {
    let rel = crate::domain::rel_key(path.strip_prefix(root).unwrap_or(path));
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            result.status = "partial";
            result.coverage.failed_files.push(format!("{rel}: {error}"));
            return;
        }
    };
    if metadata.len() > MAX_FILE_BYTES
        || metadata.len() > MAX_TOTAL_BYTES - result.coverage.bytes_scanned
    {
        result.status = "partial";
        result
            .coverage
            .failed_files
            .push(format!("{rel}: file exceeds scan bound"));
        return;
    }
    let resolved = match crate::domain::canonicalize_plain(path) {
        Ok(resolved) if resolved.starts_with(root) => resolved,
        _ => {
            result.status = "partial";
            result
                .coverage
                .failed_files
                .push(format!("{rel}: path cannot be verified inside root"));
            return;
        }
    };
    let max_read = MAX_FILE_BYTES.min(MAX_TOTAL_BYTES - result.coverage.bytes_scanned);
    let mut bytes = Vec::new();
    let read = std::fs::File::open(&resolved)
        .and_then(|file| file.take(max_read + 1).read_to_end(&mut bytes));
    if let Err(error) = read {
        result.status = "partial";
        result.coverage.failed_files.push(format!("{rel}: {error}"));
        return;
    }
    result.coverage.bytes_scanned += bytes.len() as u64;
    if bytes.len() as u64 > max_read {
        result.status = "partial";
        result
            .coverage
            .failed_files
            .push(format!("{rel}: file changed or exceeded scan bound"));
        return;
    }
    if bytes.contains(&0) {
        result.status = "partial";
        result
            .coverage
            .failed_files
            .push(format!("{rel}: binary file"));
        return;
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            result.status = "partial";
            result
                .coverage
                .failed_files
                .push(format!("{rel}: invalid UTF-8"));
            return;
        }
    };
    result.coverage.files_scanned += 1;
    for (index, line) in text.split_terminator('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let matched: Vec<(usize, usize)> = matchers
            .iter()
            .enumerate()
            .filter_map(|(i, matcher)| matcher.find(line).map(|column| (i, column)))
            .collect();
        if matched.is_empty() {
            continue;
        }
        if result.hits.len() == limit {
            result.truncated = true;
            continue;
        }
        let hit_bytes = line.len() + rel.len() + root.as_os_str().len() + matched.len() * 8;
        if hit_bytes > MAX_OUTPUT_BYTES.saturating_sub(result.coverage.output_bytes) {
            result.status = "partial";
            result.truncated = true;
            if !result
                .coverage
                .failed_files
                .iter()
                .any(|reason| reason == "source response byte bound reached")
            {
                result
                    .coverage
                    .failed_files
                    .push("source response byte bound reached".into());
            }
            continue;
        }
        result.coverage.output_bytes += hit_bytes;
        result.hits.push(SourceHit {
            repo: root.display().to_string(),
            file: rel.clone(),
            line: index + 1,
            column: matched.iter().map(|(_, column)| *column).min().unwrap_or(0) + 1,
            content: line.to_string(),
            matched_patterns: matched.iter().map(|(i, _)| *i).collect(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{SourceRequest, search};
    use std::fs;

    #[test]
    fn live_or_search_deduplicates_lines_and_reports_policy_and_bounds() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src/a.rs"), "α target target\r\ntarget\n").unwrap();
        fs::write(dir.path().join("src/b.rs"), "target\n").unwrap();
        let patterns = vec!["target".to_string(), "target".to_string()];
        let result = search(
            dir.path(),
            SourceRequest {
                patterns: &patterns,
                path: Some("src"),
                regex: false,
                ignore_case: false,
                limit: 2,
            },
        )
        .unwrap();
        assert_eq!(result.hits.len(), 2);
        assert_eq!(result.hits[0].file, "src/a.rs");
        assert_eq!(result.hits[0].column, 4); // UTF-8 byte column, not character column.
        assert_eq!(result.hits[0].content, "α target target");
        assert_eq!(result.hits[0].matched_patterns, [0, 1]);
        assert!(
            result.truncated,
            "a third distinct location exceeds the global limit"
        );
        assert_eq!(result.status, "complete");
    }

    #[test]
    fn binary_file_makes_a_zero_hit_partial_instead_of_proving_absence() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), [0, 1, 2]).unwrap();
        let result = search(
            dir.path(),
            SourceRequest {
                patterns: &["needle".to_string()],
                path: None,
                regex: false,
                ignore_case: false,
                limit: 10,
            },
        )
        .unwrap();
        assert!(result.hits.is_empty());
        assert_eq!(result.status, "partial");
        assert!(
            result
                .coverage
                .failed_files
                .iter()
                .any(|s| s.contains("binary file"))
        );
    }

    #[test]
    fn literal_pipe_is_not_or_and_regex_is_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "red\nblue\nred|blue\n").unwrap();
        let patterns = vec!["red|blue".to_string()];
        let literal = search(
            dir.path(),
            SourceRequest {
                patterns: &patterns,
                path: None,
                regex: false,
                ignore_case: false,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(literal.hits.iter().map(|h| h.line).collect::<Vec<_>>(), [3]);
        let regex = search(
            dir.path(),
            SourceRequest {
                patterns: &patterns,
                path: None,
                regex: true,
                ignore_case: false,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(
            regex.hits.iter().map(|h| h.line).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }
}
