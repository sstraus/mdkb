//! Drift between the entry projection and the database must be visible in the
//! standing health check, not only in the output of the run that caused it.
//!
//! Story 015-2dc2: 387 markdown files on the agent2 store had no database row at
//! all — 265 of them unique decision/problem knowledge — and nothing reported it.
//! It was found by accident. `mdkb update` printing a per-run count is not
//! enough: nobody re-reads the output of an update that happened weeks ago. The
//! count has to be somewhere a human or an agent looks *routinely*, which is
//! `mdkb stats` and the session-start banner.
//!
//! Bidirectional sync (story 014-fdf0) removed the *cause* — an orphan file is
//! now imported rather than ignored — but not the need for the report: a file
//! that fails validation is still skipped, still invisible, and still
//! accumulating.

use mdkb::cli::handlers::{handle_init, handle_memory_add, sync_memory_files};
use mdkb::cli::stats_report::collect_report;
use mdkb::core::Context;

fn env() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open");
    (dir, ctx)
}

fn entries_dir(ctx: &Context) -> std::path::PathBuf {
    ctx.memory_dir().join("entries")
}

/// Write a file that will never parse as an entry — the drift class that
/// bidirectional sync deliberately refuses to absorb, and therefore the one that
/// can still accumulate unnoticed.
fn write_unreadable(ctx: &Context, name: &str) {
    let dir = entries_dir(ctx);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.md")),
        "---\n<<<<<<< HEAD\ntitle: mine\n=======\ntitle: theirs\n>>>>>>> branch\n---\n\nbody\n",
    )
    .unwrap();
}

#[test]
fn stats_reports_files_that_could_not_be_read() {
    let (_dir, ctx) = env();
    handle_memory_add(
        &ctx, "healthy", "Healthy", "topic", None, "body", None, None, None, None,
    )
    .expect("add");
    sync_memory_files(&ctx).expect("sync");

    for i in 0..3 {
        write_unreadable(&ctx, &format!("unreadable-{i}"));
    }
    sync_memory_files(&ctx).expect("resync");

    let report = collect_report(&ctx).expect("collect stats");
    assert_eq!(
        report.memory.files_unreadable, 3,
        "stats must report entry files that reconciliation could not read — \
         this is the count that reached 387 unnoticed"
    );
}

/// The healthy store must report zero, or the number is noise and will be
/// ignored the one time it matters.
#[test]
fn stats_reports_no_drift_for_a_healthy_store() {
    let (_dir, ctx) = env();
    handle_memory_add(
        &ctx, "fine", "Fine", "topic", None, "body", None, None, None, None,
    )
    .expect("add");
    sync_memory_files(&ctx).expect("sync");

    let report = collect_report(&ctx).expect("collect stats");
    assert_eq!(report.memory.files_unreadable, 0);
    assert_eq!(report.memory.entries_unprojected, 0);
}

/// An entry with no file on disk is the mirror drift: it exists only in a
/// database that is explicitly not backed up and not shared.
#[test]
fn stats_reports_entries_with_no_file() {
    let (_dir, ctx) = env();
    handle_memory_add(
        &ctx, "db-only", "DB only", "topic", None, "body", None, None, None, None,
    )
    .expect("add");
    sync_memory_files(&ctx).expect("sync");

    std::fs::remove_file(entries_dir(&ctx).join("db-only.md")).expect("remove projection");
    // Deliberately no resync: the point is what a *standing* check sees on a
    // store nobody has run `mdkb update` on since the drift appeared.
    let report = collect_report(&ctx).expect("collect stats");
    assert_eq!(
        report.memory.entries_unprojected, 1,
        "an entry whose file is gone must be visible without running a sync first"
    );
}

/// Above the cap the import still completes — a fresh `git clone` of a large
/// corpus is the primary use case and must not need a flag — but it says so
/// loudly. Silence was the failure in 015-2dc2, not the import.
#[test]
fn a_bulk_import_completes_and_reports_itself() {
    let (_dir, ctx) = env();
    let dir = entries_dir(&ctx);
    std::fs::create_dir_all(&dir).unwrap();

    let count = mdkb::cli::handlers::MEMORY_SYNC_BULK_ARCHIVE_CAP + 5;
    for i in 0..count {
        std::fs::write(
            dir.join(format!("clone-{i:03}.md")),
            format!(
                "---\nid: clone-{i:03}\ntitle: Entry {i}\nentry_type: topic\n\
                 source_type: user_statement\nstatus: active\ntags: []\n\
                 created_at: 1700000000\nupdated_at: 1700000000\n---\n\nBody {i}.\n"
            ),
        )
        .unwrap();
    }

    let summary = sync_memory_files(&ctx).expect("sync");
    assert_eq!(
        summary.imported, count,
        "every entry of a fresh clone must be imported — a blocking cap here \
         would break the primary git-sync use case"
    );
    assert_eq!(
        summary.bulk_import_reported,
        Some(count),
        "an import this large must announce itself, so it is never the silent \
         event that 387 orphan files were"
    );

    let quiet = sync_memory_files(&ctx).expect("resync");
    assert_eq!(quiet.imported, 0);
    assert_eq!(
        quiet.bulk_import_reported, None,
        "a steady-state pass must stay quiet, or the warning becomes noise"
    );
}

/// The banner an agent actually sees. Session start is the one moment something
/// routinely reads mdkb's output, so it is where drift has to appear — but it is
/// a hook on the hot path, so it counts rather than classifies.
#[test]
fn the_session_start_drift_check_is_a_count_not_a_parse() {
    let (_dir, ctx) = env();
    handle_memory_add(
        &ctx, "kept", "Kept", "topic", None, "body", None, None, None, None,
    )
    .expect("add");
    sync_memory_files(&ctx).expect("sync");

    assert_eq!(
        mdkb::cli::handlers::projection_file_and_row_counts(&ctx).unwrap(),
        (1, 1),
        "a store in sync must report equal counts, or the banner is permanent noise"
    );

    // A file with no row: unreadable to the parser, but the count still sees it.
    write_unreadable(&ctx, "junk");
    let (files, rows) = mdkb::cli::handlers::projection_file_and_row_counts(&ctx).unwrap();
    assert_eq!(
        (files, rows),
        (2, 1),
        "the cheap check must notice a file the parser would reject — it is a \
         smoke signal, and refusing to parse is the whole point"
    );
}
