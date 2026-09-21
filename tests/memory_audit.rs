//! What `mdkb memory audit` is allowed to do to the store it reads.
//!
//! The feature exists because nothing ever asked whether a stored decision is
//! still true, and because a re-read could not tell a fresh judgement from an
//! ancient one. The dangerous version of that fix is a model sweeping the store
//! and stamping "still valid" on entries it has no way to check — story 092
//! ruled that out already, under the name "a prior does not gain confidence
//! from silence".
//!
//! So these tests are mostly about what the audit must NOT do: not confirm, not
//! refute, not supersede, not write a revision, and not move the decay clock.

use mdkb::cli::handlers::{handle_init, handle_memory_add};
use mdkb::core::Context;
use mdkb::core::memory_audit::{AuditSignal, handle_memory_audit};
use mdkb::store::{memory, memory_audit};

/// A store with one entry that cites a file which does not exist and never
/// did, plus one expired entry. Returns the project root.
fn store_with(entries: &[(&str, &str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    {
        let ctx = Context::open(&root).expect("open");
        for (id, entry_type, content) in entries {
            handle_memory_add(
                &ctx,
                id,
                id,
                entry_type,
                None,
                content,
                None,
                None,
                None,
                None,
                &[],
                None,
                None,
                false,
            )
            .expect("add");
        }
    }
    (dir, root)
}

/// The headline guarantee: an audit that finds nothing to change leaves no
/// trace in the edit history of the entries it read.
///
/// A revision per entry per audit would make the history unreadable within a
/// month, and every one of those revisions would say "a machine looked at this
/// and had no opinion".
#[test]
fn an_unchanged_audit_writes_no_revision() {
    let (_dir, root) = store_with(&[("kept", "decision", "a decision with no code reference")]);
    let ctx = Context::open(&root).expect("open");

    let before = memory::get_revisions(&ctx.conn, "kept")
        .expect("revisions")
        .len();
    handle_memory_audit(&ctx, false).expect("audit");
    handle_memory_audit(&ctx, false).expect("second audit");
    let after = memory::get_revisions(&ctx.conn, "kept")
        .expect("revisions")
        .len();

    assert_eq!(
        before, after,
        "two audits that changed nothing must add no revisions"
    );
}

/// The stamp records a look. Confidence must not read it.
#[test]
fn the_audit_stamp_does_not_move_the_decay_clock() {
    let (_dir, root) = store_with(&[("aged", "handoff", "a lifecycle record")]);
    let ctx = Context::open(&root).expect("open");

    let entry = memory::get_entry_without_tracking(&ctx.conn, "aged")
        .expect("get")
        .expect("entry");
    let at = entry.created_at + 400 * 86_400;
    let before = entry.confidence_at(at);

    handle_memory_audit(&ctx, false).expect("audit");

    let after_entry = memory::get_entry_without_tracking(&ctx.conn, "aged")
        .expect("get")
        .expect("entry");
    assert!(
        (after_entry.confidence_at(at) - before).abs() < f64::EPSILON,
        "an audit that verified nothing must not refresh confidence"
    );
    assert_eq!(
        after_entry.last_confirmed_at, entry.last_confirmed_at,
        "the decay reference is off limits"
    );
    assert_eq!(
        after_entry.last_refuted_at, entry.last_refuted_at,
        "and so is the refutation stamp"
    );
    assert!(
        memory_audit::last_audited_at(&ctx.conn, "aged")
            .expect("stamp")
            .is_some(),
        "but the look itself is recorded"
    );
}

/// `--dry-run` reads and reports without recording anything at all.
#[test]
fn a_dry_run_records_no_audit() {
    let (_dir, root) = store_with(&[("kept", "topic", "nothing to see")]);
    let ctx = Context::open(&root).expect("open");

    handle_memory_audit(&ctx, true).expect("dry-run audit");

    assert_eq!(
        memory_audit::last_audited_at(&ctx.conn, "kept").expect("stamp"),
        None,
        "a dry run that stamps is not a dry run"
    );
}

/// A candidate carries the stamp of the PREVIOUS audit, which is the whole
/// point of storing it: a second pass must be able to say "listed before, and
/// left alone".
#[test]
fn a_second_audit_reports_when_the_first_one_looked() {
    let (_dir, root) = store_with(&[("expired-one", "reminder", "a reminder")]);
    let ctx = Context::open(&root).expect("open");
    // Retire it by TTL so it is selected on both passes.
    ctx.conn
        .execute(
            "UPDATE memory_entries SET expires_at = 1 WHERE id = 'expired-one'",
            [],
        )
        .expect("expire");

    let first = handle_memory_audit(&ctx, false).expect("first audit");
    assert_eq!(first.candidates.len(), 1, "the expired entry is selected");
    assert_eq!(
        first.candidates[0].previously_audited_at, None,
        "nothing had looked at it before"
    );
    assert!(
        first.candidates[0]
            .signals
            .iter()
            .any(|s| matches!(s, AuditSignal::Expired { .. })),
        "and the reason is its expiry: {:?}",
        first.candidates[0].signals
    );

    let second = handle_memory_audit(&ctx, false).expect("second audit");
    assert_eq!(
        second.candidates[0].previously_audited_at,
        Some(first.audited_at),
        "the second pass must be able to say when the first one looked"
    );
}

/// The audit never decides. Whatever it selects, the belief counters and the
/// entry status are exactly as they were.
#[test]
fn the_audit_writes_no_verdict_about_anything_it_selects() {
    let (_dir, root) = store_with(&[("doomed", "reminder", "a reminder nobody reads")]);
    let ctx = Context::open(&root).expect("open");
    ctx.conn
        .execute(
            "UPDATE memory_entries SET expires_at = 1 WHERE id = 'doomed'",
            [],
        )
        .expect("expire");

    let before = memory::get_entry_without_tracking(&ctx.conn, "doomed")
        .expect("get")
        .expect("entry");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");
    assert!(!outcome.candidates.is_empty(), "fixture must select it");

    let after = memory::get_entry_without_tracking(&ctx.conn, "doomed")
        .expect("get")
        .expect("entry");
    assert_eq!(after.status, before.status, "not archived, not superseded");
    assert_eq!(after.confirmations, before.confirmations);
    assert_eq!(after.corrections, before.corrections);
    assert_eq!(after.superseded_by, before.superseded_by);
    assert_eq!(
        after.updated_at, before.updated_at,
        "an untouched updated_at is what keeps the markdown projection quiet"
    );
}

/// Run `git <args>` in `root` and fail the test if git does.
fn git(root: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .expect("git")
        .success();
    assert!(ok, "git {args:?} failed");
}

fn git_commit(root: &std::path::Path, message: &str) {
    git(
        root,
        &[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            message,
        ],
    );
}

/// The drift signal: the entry still resolves, but the file it measured has
/// been committed since the measurement, and the measurement is old enough
/// that the difference could matter.
///
/// Two conditions, not one. A commit under a path an entry cited yesterday is
/// ordinary work; `stale_after_days` is what stops the audit flagging the whole
/// store after a normal quarter.
#[test]
fn a_commit_under_a_cited_path_is_reported_only_for_an_old_entry() {
    let (_dir, root) = store_with(&[
        ("old-measurement", "problem", "measured in src/live.rs"),
        ("fresh-measurement", "problem", "also about src/live.rs"),
    ]);

    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/live.rs"), "fn live() {}\n").expect("write");
    git(&root, &["add", "src/live.rs"]);
    git_commit(&root, "add");

    {
        let ctx = Context::open(&root).expect("open");
        // Backdate one entry past the default `stale_after_days` (180). The
        // other keeps today's timestamp, so the same commit must not flag it.
        let long_ago = chrono::Utc::now().timestamp() - 300 * 86_400;
        ctx.conn
            .execute(
                "UPDATE memory_entries SET updated_at = ?1 WHERE id = 'old-measurement'",
                [long_ago],
            )
            .expect("backdate");
    }

    std::fs::write(root.join("src/live.rs"), "fn live() { changed(); }\n").expect("rewrite");
    git(&root, &["add", "src/live.rs"]);
    git_commit(&root, "change");

    let ctx = Context::open(&root).expect("reopen");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    let drifted: Vec<&str> = outcome
        .candidates
        .iter()
        .filter(|c| {
            c.signals
                .iter()
                .any(|s| matches!(s, AuditSignal::SourceChangedSince { .. }))
        })
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(
        drifted,
        vec!["old-measurement"],
        "a commit under a path a FRESH entry cites is ordinary work, not drift; got {:?}",
        outcome.candidates
    );
}

/// The same drift signal, with the store BELOW the git root.
///
/// `git log --name-only` prints paths relative to the repository root, not to
/// the `-C` directory, while every lookup here uses a path relative to the
/// mdkb project root. The two agree only when the store sits at the git root —
/// which is exactly where the test above puts it, so it could never catch
/// this. In a store at `repo/sub`, git answered `sub/src/live.rs` and the
/// audit asked for `src/live.rs`: the map was built, consulted, and never
/// matched, so drift reported nothing and the pass looked clean.
#[test]
fn drift_is_found_when_the_store_sits_below_the_git_root() {
    let outer = tempfile::tempdir().expect("tempdir");
    let repo = outer.path().canonicalize().expect("canonicalize");
    git(&repo, &["init", "-q"]);

    let root = repo.join("sub");
    std::fs::create_dir_all(&root).expect("mkdir sub");
    handle_init(&root).expect("init");
    {
        let ctx = Context::open(&root).expect("open");
        handle_memory_add(
            &ctx,
            "old-measurement",
            "old-measurement",
            "problem",
            None,
            "measured in src/live.rs",
            None,
            None,
            None,
            None,
            &[],
            None,
            None,
            false,
        )
        .expect("add");
    }

    std::fs::create_dir_all(root.join("src")).expect("mkdir src");
    std::fs::write(root.join("src/live.rs"), "fn live() {}\n").expect("write");
    git(&repo, &["add", "-A"]);
    git_commit(&repo, "add");

    {
        let ctx = Context::open(&root).expect("open");
        let long_ago = chrono::Utc::now().timestamp() - 300 * 86_400;
        ctx.conn
            .execute(
                "UPDATE memory_entries SET updated_at = ?1 WHERE id = 'old-measurement'",
                [long_ago],
            )
            .expect("backdate");
    }

    std::fs::write(root.join("src/live.rs"), "fn live() { changed(); }\n").expect("rewrite");
    git(&repo, &["add", "-A"]);
    git_commit(&repo, "change");

    let ctx = Context::open(&root).expect("reopen");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    let drifted: Vec<&str> = outcome
        .candidates
        .iter()
        .filter(|c| {
            c.signals
                .iter()
                .any(|s| matches!(s, AuditSignal::SourceChangedSince { .. }))
        })
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(
        drifted,
        vec!["old-measurement"],
        "a store below the git root must see its own drift; got {:?}",
        outcome.candidates
    );
}

/// The defect story 116 closes: a store with no embeddings for any active
/// entry (no model ever pulled, `mdkb embed` simply never run, or
/// auto-embed-on-add disabled) must say so rather than report the same
/// "nothing found" a real check would. An operator reading a clean report has
/// no other way to tell "checked, nothing there" from "never checked at all".
///
/// Embeddings are cleared explicitly rather than relying on a fresh store
/// having none: `handle_memory_add` auto-embeds by default when a model is
/// available (`auto_embed_memory`), so `store_with` alone is not a reliable
/// way to get an entry with no embedding on a machine that has the model.
#[test]
fn a_store_with_no_embeddings_reports_the_near_duplicate_check_as_skipped() {
    let (_dir, root) = store_with(&[("alone", "topic", "nothing to compare against")]);
    let ctx = Context::open(&root).expect("open");
    ctx.conn
        .execute("DELETE FROM memory_embeddings", [])
        .expect("clear embeddings");

    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    assert!(
        !outcome.near_duplicate_checked,
        "no entry has an embedding, so the pass never ran"
    );
}

/// Two entries the embedding cannot tell apart are a pair, and both ends are
/// told about it — a reader asked to judge one needs the other named.
///
/// The embeddings are fabricated rather than produced by the model: the signal
/// under test is the pairing rule, and an audit must not need a model pulled.
#[test]
fn identical_embeddings_make_both_entries_candidates() {
    let (_dir, root) = store_with(&[
        ("said-once", "topic", "the store is canonicalized on open"),
        (
            "said-twice",
            "topic",
            "on open, the store path is canonical",
        ),
    ]);
    let ctx = Context::open(&root).expect("open");

    // One vector, stored for both entries: cosine 1.0, above any threshold.
    let vector: Vec<f32> = (0..384).map(|i| ((i % 7) as f32) - 3.0).collect();
    for id in ["said-once", "said-twice"] {
        let rowid: i64 = ctx
            .conn
            .query_row(
                "SELECT rowid FROM memory_entries WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .expect("rowid");
        mdkb::store::vectors::store_memory_embedding(&ctx.conn, rowid, &vector, "test")
            .expect("store embedding");
    }

    let outcome = handle_memory_audit(&ctx, false).expect("audit");
    assert!(
        outcome.near_duplicate_checked,
        "embeddings were stored for both entries, so the pass did run"
    );
    let mut paired: Vec<(&str, &str)> = outcome
        .candidates
        .iter()
        .flat_map(|c| {
            c.signals.iter().filter_map(move |s| match s {
                AuditSignal::NearDuplicate { other, .. } => Some((c.id.as_str(), other.as_str())),
                _ => None,
            })
        })
        .collect();
    paired.sort_unstable();
    assert_eq!(
        paired,
        vec![("said-once", "said-twice"), ("said-twice", "said-once")],
        "both ends of a near-duplicate pair must name the other; got {:?}",
        outcome.candidates
    );
}

/// A reference to a file that is really gone is the signal; a reference to a
/// file that never existed is prose that looked like a path.
#[test]
fn a_dead_reference_is_reported_and_prose_is_not() {
    let (_dir, root) = store_with(&[
        ("cites-gone", "topic", "the fix is in src/gone.rs"),
        ("cites-prose", "topic", "we weighed this and/or that.md"),
    ]);

    // Make `src/gone.rs` a path git has heard of, then delete it: exactly the
    // shape of a reference that stopped resolving.
    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/gone.rs"), "fn gone() {}\n").expect("write");
    git(&root, &["add", "src/gone.rs"]);
    git_commit(&root, "add");
    git(&root, &["rm", "-q", "src/gone.rs"]);
    git_commit(&root, "remove");

    let ctx = Context::open(&root).expect("open");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    let dead: Vec<&str> = outcome
        .candidates
        .iter()
        .filter(|c| {
            c.signals
                .iter()
                .any(|s| matches!(s, AuditSignal::DeadCodeReference { .. }))
        })
        .map(|c| c.id.as_str())
        .collect();
    assert_eq!(
        dead,
        vec!["cites-gone"],
        "only the reference git has heard of is a dead reference; got {:?}",
        outcome.candidates
    );
}

/// A store with no git repository at all has nothing to ask git, and a path
/// that is not on disk is prose that happened to look like a path — not a
/// finding. Reporting one here would be the failure mode this story exists to
/// prevent turned inside out: noise instead of a false all-clear.
#[test]
fn a_store_with_no_git_repo_reports_no_dead_reference_for_a_missing_path() {
    let (_dir, root) = store_with(&[(
        "cites-nothing",
        "topic",
        "see src/does_not_exist_anywhere.rs for details",
    )]);
    assert!(!root.join(".git").exists(), "fixture must have no git repo");

    let ctx = Context::open(&root).expect("open");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    let dead: Vec<&str> = outcome
        .candidates
        .iter()
        .filter(|c| {
            c.signals
                .iter()
                .any(|s| matches!(s, AuditSignal::DeadCodeReference { .. }))
        })
        .map(|c| c.id.as_str())
        .collect();
    assert!(
        dead.is_empty(),
        "no git repository means git was never asked, not that it answered \
         'dead': {:?}",
        outcome.candidates
    );
}

/// The defect this story closes, at the audit's own boundary: a git failure
/// must surface as "could not check", never silently collapse into a
/// resolved reference or vanish with no trace. And it must not inflate the
/// "worth re-reading" count either — "I could not check" is not a finding
/// about the entry, so it belongs in `unchecked`, not `candidates`.
#[test]
fn a_git_failure_is_reported_as_could_not_check_not_resolved() {
    let (_dir, root) = store_with(&[("cites-gone", "topic", "the fix is in src/gone.rs")]);

    // A real repo, so `find_git_root` matches — but `HEAD` is corrupt enough
    // that git refuses to run at all, the case a bare bool could not tell
    // apart from "never existed".
    git(&root, &["init", "-q"]);
    std::fs::write(root.join(".git/HEAD"), "garbage\n").expect("corrupt HEAD");

    let ctx = Context::open(&root).expect("open");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    assert!(
        !outcome.candidates.iter().any(|c| c.id == "cites-gone"),
        "a check failure alone must not count toward 'worth re-reading': {:?}",
        outcome.candidates
    );
    let candidate = outcome
        .unchecked
        .iter()
        .find(|c| c.id == "cites-gone")
        .unwrap_or_else(|| {
            panic!(
                "must be reported as unchecked, silence is the bug: {:?}",
                outcome.unchecked
            )
        });
    assert!(
        candidate
            .signals
            .iter()
            .any(|s| matches!(s, AuditSignal::ReferenceCheckFailed { .. })),
        "a git failure must say it could not check, not resolve silently: {:?}",
        candidate.signals
    );
    assert!(
        !candidate
            .signals
            .iter()
            .any(|s| matches!(s, AuditSignal::DeadCodeReference { .. })),
        "a check that failed is not the same claim as a confirmed dead reference: {:?}",
        candidate.signals
    );
}

/// The renderer's dangerous path, pinned at this layer regardless of what any
/// renderer does with it: when git fails on every cited reference,
/// `candidates` is genuinely empty — nothing else in the store gave it a
/// reason to be non-empty — while `unchecked` is not. A caller that only
/// checks `candidates.is_empty()` before printing "nothing selected for
/// re-reading" would otherwise print a false all-clear while nothing had
/// actually been checked: the exact defect this story exists to remove, one
/// layer up.
#[test]
fn candidates_is_empty_but_unchecked_is_not_when_git_fails_on_the_only_reference() {
    let (_dir, root) = store_with(&[("cites-gone", "topic", "the fix is in src/gone.rs")]);

    git(&root, &["init", "-q"]);
    std::fs::write(root.join(".git/HEAD"), "garbage\n").expect("corrupt HEAD");

    let ctx = Context::open(&root).expect("open");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    assert!(
        outcome.candidates.is_empty(),
        "no other signal fired in this fixture, so candidates must be \
         genuinely empty: {:?}",
        outcome.candidates
    );
    assert!(
        !outcome.unchecked.is_empty(),
        "the check failure must still be reported somewhere, or it vanished \
         silently"
    );
}

/// Story 122: `DeadCodeReference`, `SourceChangedSince` and `NearDuplicate`
/// are each proven above through `handle_memory_audit` itself.  `Contradicts`
/// and `AgedLifecycle` were previously proven only at the SQL layer, by unit
/// tests calling `contradicting_pairs` and `expired_or_aged` directly —
/// nothing asserted either one actually reaches `AuditSignal` out of
/// `select()`. A wrong match arm, or an id that does not line up with a row
/// so `by_id.remove` silently drops it, would have passed every existing
/// test in this file.
///
/// A `contradicts` edge between two live entries must surface as
/// `AuditSignal::Contradicts` on BOTH ends, each naming the other — a reader
/// asked to judge one side needs to be told what it disagrees with.
#[test]
fn a_contradicts_edge_between_live_entries_is_reported_out_of_the_audit() {
    let (_dir, root) = store_with(&[
        ("side-a", "decision", "we will use postgres"),
        ("side-b", "decision", "we will use sqlite"),
    ]);
    let ctx = Context::open(&root).expect("open");
    ctx.conn
        .execute(
            "INSERT INTO memory_edges (source_id, target_ref, target_kind, relation, created_at)
             VALUES ('side-a', 'side-b', 'memory', 'contradicts', 1000)",
            [],
        )
        .expect("insert contradicts edge");

    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    let mut paired: Vec<(&str, &str)> = outcome
        .candidates
        .iter()
        .flat_map(|c| {
            c.signals.iter().filter_map(move |s| match s {
                AuditSignal::Contradicts { other } => Some((c.id.as_str(), other.as_str())),
                _ => None,
            })
        })
        .collect();
    paired.sort_unstable();
    assert_eq!(
        paired,
        vec![("side-a", "side-b"), ("side-b", "side-a")],
        "both ends of a contradiction must name the other, out of the audit \
         itself, not just the SQL layer: {:?}",
        outcome.candidates
    );
}

/// A lifecycle entry (reminder, prior, handoff) untouched for longer than
/// `aged_lifecycle_days` must surface as `AuditSignal::AgedLifecycle` out of
/// `handle_memory_audit`, the same way `expired_or_aged` already proves it at
/// the SQL layer alone.
#[test]
fn an_aged_lifecycle_entry_is_reported_out_of_the_audit() {
    let (_dir, root) = store_with(&[("stale-reminder", "reminder", "a reminder nobody reads")]);
    let ctx = Context::open(&root).expect("open");
    // Past the default aged_lifecycle_days (90), with no read since to keep
    // COALESCE(last_accessed, created_at) pinned to the backdated value.
    let long_ago = chrono::Utc::now().timestamp() - 200 * 86_400;
    ctx.conn
        .execute(
            "UPDATE memory_entries SET created_at = ?1, updated_at = ?1 WHERE id = 'stale-reminder'",
            [long_ago],
        )
        .expect("backdate");

    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    let candidate = outcome
        .candidates
        .iter()
        .find(|c| c.id == "stale-reminder")
        .unwrap_or_else(|| {
            panic!(
                "the aged entry must be selected out of the audit itself: {:?}",
                outcome.candidates
            )
        });
    assert!(
        candidate
            .signals
            .iter()
            .any(|s| matches!(s, AuditSignal::AgedLifecycle { .. })),
        "must be reported as aged, not merely selected for some other reason: {:?}",
        candidate.signals
    );
}

/// `PATH` is process-global, so a test that prepends a spy `git` to it must be
/// the only one touching `PATH` while it runs. Mirrors
/// `tests/cli/common.rs::env_lock`, duplicated here because each top-level
/// file under `tests/` is its own integration test binary.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Restores `PATH` on drop, including if the test panics mid-assertion.
struct RestorePath(Option<std::ffi::OsString>);

impl Drop for RestorePath {
    fn drop(&mut self) {
        match self.0.take() {
            Some(v) => unsafe { std::env::set_var("PATH", v) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }
}

/// The claim story 115 makes: the resolution cache is keyed on the path, not
/// on the `path:LINE` reference string, so the same dead path cited from two
/// different entries at two different lines costs one `git` spawn — not two,
/// and not hundreds after a refactor that deletes a handful of files cited
/// all over the store.
///
/// Proven by putting a spy in front of the real `git`: every invocation
/// appends a line to a counter file, then execs the real binary so the audit
/// still gets correct answers from it. If the cache were still keyed on the
/// whole reference string (the bug this story closes), this would count 2.
#[test]
fn a_dead_path_cited_from_two_entries_spawns_git_once() {
    let _lock = env_lock();
    let (_dir, root) = store_with(&[
        ("first", "topic", "see src/gone.rs:5 for the old approach"),
        ("second", "topic", "also see src/gone.rs:9 for the rewrite"),
    ]);

    git(&root, &["init", "-q"]);
    std::fs::create_dir_all(root.join("src")).expect("mkdir");
    std::fs::write(root.join("src/gone.rs"), "fn gone() {}\n").expect("write");
    git(&root, &["add", "src/gone.rs"]);
    git_commit(&root, "add");
    git(&root, &["rm", "-q", "src/gone.rs"]);
    git_commit(&root, "remove");

    let real_git = String::from_utf8(
        std::process::Command::new("which")
            .arg("git")
            .output()
            .expect("which git")
            .stdout,
    )
    .expect("utf8")
    .trim()
    .to_string();
    assert!(!real_git.is_empty(), "no real git on PATH to spy on");

    let spy_dir = tempfile::tempdir().expect("spy dir");
    let log = spy_dir.path().join("calls.log");
    std::fs::write(&log, "").expect("seed log");
    let script = spy_dir.path().join("git");
    // Count only the spawns the audit makes against *this* repo. `PATH` is
    // global to the process and the other tests in this binary spawn `git`
    // too, without taking `env_lock` — a spy that logged unconditionally
    // counted their calls as well, and failed whenever one overlapped. The
    // audit reaches git as `git -C <root> ...` (see `git::path_ever_existed`),
    // so the root argument is what identifies a call as ours.
    let repo = root.display().to_string();
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = -C ] && [ \"$2\" = {repo:?} ]; then echo called >> {log:?}; fi\nexec {real_git:?} \"$@\"\n"
        ),
    )
    .expect("write spy");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    let original_path = std::env::var_os("PATH");
    let _restore = RestorePath(original_path.clone());
    let mut new_path = spy_dir.path().as_os_str().to_os_string();
    if let Some(p) = &original_path {
        new_path.push(":");
        new_path.push(p);
    }
    // SAFETY: serialized by `env_lock` above, and restored by `RestorePath`
    // even if this test panics before reaching the end.
    unsafe { std::env::set_var("PATH", new_path) };

    let ctx = Context::open(&root).expect("open");
    let outcome = handle_memory_audit(&ctx, false).expect("audit");

    let calls = std::fs::read_to_string(&log).expect("read log");
    let call_count = calls.lines().filter(|l| !l.is_empty()).count();
    assert_eq!(
        call_count, 1,
        "one dead path cited at two lines across two entries must cost one \
         git spawn, not one per line: log was {calls:?}"
    );

    let mut dead: Vec<&str> = outcome
        .candidates
        .iter()
        .filter(|c| {
            c.signals
                .iter()
                .any(|s| matches!(s, AuditSignal::DeadCodeReference { .. }))
        })
        .map(|c| c.id.as_str())
        .collect();
    dead.sort_unstable();
    assert_eq!(
        dead,
        vec!["first", "second"],
        "both citations of the dead path must still be reported dead: {:?}",
        outcome.candidates
    );
}
