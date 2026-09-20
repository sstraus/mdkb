//! `mdkb memory audit` — select stored entries that deserve a fresh look, and
//! decide nothing about them.
//!
//! Nothing in mdkb ever asks whether a stored decision is still true. Decay is
//! time-based and computed at read, `archive_expired` only runs inside
//! `mdkb update`, and confirm/refute is something a human types. A re-read also
//! cannot tell a fresh judgement from an ancient one, because nothing records
//! when an entry was last re-evaluated.
//!
//! The shape that does NOT work is an AI sweep: the model has no ground truth
//! to check an entry against, so it would stamp confident "still valid" on
//! entries nobody verified. That is story 092 under another name — a prior does
//! not gain confidence from silence.
//!
//! So this is a SELECTOR over signals the store already holds:
//!
//! * a path an entry cites that is no longer in the working tree, but that git
//!   has heard of — a reference that stopped resolving, as opposed to prose
//!   that looked like a path;
//! * a commit under a cited path made after the entry was last written, when
//!   the entry is older than [`crate::config::MemoryAuditConfig::stale_after_days`]
//!   — the measurement may predate the code it describes;
//! * two active entries the embedding cannot tell apart, or two joined by a
//!   `contradicts` edge nobody resolved;
//! * expired entries, and lifecycle records past their age.
//!
//! The only write is the `last_audited_at` stamp. No confirmation, no
//! refutation, no supersession, no revision.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::MemoryAuditConfig;
use crate::core::Context;
use crate::error::Result;
use crate::store::memory_audit::{self, AuditRow, LifecycleReason};

/// Compiled once: a repo-relative path reference, optionally with `:LINE`.
///
/// At least one `/` and a dotted extension beginning with a letter, because
/// without both the pattern matches `3.9.0`, `e.g.` and every sentence-ending
/// abbreviation in the store. The trade is deliberate: a bare `heal.rs` in
/// prose is missed, and nothing false is reported.
fn path_ref_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:[\w.\-]+/)+[\w.\-]+\.[A-Za-z][A-Za-z0-9]{0,7}(?::\d+)?").unwrap()
    })
}

/// Why one entry was selected. Several can hold at once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum AuditSignal {
    /// The entry cites a path that is gone from the working tree, or a line
    /// number past the end of a file that is still there.
    DeadCodeReference { reference: String },
    /// Git failed while checking whether a cited path ever existed, so
    /// whether it is dead could not be determined this pass. Must never be
    /// silently read as "resolves".
    ReferenceCheckFailed { reference: String },
    /// A cited path was committed after this entry was last written.
    SourceChangedSince { path: String, changed_at: i64 },
    /// Another active entry the embedding places at `similarity` or closer.
    NearDuplicate { other: String, similarity: f64 },
    /// Another active entry a `contradicts` edge points at, still unresolved.
    Contradicts { other: String },
    /// `expires_at` has passed.
    Expired { expires_at: i64 },
    /// A reminder, prior or handoff nobody has read for the configured age.
    AgedLifecycle { last_touched: i64 },
}

/// One entry the audit is asking a human to look at.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditCandidate {
    pub id: String,
    pub title: String,
    pub entry_type: String,
    /// Last write to the entry.
    pub updated_at: i64,
    /// When the PREVIOUS audit looked at it; `None` = never before. Carried so
    /// a reader can tell a candidate nobody has considered from one that has
    /// been listed and left alone on purpose.
    pub previously_audited_at: Option<i64>,
    pub signals: Vec<AuditSignal>,
}

/// What one audit pass established.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditOutcome {
    /// Active entries examined.
    pub scanned: usize,
    /// When this pass ran — the value written to `last_audited_at`.
    pub audited_at: i64,
    pub candidates: Vec<AuditCandidate>,
    /// Entries selected for no reason other than a reference git failed to
    /// check. Kept out of `candidates` on purpose: "I could not check" is not
    /// a finding about the entry, and folding it into the same list would
    /// tell an operator an entry is worth re-reading for a reason that has
    /// nothing to do with the entry's own quality — a git failure, not a
    /// dead reference, drifted source, duplicate or contradiction. Still
    /// reported, never silently dropped.
    pub unchecked: Vec<AuditCandidate>,
    /// Whether the near-duplicate pass had any embedding to check against.
    /// `false` — no active entry has one: no model available, auto-embed
    /// disabled, or entries written by a path that skips it — must read as
    /// "not checked", not fold into the same silence as "checked, found
    /// nothing": the two only look alike from the outside.
    pub near_duplicate_checked: bool,
}

