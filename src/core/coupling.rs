//! `mdkb coupling`: pairs of files that git says must change together, but
//! the code graph has never recorded an edge between.
//!
//! The static graph answers "what does this call?"; git answers "what broke
//! last time this changed?" — and the two diverge exactly where the debt no
//! static analysis can see: an implicit contract, a config duplicated in two
//! places, a test that knows the implementation, two hand-synced copies of one
//! rule. Reported here as the file pairs that co-change often enough to be a
//! pattern, and that no confidently resolved `Calls` edge connects.
//!
//! `Calls` is the only edge kind that suppresses a pair, and only at tiers 1–2:
//! [`resolved_edges`] filters on `r.kind = 'Calls'` and
//! [`connected_file_pairs`] keeps `nearest <= MAX_SUPPRESSING_TIER`. A pair
//! joined solely by `Uses`, `Implements`, `Expands` or `Defines` is therefore
//! still reported, even when that edge resolves at tier 1. Widening the kind
//! set is out of scope here: no co-change pair connected by one of those kinds
//! has been measured as a false positive, and each kind carries a different
//! claim about the two files — `Implements` between a trait and its impl says
//! nothing about whether the two must change together.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, params};

use crate::code::duplication::candidates::MAX_SUPPRESSING_TIER;
use crate::code::storage::resolved_edges;
use crate::error::{Error, Result};
use crate::git::{CoChangeHistory, co_change_history};

/// Fewest co-changing commits before a pair is worth reporting.
///
/// Below this, two files sharing a handful of commits is coincidence — a
/// docs pass or a dependency bump that happened to touch both — not a
/// pattern. Matches the order of magnitude `mdkb dup`'s own defaults use for
/// "this is not noise".
pub const DEFAULT_MIN_COCHANGES: usize = 5;

/// How far back the co-change window reaches.
///
/// "Coupling from 2019 says nothing about today's code" — the proposal this
/// command implements names the time window as the one parameter that must be
/// calibrated. Twelve months is long enough to catch a pattern that repeats a
/// few times a year (release branches, quarterly config changes) and short
/// enough that a file pair which hasn't co-changed since a rewrite stops being
/// reported once the rewrite is a year old.
pub const DEFAULT_SINCE: &str = "12 months ago";

/// Commits touching more files than this are dropped from the co-change count
/// entirely, not truncated.
///
/// A mass rename or a repo-wide reformat touching hundreds of files is not
/// evidence that any two of them are coupled — it is one event that happened
/// to touch both. Counting every pair inside such a commit is also O(n^2)
/// work for that false signal: a single 500-file commit would contribute
/// ~124,750 pairs. 100 is comfortably above an ordinary multi-file feature
/// commit and comfortably below a repo-wide sweep.
const MAX_FILES_PER_COMMIT: usize = 100;

/// What the caller overrode on the command line. `None` keeps the default.
#[derive(Debug, Default, Clone)]
pub struct CouplingOverrides {
    pub min_cochanges: Option<usize>,
    pub since: Option<String>,
    pub git_ref: Option<String>,
}

/// The audit, ready to print.
#[derive(Debug)]
pub struct CouplingReport {
    pub markdown: String,
    /// The ranked findings behind `markdown`.
    ///
    /// Carried rather than counted so a caller that wants JSON or CSV renders
    /// the same pairs the prose was rendered from.
    pub findings: Vec<CoupledPair>,
    /// False when there is no code index. The caller reports that and stops;
    /// it is not an error, it is a repository nobody has indexed yet.
    pub indexed: bool,
}

/// One hidden-coupling finding: two files git says change together, that the
/// code graph has no edge between.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoupledPair {
    pub file_a: String,
    pub file_b: String,
    pub cochanges: usize,
}

impl CouplingReport {
    /// How many pairs the report names.
    pub fn pairs(&self) -> usize {
        self.findings.len()
    }
}

