//! `mdkb graph coupling` (story 081-1642): documents that change with a
//! source file, and the ones that stopped changing while the source moved on.
//!
//! `mdkb code coupling` drops every path the code index never parsed, because a
//! changelog or a lockfile pairs with every source file and says nothing. That
//! filter also drops every document. This report keeps exactly the pairs the
//! other one cannot see: one indexed document, one indexed source file.
//!
//! "Documents" are the files `mdkb update` indexed, read from the store's
//! `documents` table, minus the files that change in most commits by nature
//! ([`is_doc_noise`]). A pair is `drifted` when the source file had commits
//! after the last commit that touched the document.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use super::coupling::{
    CouplingOverrides, DEFAULT_MIN_COCHANGES, DEFAULT_SINCE, MAX_FILES_PER_COMMIT, count_cochanges,
    indexed_file_set,
};
use crate::error::{Error, Result};
use crate::git::{CoChangeHistory, co_change_history};

/// One document and one source file that change together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocPair {
    pub doc: String,
    pub source: String,
    pub cochanges: usize,
    /// Commits touching `source`, inside the window, newer than the newest
    /// commit touching `doc`. Above zero means the document may describe an
    /// older version of the code.
    pub source_commits_after_doc: usize,
}

/// The report, ready to print. `indexed` is false without a code index.
#[derive(Debug)]
pub struct DocCouplingReport {
    pub markdown: String,
    pub findings: Vec<DocPair>,
    pub indexed: bool,
}

/// Files that change in most commits by nature: release notes, generated
/// agent summaries, work-tracking files and the memory projection. Pairing
/// them with source is how commits work, not a finding.
pub fn is_doc_noise(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name.starts_with("changes")
        || name.starts_with("changelog")
        || name == "llms.txt"
        || name.starts_with("todo")
        || ["stories/", "plans/", "reviews/", ".mdkb/"]
            .iter()
            .any(|dir| path.starts_with(dir) || path.contains(&format!("/{dir}")))
}

/// Repo-relative paths of the documents the store indexed, noise removed.
fn document_paths(root: &Path) -> Result<HashSet<String>> {
    let index = crate::store::namespace::store_dir(root)?.join("index.sqlite");
    if !index.is_file() {
        return Ok(HashSet::new());
    }
    let conn = Connection::open_with_flags(&index, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| Error::other(format!("cannot open the document index read-only: {e}")))?;
    let mut stmt = conn.prepare(
        "SELECT c.path, d.relative_path FROM documents d \
         JOIN collections c ON c.name = d.collection \
         WHERE d.status = 'current'",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = HashSet::new();
    for row in rows {
        let (base, rel) = row?;
        if let Some(path) = repo_relative(root, &base, &rel)
            && !is_doc_noise(&path)
        {
            out.insert(path);
        }
    }
    Ok(out)
}

/// `collection path` + `relative path`, as git names it; `None` when the
/// collection lives outside the repository.
fn repo_relative(root: &Path, base: &str, rel: &str) -> Option<String> {
    let base_path = Path::new(base);
    let prefix = if base_path.is_absolute() {
        let stripped = base_path.strip_prefix(root).ok()?;
        stripped.to_string_lossy().into_owned()
    } else {
        base.trim_start_matches("./")
            .trim_start_matches('.')
            .to_string()
    };
    let prefix = prefix.trim_matches('/');
    Some(if prefix.is_empty() {
        rel.to_string()
    } else {
        format!("{prefix}/{rel}")
    })
}

/// Run the document coupling report over the repository at `root`.
pub fn handle_doc_coupling(
    root: &Path,
    overrides: &CouplingOverrides,
) -> Result<DocCouplingReport> {
    let missing = || DocCouplingReport {
        markdown: format!(
            "# Document Coupling\n\nNo code index in {}. Run `mdkb update --code` first.\n",
            root.display()
        ),
        findings: Vec::new(),
        indexed: false,
    };
    let code_path = root.join(".mdkb/code.sqlite");
    if !code_path.exists() {
        return Ok(missing());
    }
    // Same open sequence as `handle_coupling`: the shared live lock keeps a
    // quarantine from renaming the file under this read.
    let _live = crate::store::mutation_lock::acquire_live_shared(&code_path)?;
    let code = Connection::open_with_flags(&code_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| Error::other(format!("cannot open the code index read-only: {e}")))?;
    code.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| Error::other(format!("cannot configure the code index: {e}")))?;
    let sources = indexed_file_set(&code)?;
    if sources.is_empty() {
        return Ok(missing());
    }
    let docs = document_paths(root)?;

    let since = overrides.since.as_deref().unwrap_or(DEFAULT_SINCE);
    let commits = match co_change_history(root, overrides.git_ref.as_deref(), since)? {
        CoChangeHistory::NoHistory => {
            return Ok(DocCouplingReport {
                markdown: "# Document Coupling\n\nNo git history in this repository.\n".into(),
                findings: Vec::new(),
                indexed: true,
            });
        }
        CoChangeHistory::Commits(commits) => commits,
    };
    let kept: Vec<Vec<String>> = commits
        .into_iter()
        .map(|files| {
            files
                .into_iter()
                .filter(|f| sources.contains(f) || docs.contains(f))
                .collect()
        })
        .collect();

    let min = overrides.min_cochanges.unwrap_or(DEFAULT_MIN_COCHANGES);
    let mut findings: Vec<DocPair> = count_cochanges(&kept, MAX_FILES_PER_COMMIT)
        .into_iter()
        .filter(|(_, n)| *n >= min)
        .filter_map(|((a, b), cochanges)| {
            let (doc, source) = match (docs.contains(&a), docs.contains(&b)) {
                (true, false) if sources.contains(&b) => (a, b),
                (false, true) if sources.contains(&a) => (b, a),
                _ => return None,
            };
            Some(DocPair {
                source_commits_after_doc: commits_after(&kept, &source, &doc),
                doc,
                source,
                cochanges,
            })
        })
        .collect();
    findings.sort_by(|x, y| {
        y.cochanges
            .cmp(&x.cochanges)
            .then_with(|| x.doc.cmp(&y.doc))
            .then_with(|| x.source.cmp(&y.source))
    });
    Ok(DocCouplingReport {
        markdown: render(&findings),
        findings,
        indexed: true,
    })
}

