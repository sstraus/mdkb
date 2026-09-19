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