/// Run the hidden-coupling audit over the repository at `root`.
pub fn handle_coupling(root: &Path, overrides: &CouplingOverrides) -> Result<CouplingReport> {
    let code_path = root.join(".mdkb/code.sqlite");
    let missing = || CouplingReport {
        markdown: format!(
            "# Hidden Coupling\n\nNo code index in {}. Run `mdkb code index` first.\n",
            root.display()
        ),
        findings: Vec::new(),
        indexed: false,
    };
    if !code_path.exists() {
        return Ok(missing());
    }

    // Announce the connection before opening it, matching `mdkb dup`: autoheal
    // quarantines by renaming the path while surviving connections still
    // derive -wal/-shm from it, and the shared live lock is what makes
    // quarantine wait instead.
    let _live = crate::store::mutation_lock::acquire_live_shared(&code_path)?;

    let code = Connection::open_with_flags(&code_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| Error::other(format!("cannot open the code index read-only: {e}")))?;
    code.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| Error::other(format!("cannot configure the code index: {e}")))?;

    let symbols: i64 = code
        .query_row("SELECT COUNT(*) FROM code_symbols", [], |r| r.get(0))
        .unwrap_or(0);
    if symbols == 0 {
        return Ok(missing());
    }

    let min_cochanges = overrides.min_cochanges.unwrap_or(DEFAULT_MIN_COCHANGES);
    let since = overrides.since.as_deref().unwrap_or(DEFAULT_SINCE);

    let commits = match co_change_history(root, overrides.git_ref.as_deref(), since)? {
        CoChangeHistory::NoHistory => {
            return Ok(CouplingReport {
                markdown: "# Hidden Coupling\n\nNo git history in this repository.\n".to_string(),
                findings: Vec::new(),
                indexed: true,
            });
        }
        CoChangeHistory::Commits(commits) => commits,
    };

    // A file the code index never saw — a lockfile, a changelog, a build
    // manifest — can carry no graph edge by construction: it has no symbols
    // to connect. Pairing it with the source file it co-changes with would
    // report "hidden coupling" for every commit that touches code and
    // describes it, which is not a finding, it is how commits work. Measured
    // on mdkb's own history: without this filter, 51 of 61 pairs were exactly
    // this shape (`Cargo.lock` <-> `Cargo.toml`, `CHANGES.md` <-> a source
    // file, …) rather than a real candidate.
    let indexed_files = indexed_file_set(&code)?;
    let commits: Vec<Vec<String>> = commits
        .into_iter()
        .map(|files| {
            files
                .into_iter()
                .filter(|f| indexed_files.contains(f))
                .collect()
        })
        .collect();

    let counts = count_cochanges(&commits, MAX_FILES_PER_COMMIT);
    let connected = connected_file_pairs(&code)?;

    let mut hidden: Vec<CoupledPair> = counts
        .into_iter()
        .filter(|(_, count)| *count >= min_cochanges)
        .filter(|(pair, _)| !connected.contains(pair))
        .map(|((file_a, file_b), cochanges)| CoupledPair {
            file_a,
            file_b,
            cochanges,
        })
        .collect();

    rank(&mut hidden);

    Ok(CouplingReport {
        markdown: render(&hidden),
        findings: hidden,
        indexed: true,
    })
}

/// Count, for every unordered pair of files, how many commits touched both.
///
/// A commit's own file list is deduplicated and sorted first: git can list a
/// path twice inside one `--name-only` diff (e.g. a rename recorded as a
/// delete and an add of the same path under `--no-renames`), and an
/// unordered pair must not be counted twice for the same commit.
fn count_cochanges(
    commits: &[Vec<String>],
    max_files_per_commit: usize,
) -> HashMap<(String, String), usize> {
    let mut counts = HashMap::new();
    for files in commits {
        if files.len() < 2 || files.len() > max_files_per_commit {
            continue;
        }
        let mut sorted = files.clone();
        sorted.sort_unstable();
        sorted.dedup();
        for i in 0..sorted.len() {
            for j in (i + 1)..sorted.len() {
                *counts
                    .entry((sorted[i].clone(), sorted[j].clone()))
                    .or_insert(0) += 1;
            }
        }
    }
    counts
}