/// Run an audit: select, then stamp.
///
/// `dry_run` skips the stamp, so `--dry-run` really is read-only rather than
/// read-plus-one-column.
pub fn handle_memory_audit(ctx: &Context, dry_run: bool) -> Result<AuditOutcome> {
    let config = crate::config::Config::load_or_default(&ctx.config_path);
    let now = chrono::Utc::now().timestamp();
    let rows = memory_audit::auditable_entries(&ctx.conn)?;
    let outcome = select(ctx, &rows, &config.memory.audit, now)?;

    if !dry_run {
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        memory_audit::stamp_audited(&ctx.conn, &ids, now)?;
    }
    Ok(outcome)
}

/// The selection, with the clock injected and no write of any kind.
fn select(
    ctx: &Context,
    rows: &[AuditRow],
    config: &MemoryAuditConfig,
    now: i64,
) -> Result<AuditOutcome> {
    // Signals accumulate per entry, so an entry caught by two of them is
    // listed once carrying both rather than twice.
    let mut by_id: BTreeMap<String, Vec<AuditSignal>> = BTreeMap::new();

    let stale_cutoff = now - (i64::from(config.stale_after_days) * 86_400);
    let refs = collect_references(rows);

    // ── Signal 1 and 2: the paths entries cite ───────────────────────────────
    //
    // One git walk for the whole audit, starting at the oldest entry that could
    // possibly be stale. Walking from the epoch would read the entire history
    // to answer a question bounded by the store's own timestamps.
    let oldest_stale = rows
        .iter()
        .filter(|r| r.updated_at <= stale_cutoff)
        .map(|r| r.updated_at)
        .min();
    let changed = match oldest_stale {
        Some(since) => match crate::git::paths_changed_since(ctx.root(), since) {
            Ok(map) => map,
            Err(e) => {
                // Discarding this silently is the other half of the same
                // defect class as a dead reference misread as resolved: every
                // stale entry would lose its SourceChangedSince signal with
                // no trace that git ever failed to answer.
                tracing::warn!(
                    error = %e,
                    "git failed while checking for source drift; SourceChangedSince is skipped this pass"
                );
                HashMap::new()
            }
        },
        None => HashMap::new(),
    };

    let rows_by_id: HashMap<&str, &AuditRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();

    // Keyed on the path alone, not the `path:LINE` reference, so the same
    // dead path cited at ten lines costs one git spawn, not ten.
    let mut path_statuses: HashMap<&str, PathStatus> = HashMap::new();
    for (id, reference) in &refs {
        let (path, line) = split_line_suffix(reference);
        let cached_status = *path_statuses
            .entry(path)
            .or_insert_with(|| path_status(ctx.root(), path));
        let status = resolve_status(ctx.root(), path, cached_status, line);
        match status {
            Resolution::Dead => {
                by_id
                    .entry(id.clone())
                    .or_default()
                    .push(AuditSignal::DeadCodeReference {
                        reference: reference.clone(),
                    });
                continue;
            }
            Resolution::CheckFailed => {
                by_id
                    .entry(id.clone())
                    .or_default()
                    .push(AuditSignal::ReferenceCheckFailed {
                        reference: reference.clone(),
                    });
                continue;
            }
            Resolution::Resolves => {}
        }
        let Some(row) = rows_by_id.get(id.as_str()) else {
            continue;
        };
        if row.updated_at > stale_cutoff {
            continue; // measured recently enough that drift is not news
        }
        if let Some(&changed_at) = changed.get(path)
            && changed_at > row.updated_at
        {
            by_id
                .entry(id.clone())
                .or_default()
                .push(AuditSignal::SourceChangedSince {
                    path: path.to_string(),
                    changed_at,
                });
        }
    }

    // ── Signal 3: pairs the store already relates ────────────────────────────
    // `rows` is already the caller's own rowid->id map — no need to make
    // `near_duplicate_pairs` query it a second time.
    let ids_by_rowid: HashMap<i64, String> = rows.iter().map(|r| (r.rowid, r.id.clone())).collect();
    let (near_duplicate_checked, near_duplicate_pairs) = memory_audit::near_duplicate_pairs(
        &ctx.conn,
        config.near_duplicate_similarity,
        &ids_by_rowid,
    )?;
    for (a, b, similarity) in near_duplicate_pairs {
        by_id
            .entry(a.clone())
            .or_default()
            .push(AuditSignal::NearDuplicate {
                other: b.clone(),
                similarity,
            });
        by_id
            .entry(b)
            .or_default()
            .push(AuditSignal::NearDuplicate {
                other: a,
                similarity,
            });
    }
    for (a, b) in memory_audit::contradicting_pairs(&ctx.conn)? {
        by_id
            .entry(a.clone())
            .or_default()
            .push(AuditSignal::Contradicts { other: b.clone() });
        by_id
            .entry(b)
            .or_default()
            .push(AuditSignal::Contradicts { other: a });
    }

    // ── Signal 4: lifecycle ──────────────────────────────────────────────────
    for (id, reason) in memory_audit::expired_or_aged(&ctx.conn, config.aged_lifecycle_days, now)? {
        let signal = match reason {
            LifecycleReason::Expired { expires_at } => AuditSignal::Expired { expires_at },
            LifecycleReason::Aged { last_touched } => AuditSignal::AgedLifecycle { last_touched },
        };
        by_id.entry(id).or_default().push(signal);
    }

    // A `ReferenceCheckFailed`-only entry did not earn its place by any
    // property of its own content — git simply could not answer. Splitting
    // it into `unchecked` rather than `candidates` is what keeps "N worth
    // re-reading" honest: an entry with a genuine finding alongside a check
    // failure still belongs in `candidates`, signal and all.
    let mut candidates = Vec::new();
    let mut unchecked = Vec::new();
    for row in rows {
        let Some(signals) = by_id.remove(&row.id) else {
            continue;
        };
        let candidate = AuditCandidate {
            id: row.id.clone(),
            title: row.title.clone(),
            entry_type: row.entry_type.clone(),
            updated_at: row.updated_at,
            previously_audited_at: row.last_audited_at,
            signals,
        };
        if candidate
            .signals
            .iter()
            .all(|s| matches!(s, AuditSignal::ReferenceCheckFailed { .. }))
        {
            unchecked.push(candidate);
        } else {
            candidates.push(candidate);
        }
    }

    Ok(AuditOutcome {
        scanned: rows.len(),
        audited_at: now,
        candidates,
        unchecked,
        near_duplicate_checked,
    })
}