/// Commits touching `source` before (newer than) the newest commit touching
/// `doc`. `commits` is newest first.
fn commits_after(commits: &[Vec<String>], source: &str, doc: &str) -> usize {
    commits
        .iter()
        .take_while(|files| !files.iter().any(|f| f == doc))
        .filter(|files| files.iter().any(|f| f == source))
        .count()
}

fn render(pairs: &[DocPair]) -> String {
    let mut out = String::from("# Document Coupling\n\n");
    if pairs.is_empty() {
        out.push_str("No document changes together with a source file often enough to report.\n");
        return out;
    }
    let drifted = pairs
        .iter()
        .filter(|p| p.source_commits_after_doc > 0)
        .count();
    out.push_str(&format!(
        "{} pair(s), {drifted} drifted (the source changed after the document last did).\n\n",
        pairs.len()
    ));
    for p in pairs {
        let drift = if p.source_commits_after_doc > 0 {
            format!(
                "   drifted (source +{} commits)",
                p.source_commits_after_doc
            )
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{}  <->  {}   {} co-changes{drift}\n",
            p.doc, p.source, p.cochanges
        ));
    }
    out
}

/// The findings as JSON.
pub fn render_json(pairs: &[DocPair]) -> String {
    let findings: Vec<serde_json::Value> = pairs
        .iter()
        .map(|p| {
            serde_json::json!({
                "doc": p.doc,
                "source": p.source,
                "cochanges": p.cochanges,
                "source_commits_after_doc": p.source_commits_after_doc,
            })
        })
        .collect();
    let value = serde_json::json!({ "pairs": pairs.len(), "findings": findings });
    format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
}

/// The findings as CSV, one row per pair.
pub fn render_csv(pairs: &[DocPair]) -> String {
    let mut out = String::from("doc,source,cochanges,source_commits_after_doc\n");
    for p in pairs {
        out.push_str(&format!(
            "{},{},{},{}\n",
            crate::code::duplication::report::csv_field(&p.doc),
            crate::code::duplication::report::csv_field(&p.source),
            p.cochanges,
            p.source_commits_after_doc,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a release-notes or work-tracking file reported as a document
    /// that drifts with every source file.
    #[test]
    fn noise_files_are_not_documents() {
        for noise in [
            "CHANGES.md",
            "CHANGELOG.md",
            "llms.txt",
            "todo.md",
            "stories/001-x.md",
            "plans/p.md",
            "sub/plans/p.md",
            ".mdkb/memory/entries/x.md",
        ] {
            assert!(is_doc_noise(noise), "{noise}");
        }
        for doc in ["README.md", "docs/hooks.md", "docs/plans-overview.md"] {
            assert!(!is_doc_noise(doc), "{doc}");
        }
    }

    /// Catches a drift count that also counts source commits older than the
    /// document's last change.
    #[test]
    fn drift_counts_only_source_commits_newer_than_the_doc() {
        let commits = vec![
            vec!["src/a.rs".to_string()],
            vec!["src/a.rs".to_string()],
            vec!["docs/a.md".to_string(), "src/a.rs".to_string()],
            vec!["src/a.rs".to_string()],
        ];
        assert_eq!(commits_after(&commits, "src/a.rs", "docs/a.md"), 2);
    }
}