/// Every unordered file pair the code graph already connects, in either
/// direction.
///
/// Uses the same `Calls` candidate cascade as the duplication pass. A bare
/// name match is only a hypothesis, so it must not hide a co-change pair: only
/// candidates the cascade placed at tiers 1–2 count as an existing connection.
/// The cascade itself carries the callable-kind filter, which means a field or
/// module sharing a call's name cannot suppress a finding either.
fn connected_file_pairs(code: &Connection) -> Result<HashSet<(String, String)>> {
    let edges = resolved_edges("r.from_symbol_id IS NOT NULL");
    let mut stmt = code.prepare(&format!(
        "SELECT DISTINCT ff.rel_path, tf.rel_path \
         FROM ({edges}) e \
         JOIN code_symbols fs ON fs.id = e.from_id \
         JOIN code_files ff ON fs.file_id = ff.id \
         JOIN code_symbols ts ON ts.id = e.sym_id \
         JOIN code_files tf ON ts.file_id = tf.id \
         WHERE e.tier = e.nearest \
           AND e.nearest <= ?1 \
           AND ff.rel_path <> tf.rel_path"
    ))?;
    let rows = stmt.query_map(params![MAX_SUPPRESSING_TIER], |row| {
        let a: String = row.get(0)?;
        let b: String = row.get(1)?;
        Ok(normalize_pair(a, b))
    })?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row?);
    }
    Ok(set)
}

/// Every path the code index has a row for.
///
/// Git's file lists include everything a commit touched — lockfiles, docs,
/// CI config — but only a path in `code_files` was ever parsed into symbols,
/// so only such a path could ever appear on either side of
/// [`connected_file_pairs`]. A commit's file list is filtered against this
/// set before counting co-changes, so a lockfile paired with the source file
/// it accompanies never becomes a candidate in the first place.
fn indexed_file_set(code: &Connection) -> Result<HashSet<String>> {
    let mut stmt = code.prepare("SELECT rel_path FROM code_files")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut set = HashSet::new();
    for row in rows {
        set.insert(row?);
    }
    Ok(set)
}

/// A pair in a fixed order, so the same two files always hash to the same key
/// regardless of which one git or the graph query happened to name first.
fn normalize_pair(a: String, b: String) -> (String, String) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Order findings worst-first: most co-changes first, then alphabetically for
/// a deterministic report when two pairs tie.
fn rank(pairs: &mut [CoupledPair]) {
    pairs.sort_by(|x, y| {
        y.cochanges
            .cmp(&x.cochanges)
            .then_with(|| x.file_a.cmp(&y.file_a))
            .then_with(|| x.file_b.cmp(&y.file_b))
    });
}

/// The markdown report.
fn render(pairs: &[CoupledPair]) -> String {
    if pairs.is_empty() {
        return "# Hidden Coupling\n\nNo hidden coupling found.\n".to_string();
    }

    let mut out = String::from("# Hidden Coupling\n\n");
    out.push_str(&format!(
        "{} pair{} co-change without a call-graph edge.\n",
        pairs.len(),
        if pairs.len() == 1 { "" } else { "s" },
    ));

    for (n, pair) in pairs.iter().enumerate() {
        out.push_str(&format!(
            "\n## {}. `{}` \u{2194} `{}`\n\n{} co-change{}, no code-graph edge either way.\n",
            n + 1,
            pair.file_a,
            pair.file_b,
            pair.cochanges,
            if pair.cochanges == 1 { "" } else { "s" },
        ));
    }
    out
}

/// The same findings as JSON.
pub fn render_json(pairs: &[CoupledPair]) -> String {
    let findings: Vec<serde_json::Value> = pairs
        .iter()
        .map(|p| {
            serde_json::json!({
                "file_a": p.file_a,
                "file_b": p.file_b,
                "cochanges": p.cochanges,
            })
        })
        .collect();
    let value = serde_json::json!({ "pairs": pairs.len(), "findings": findings });
    format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
}