/// True if any component of `path` is `..`.
///
/// `path_ref_regex` matches `[\w.\-]+` per segment, which admits `..`
/// unfiltered — `../../../../etc/passwd.conf` reads as a path reference to
/// the pattern alone. The audit is scoped to the project root; a `..`
/// component means the reference is asking to climb out of it, which is an
/// escape, not a citation. Checked component-wise rather than by substring,
/// so a legitimate filename like `..bashrc` is not caught by mistake.
fn has_parent_dir_component(path: &str) -> bool {
    Path::new(path)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Every `(entry id, path reference)` pair the store's entries mention, in
/// entry order, deduplicated per entry.
///
/// A `..` component is dropped here, at extraction — the earliest point —
/// so it never reaches [`reference_resolves`] and never causes a filesystem
/// call outside the project root.
fn collect_references(rows: &[AuditRow]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for row in rows {
        let mut seen = std::collections::HashSet::new();
        for m in path_ref_regex().find_iter(&row.content) {
            let reference = m.as_str().to_string();
            if has_parent_dir_component(&reference) {
                continue;
            }
            if seen.insert(reference.clone()) {
                out.push((row.id.clone(), reference));
            }
        }
    }
    out
}

/// Split `src/store/heal.rs:216` into its path and its line number.
fn split_line_suffix(reference: &str) -> (&str, Option<usize>) {
    match reference.rsplit_once(':') {
        Some((path, line)) => match line.parse::<usize>() {
            Ok(n) => (path, Some(n)),
            Err(_) => (reference, None),
        },
        None => (reference, None),
    }
}

/// What [`reference_resolves`] established about one cited reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolution {
    /// The file is there, or git never heard of a path that is not.
    Resolves,
    /// A real dead reference: the path is gone, but git has recorded it, or
    /// a cited line is past the end of a file that is still there.
    Dead,
    /// Git failed while answering, so this pass could not tell `Resolves`
    /// from `Dead`. Deliberately its own outcome — folding it into either of
    /// the other two is the bug this type exists to close.
    CheckFailed,
}

