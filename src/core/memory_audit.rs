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
        Some(since) => crate::git::paths_changed_since(ctx.root(), since).unwrap_or_default(),
        None => HashMap::new(),
    };

    let mut resolution: HashMap<String, bool> = HashMap::new();
    for (id, reference) in &refs {
        let (path, line) = split_line_suffix(reference);
        let resolves = *resolution
            .entry(reference.clone())
            .or_insert_with(|| reference_resolves(ctx.root(), path, line));
        if !resolves {
            by_id
                .entry(id.clone())
                .or_default()
                .push(AuditSignal::DeadCodeReference {
                    reference: reference.clone(),
                });
            continue;
        }
        let Some(row) = rows.iter().find(|r| &r.id == id) else {
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
    for (a, b, similarity) in
        memory_audit::near_duplicate_pairs(&ctx.conn, config.near_duplicate_similarity)?
    {
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

    let candidates = rows
        .iter()
        .filter_map(|row| {
            let signals = by_id.remove(&row.id)?;
            Some(AuditCandidate {
                id: row.id.clone(),
                title: row.title.clone(),
                entry_type: row.entry_type.clone(),
                updated_at: row.updated_at,
                previously_audited_at: row.last_audited_at,
                signals,
            })
        })
        .collect();

    Ok(AuditOutcome {
        scanned: rows.len(),
        audited_at: now,
        candidates,
    })
}

/// Every `(entry id, path reference)` pair the store's entries mention, in
/// entry order, deduplicated per entry.
fn collect_references(rows: &[AuditRow]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for row in rows {
        let mut seen = std::collections::HashSet::new();
        for m in path_ref_regex().find_iter(&row.content) {
            let reference = m.as_str().to_string();
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

/// Whether a cited reference still points at something.
///
/// Three outcomes, and only the middle one is a finding:
/// * the file is there (and long enough, when a line was cited) — resolves;
/// * the file is gone but git has heard of it — a real dead reference;
/// * the file is gone and git has never heard of it — prose shaped like a
///   path. Treated as resolving, because reporting it is noise.
fn reference_resolves(root: &Path, rel_path: &str, line: Option<usize>) -> bool {
    let full = root.join(rel_path);
    if full.is_file() {
        let Some(line) = line else {
            return true;
        };
        let Ok(text) = std::fs::read_to_string(&full) else {
            // Unreadable or not UTF-8 — the file exists, which is all this
            // signal claims to know.
            return true;
        };
        return line >= 1 && line <= text.lines().count();
    }
    !crate::git::path_ever_existed(root, rel_path)
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

        assert!(reference_resolves(dir.path(), "src/a.rs", Some(2)));
        assert!(
            !reference_resolves(dir.path(), "src/a.rs", Some(99)),
            "a citation past EOF points at nothing, even though the file is there"
        );
    }

    #[test]
    fn prose_shaped_like_a_path_is_not_reported_as_a_dead_reference() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            reference_resolves(dir.path(), "and/or.md", None),
            "git never heard of it, so it was never a reference"
        );
    }
}