/// The same findings as CSV, one row per pair.
pub fn render_csv(pairs: &[CoupledPair]) -> String {
    let mut out = String::from("file_a,file_b,cochanges\n");
    for pair in pairs {
        out.push_str(&format!(
            "{},{},{}\n",
            crate::code::duplication::report::csv_field(&pair.file_a),
            crate::code::duplication::report::csv_field(&pair.file_b),
            pair.cochanges,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["init", "-q", "-b", "main"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// One commit touching every file in `rels`, dated `date` so `--since`
    /// windows in these tests never depend on when they happen to run.
    fn commit_touching(root: &Path, rels: &[&str], date: &str) {
        for rel in rels {
            if let Some(parent) = Path::new(rel).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(root.join(parent)).unwrap();
                }
            }
            std::fs::write(root.join(rel), "x").unwrap();
        }
        std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .arg("add")
            .args(rels)
            .output()
            .unwrap();
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit", "-m", "touch files"])
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// `N` separate commits, each touching exactly `rels`, so the co-change
    /// count for that pair is exactly `N`.
    fn cochange_n_times(root: &Path, rels: &[&str], n: usize) {
        for i in 0..n {
            for rel in rels {
                if let Some(parent) = Path::new(rel).parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(root.join(parent)).unwrap();
                    }
                }
                std::fs::write(root.join(rel), format!("v{i}")).unwrap();
            }
            std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .arg("add")
                .args(rels)
                .output()
                .unwrap();
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["commit", "-m", &format!("revision {i}")])
                .env("GIT_AUTHOR_DATE", "2026-06-01T12:00:00")
                .env("GIT_COMMITTER_DATE", "2026-06-01T12:00:00")
                .env("GIT_AUTHOR_NAME", "Test")
                .env("GIT_AUTHOR_EMAIL", "test@example.com")
                .env("GIT_COMMITTER_NAME", "Test")
                .env("GIT_COMMITTER_EMAIL", "test@example.com")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    /// A `.mdkb/code.sqlite` with the real schema and the given files/symbols,
    /// plus an optional `Calls` edge from `caller` to `callee` (by name).
    fn code_index(root: &Path, files: &[&str], edge: Option<(&str, &str, &str, &str)>) {
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        let code = Connection::open(root.join(".mdkb/code.sqlite")).unwrap();
        crate::code::storage::schema::init_schema(&code).unwrap();

        let mut file_ids = std::collections::HashMap::new();
        for (i, file) in files.iter().enumerate() {
            let id = i as i64 + 1;
            code.execute(
                "INSERT INTO code_files (id, path, rel_path, hash, language) \
                 VALUES (?1, ?2, ?2, 'h', 'rust')",
                rusqlite::params![id, file],
            )
            .unwrap();
            file_ids.insert(*file, id);
            // One symbol per file, named after the file, so a caller/callee
            // pair below always has something to attach to.
            code.execute(
                "INSERT INTO code_symbols (id, name, kind, file_id, file_path, visibility, line_start, line_end) \
                 VALUES (?1, ?2, 'Function', ?3, ?4, 0, 0, 3)",
                rusqlite::params![id, format!("sym_{i}"), id, file],
            )
            .unwrap();
        }

        if let Some((caller_file, caller_sym, callee_file, callee_sym)) = edge {
            let caller_file_id = file_ids[caller_file];
            let callee_file_id = file_ids[callee_file];
            code.execute(
                "INSERT INTO code_symbols (id, name, kind, file_id, file_path, visibility, line_start, line_end) \
                 VALUES (100, ?1, 'Function', ?2, ?3, 0, 0, 3)",
                rusqlite::params![caller_sym, caller_file_id, caller_file],
            )
            .unwrap();
            code.execute(
                "INSERT INTO code_symbols (id, name, kind, file_id, file_path, visibility, line_start, line_end, owner_name) \
                 VALUES (101, ?1, 'Function', ?2, ?3, 0, 0, 3, 'Target')",
                rusqlite::params![callee_sym, callee_file_id, callee_file],
            )
            .unwrap();
            code.execute(
                "INSERT INTO code_relationships \
                 (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
                 VALUES (100, ?1, ?2, 'Calls', ?3, 'Target')",
                rusqlite::params![caller_sym, callee_sym, caller_file_id],
            )
            .unwrap();
        }
    }

    #[test]
    fn a_repository_with_no_code_index_is_reported_not_refused() {
        let root = tempfile::tempdir().unwrap();

        let report = handle_coupling(root.path(), &CouplingOverrides::default()).unwrap();

        assert!(!report.indexed);
        assert_eq!(report.pairs(), 0);
        assert!(
            report.markdown.contains("No code index"),
            "{}",
            report.markdown
        );
    }

    /// Requirement 6: a repository with no git history is reported, not
    /// crashed. The code index exists and is populated; git has nothing.
    #[test]
    fn a_repository_with_no_git_history_is_reported_not_crashed() {
        let root = tempfile::tempdir().unwrap();
        code_index(root.path(), &["src/a.rs", "src/b.rs"], None);
        // No `git init` at all here.

        let report = handle_coupling(root.path(), &CouplingOverrides::default()).unwrap();

        assert!(report.indexed);
        assert_eq!(report.pairs(), 0);
        assert!(
            report.markdown.contains("No git history"),
            "{}",
            report.markdown
        );
    }

    /// Requirement 2: co-changing files with no graph edge are reported.
    #[test]
    fn files_that_cochange_with_no_graph_edge_are_reported() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        cochange_n_times(root.path(), &["src/a.rs", "src/b.rs"], 5);
        code_index(root.path(), &["src/a.rs", "src/b.rs"], None);

        let report = handle_coupling(
            root.path(),
            &CouplingOverrides {
                min_cochanges: Some(5),
                since: Some("5 years ago".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.pairs(), 1, "{}", report.markdown);
        assert!(report.markdown.contains("src/a.rs"), "{}", report.markdown);
        assert!(report.markdown.contains("src/b.rs"), "{}", report.markdown);
        assert!(
            report.markdown.contains("5 co-changes"),
            "{}",
            report.markdown
        );
    }

    /// A file the code index never saw — a lockfile, a changelog, a manifest
    /// — can never carry a graph edge by construction: it has no symbols to
    /// connect. Pairing it with a source file it co-changes with would report
    /// "hidden coupling" for every commit that touches code and describes it,
    /// which is not a finding, it is how commits work. Measured on mdkb's own
    /// history: without this filter, 51 of 61 pairs were exactly this shape
    /// (`Cargo.lock` \u{2194} `Cargo.toml`, `CHANGES.md` \u{2194} a source file, …).
    #[test]
    fn a_cochanging_file_never_seen_by_the_code_index_is_not_reported() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        cochange_n_times(root.path(), &["Cargo.toml", "src/a.rs"], 5);
        // Only src/a.rs is indexed — Cargo.toml never is, matching a real repo
        // where the code index only ever sees source files.
        code_index(root.path(), &["src/a.rs"], None);

        let report = handle_coupling(
            root.path(),
            &CouplingOverrides {
                min_cochanges: Some(5),
                since: Some("5 years ago".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.pairs(), 0, "{}", report.markdown);
    }

    /// Requirement 1: co-changing files that DO have a call edge are ordinary
    /// coupling, visible to the graph — not reported here.
    #[test]
    fn files_that_cochange_and_have_a_graph_edge_are_not_reported() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        cochange_n_times(root.path(), &["src/a.rs", "src/b.rs"], 5);
        code_index(
            root.path(),
            &["src/a.rs", "src/b.rs"],
            Some(("src/a.rs", "caller", "src/b.rs", "callee")),
        );

        let report = handle_coupling(
            root.path(),
            &CouplingOverrides {
                min_cochanges: Some(5),
                since: Some("5 years ago".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.pairs(), 0, "{}", report.markdown);
        assert!(
            report.markdown.contains("No hidden coupling"),
            "{}",
            report.markdown
        );
    }

    #[test]
    fn a_bare_name_call_at_the_unplaced_tier_does_not_suppress_coupling() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        cochange_n_times(root.path(), &["src/a.rs", "src/b.rs"], 5);
        code_index(root.path(), &["src/a.rs", "src/b.rs"], None);
        let code = Connection::open(root.path().join(".mdkb/code.sqlite")).unwrap();
        code.execute(
            "INSERT INTO code_symbols \
             (id, name, kind, file_id, file_path, visibility, line_start, line_end) \
             VALUES (100, 'caller', 'Function', 1, 'src/a.rs', 0, 0, 3), \
                    (101, 'target', 'Function', 2, 'src/b.rs', 0, 0, 3)",
            [],
        )
        .unwrap();
        code.execute(
            "INSERT INTO code_relationships (from_symbol_id, from_name, to_name, kind, file_id) \
             VALUES (100, 'caller', 'target', 'Calls', 1)",
            [],
        )
        .unwrap();
        drop(code);

        let report = handle_coupling(
            root.path(),
            &CouplingOverrides {
                min_cochanges: Some(5),
                since: Some("5 years ago".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.pairs(), 1, "a tier-7 name match is not a connection");
    }

    /// Pins the documented kind filter: `Calls` is the only suppressing kind.
    /// The edge below is shaped exactly like the one
    /// `files_that_cochange_and_have_a_graph_edge_are_not_reported` uses — the
    /// qualifier matches the target's `owner_name`, so it resolves at tier 1 —
    /// and differs only in its kind. It still does not hide the pair.
    #[test]
    fn a_pair_connected_only_by_an_implements_edge_is_still_reported() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        cochange_n_times(root.path(), &["src/a.rs", "src/b.rs"], 5);
        code_index(root.path(), &["src/a.rs", "src/b.rs"], None);
        let code = Connection::open(root.path().join(".mdkb/code.sqlite")).unwrap();
        code.execute(
            "INSERT INTO code_symbols \
             (id, name, kind, file_id, file_path, visibility, line_start, line_end, owner_name) \
             VALUES (100, 'Impl', 'Struct', 1, 'src/a.rs', 0, 0, 3, NULL), \
                    (101, 'method', 'Function', 2, 'src/b.rs', 0, 0, 3, 'Target')",
            [],
        )
        .unwrap();
        code.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
             VALUES (100, 'Impl', 'method', 'Implements', 1, 'Target')",
            [],
        )
        .unwrap();
        drop(code);

        let report = handle_coupling(
            root.path(),
            &CouplingOverrides {
                min_cochanges: Some(5),
                since: Some("5 years ago".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            report.pairs(),
            1,
            "only Calls suppresses a pair; Implements does not: {}",
            report.markdown
        );
    }

    #[test]
    fn non_call_or_non_callable_relationships_do_not_suppress_coupling() {
        let root = tempfile::tempdir().unwrap();
        code_index(root.path(), &["src/a.rs", "src/b.rs"], None);
        let code = Connection::open(root.path().join(".mdkb/code.sqlite")).unwrap();
        code.execute(
            "INSERT INTO code_symbols \
             (id, name, kind, file_id, file_path, visibility, line_start, line_end, owner_name) \
             VALUES (100, 'caller', 'Function', 1, 'src/a.rs', 0, 0, 3, NULL), \
                    (101, 'callable', 'Function', 2, 'src/b.rs', 0, 0, 3, 'Target'), \
                    (102, 'field', 'Field', 2, 'src/b.rs', 0, 4, 4, 'Target')",
            [],
        )
        .unwrap();
        code.execute(
            "INSERT INTO code_relationships \
             (from_symbol_id, from_name, to_name, kind, file_id, to_qualifier) \
             VALUES (100, 'caller', 'callable', 'Uses', 1, 'Target'), \
                    (100, 'caller', 'field', 'Calls', 1, 'Target')",
            [],
        )
        .unwrap();

        assert!(
            !connected_file_pairs(&code)
                .unwrap()
                .contains(&("src/a.rs".to_string(), "src/b.rs".to_string())),
            "only confident Calls candidates of callable kinds connect files"
        );
    }

    /// Requirement 3: co-change below the threshold is not reported.
    #[test]
    fn cochange_below_the_threshold_is_not_reported() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        cochange_n_times(root.path(), &["src/a.rs", "src/b.rs"], 4);
        code_index(root.path(), &["src/a.rs", "src/b.rs"], None);

        let report = handle_coupling(
            root.path(),
            &CouplingOverrides {
                min_cochanges: Some(5),
                since: Some("5 years ago".to_string()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(report.pairs(), 0, "{}", report.markdown);
    }

    #[test]
    fn a_bad_ref_is_an_error_not_an_empty_report() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        commit_touching(root.path(), &["src/a.rs"], "2026-06-01T12:00:00");
        code_index(root.path(), &["src/a.rs"], None);

        let result = handle_coupling(
            root.path(),
            &CouplingOverrides {
                git_ref: Some("no-such-branch".to_string()),
                ..Default::default()
            },
        );

        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn a_ref_starting_with_dash_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        init_repo(root.path());
        commit_touching(root.path(), &["src/a.rs"], "2026-06-01T12:00:00");
        code_index(root.path(), &["src/a.rs"], None);

        let result = handle_coupling(
            root.path(),
            &CouplingOverrides {
                git_ref: Some("--upload-pack=evil".to_string()),
                ..Default::default()
            },
        );

        let err = result.unwrap_err();
        assert!(err.to_string().contains("--upload-pack=evil"), "{err}");
    }

    #[test]
    fn count_cochanges_ignores_a_single_file_commit() {
        let counts = count_cochanges(&[vec!["a.rs".to_string()]], MAX_FILES_PER_COMMIT);
        assert!(counts.is_empty());
    }

    #[test]
    fn count_cochanges_drops_a_commit_over_the_file_cap() {
        let huge: Vec<String> = (0..200).map(|i| format!("f{i}.rs")).collect();
        let counts = count_cochanges(&[huge], 100);
        assert!(
            counts.is_empty(),
            "a 200-file commit must not contribute pairs"
        );
    }

    #[test]
    fn count_cochanges_counts_a_normal_commit() {
        let counts = count_cochanges(
            &[vec![
                "a.rs".to_string(),
                "b.rs".to_string(),
                "c.rs".to_string(),
            ]],
            MAX_FILES_PER_COMMIT,
        );
        assert_eq!(counts.len(), 3, "{counts:?}");
        assert_eq!(counts[&("a.rs".to_string(), "b.rs".to_string())], 1);
    }

    #[test]
    fn normalize_pair_is_order_independent() {
        assert_eq!(
            normalize_pair("b.rs".to_string(), "a.rs".to_string()),
            normalize_pair("a.rs".to_string(), "b.rs".to_string()),
        );
    }

    #[test]
    fn rank_orders_most_cochanges_first_then_alphabetically() {
        let mut pairs = vec![
            CoupledPair {
                file_a: "z.rs".to_string(),
                file_b: "y.rs".to_string(),
                cochanges: 5,
            },
            CoupledPair {
                file_a: "a.rs".to_string(),
                file_b: "b.rs".to_string(),
                cochanges: 9,
            },
            CoupledPair {
                file_a: "c.rs".to_string(),
                file_b: "d.rs".to_string(),
                cochanges: 5,
            },
        ];

        rank(&mut pairs);

        assert_eq!(pairs[0].cochanges, 9);
        assert_eq!(pairs[1].file_a, "c.rs", "tie broken alphabetically");
        assert_eq!(pairs[2].file_a, "z.rs");
    }

    #[test]
    fn an_empty_report_says_so() {
        assert!(render(&[]).contains("No hidden coupling found"));
    }
}