/// What [`path_status`] established about one cited path, independent of any
/// line number — the part that is worth caching per path, since it is the
/// part that can spawn `git`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStatus {
    /// A `..` component — refused before any filesystem call, defense in
    /// depth against a caller that reaches this function some other way.
    Escaped,
    /// The file is on disk.
    Exists,
    /// The file is gone but git has recorded it: a real dead reference.
    GoneButRecorded,
    /// The file is gone and git has never heard of it: prose shaped like a
    /// path, not a real reference.
    NeverExisted,
    /// Git failed while answering, so this pass could not tell whether the
    /// path ever existed.
    CheckFailed,
}

/// Whether a cited path exists, without regard to any line number.
///
/// This is the expensive half of resolving a reference — the half that may
/// spawn `git` — and the only half that depends on the path alone, which is
/// what makes it safe to cache per path rather than per `path:LINE`
/// reference.
fn path_status(root: &Path, rel_path: &str) -> PathStatus {
    if has_parent_dir_component(rel_path) {
        return PathStatus::Escaped;
    }
    if root.join(rel_path).is_file() {
        return PathStatus::Exists;
    }
    match crate::git::path_ever_existed(root, rel_path) {
        Ok(true) => PathStatus::GoneButRecorded,
        Ok(false) => PathStatus::NeverExisted,
        Err(e) => {
            tracing::warn!(
                path = rel_path,
                error = %e,
                "git failed while checking whether a cited reference ever existed"
            );
            PathStatus::CheckFailed
        }
    }
}

/// Combines a path's [`PathStatus`] with the line a particular reference
/// cited. Cheap and line-dependent, unlike `path_status` — never spawns a
/// process, so there is nothing to gain from caching it.
fn resolve_status(
    root: &Path,
    rel_path: &str,
    status: PathStatus,
    line: Option<usize>,
) -> Resolution {
    match status {
        PathStatus::Escaped | PathStatus::NeverExisted => Resolution::Resolves,
        PathStatus::GoneButRecorded => Resolution::Dead,
        PathStatus::CheckFailed => Resolution::CheckFailed,
        PathStatus::Exists => {
            let Some(line) = line else {
                return Resolution::Resolves;
            };
            let full = root.join(rel_path);
            let Ok(text) = std::fs::read_to_string(&full) else {
                // Unreadable or not UTF-8 — the file exists, which is all
                // this signal claims to know.
                return Resolution::Resolves;
            };
            if line >= 1 && line <= text.lines().count() {
                Resolution::Resolves
            } else {
                Resolution::Dead
            }
        }
    }
}

/// Whether a cited reference still points at something.
///
/// Four outcomes, and only `Dead` is a finding on its own:
/// * the file is there (and long enough, when a line was cited) — resolves;
/// * the file is gone but git has heard of it — a real dead reference;
/// * the file is gone and git has never heard of it — prose shaped like a
///   path. Treated as resolving, because reporting it is noise;
/// * the file is gone and git itself failed to answer — the check could not
///   be completed, which must never be read as "resolves".
///
/// A thin wrapper over [`path_status`] and [`resolve_status`] — the split
/// that lets [`select`] cache the expensive, path-only part across every
/// line a path is cited at. `select` calls the two halves directly for that
/// caching; this whole-reference form only survives for the tests below,
/// which predate the split and still exercise it as one call.
#[cfg(test)]
fn reference_resolves(root: &Path, rel_path: &str, line: Option<usize>) -> Resolution {
    resolve_status(root, rel_path, path_status(root, rel_path), line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_reference_is_recognised_with_and_without_a_line() {
        let re = path_ref_regex();
        let found: Vec<&str> = re
            .find_iter("see src/store/heal.rs:216 and src/git.rs for the rest")
            .map(|m| m.as_str())
            .collect();
        assert_eq!(found, vec!["src/store/heal.rs:216", "src/git.rs"]);
    }

    #[test]
    fn a_version_number_and_an_abbreviation_are_not_paths() {
        let re = path_ref_regex();
        let text = "mdkb 3.9.0 is the release; e.g. the store is fine. See docs/graph.md.";
        let found: Vec<&str> = re.find_iter(text).map(|m| m.as_str()).collect();
        assert_eq!(
            found,
            vec!["docs/graph.md"],
            "a pattern that matches a version string fills every audit with noise"
        );
    }

    /// Confirms the premise story 114 is built on: the regex alone admits a
    /// `..` component, so something downstream must refuse it.
    #[test]
    fn the_path_regex_matches_a_reference_with_parent_dir_components() {
        let re = path_ref_regex();
        let found: Vec<&str> = re
            .find_iter("see ../../../../etc/passwd.conf for details")
            .map(|m| m.as_str())
            .collect();
        assert_eq!(found, vec!["../../../../etc/passwd.conf"]);
    }

    #[test]
    fn a_reference_with_a_parent_dir_component_is_not_extracted() {
        let row = AuditRow {
            rowid: 1,
            id: "leaky".to_string(),
            title: "leaky".to_string(),
            entry_type: "topic".to_string(),
            content: "see ../../../../etc/passwd.conf for details".to_string(),
            updated_at: 0,
            last_audited_at: None,
        };

        let refs = collect_references(std::slice::from_ref(&row));

        assert!(
            refs.is_empty(),
            "a '..' component must never reach reference_resolves: {refs:?}"
        );
    }

    #[test]
    fn a_reference_with_a_parent_dir_component_is_never_stat_ed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        // A real file OUTSIDE the project root, one line long. If the audit
        // ever stat-ed a `..` reference, citing a line number past this
        // file's single line would resolve to `Dead` — proving the check ran
        // against it. It must not: the escape is refused before any stat.
        std::fs::write(dir.path().join("secret.txt"), "one line\n").unwrap();

        let status = reference_resolves(&root, "../secret.txt", Some(99));

        assert_eq!(
            status,
            Resolution::Resolves,
            "a '..' component must be refused before any filesystem call, \
             never evaluated against a path outside the project root"
        );
    }

    #[test]
    fn the_line_suffix_is_split_off_only_when_it_is_a_number() {
        assert_eq!(
            split_line_suffix("src/a.rs:12"),
            ("src/a.rs", Some(12)),
            "a cited line is part of the reference"
        );
        assert_eq!(split_line_suffix("src/a.rs"), ("src/a.rs", None));
    }

    #[test]
    fn a_line_past_the_end_of_a_real_file_does_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.rs"), "one\ntwo\n").unwrap();

        assert_eq!(
            reference_resolves(dir.path(), "src/a.rs", Some(2)),
            Resolution::Resolves
        );
        assert_eq!(
            reference_resolves(dir.path(), "src/a.rs", Some(99)),
            Resolution::Dead,
            "a citation past EOF points at nothing, even though the file is there"
        );
    }

    #[test]
    fn prose_shaped_like_a_path_is_not_reported_as_a_dead_reference() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            reference_resolves(dir.path(), "and/or.md", None),
            Resolution::Resolves,
            "git never heard of it, so it was never a reference"
        );
    }

    /// The defect this story closes: a git failure must never be read as
    /// "resolves" just because it is also not a dead reference.
    #[test]
    fn a_git_failure_is_reported_as_check_failed_not_resolved() {
        let dir = tempfile::tempdir().unwrap();
        // `.git` exists (so this is not the "no repository" case), but `HEAD`
        // is corrupt enough that git refuses to run at all.
        std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["init", "-q"])
            .output()
            .unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "garbage\n").unwrap();

        assert_eq!(
            reference_resolves(dir.path(), "src/gone.rs", None),
            Resolution::CheckFailed
        );
    }
}
