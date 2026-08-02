//! Reproduction of the corruption loop observed in production between
//! 2026-07-17 and 2026-07-31.
//!
//! `~/.mdkb/logs/daemon.log` recorded `failed PRAGMA quick_check after mutation`
//! for the same four stores 17153 times across 13 days. The corruption itself
//! predated the 3.7.7/3.7.8 lock fixes, but nothing ever recovered from it: the
//! daemon holds a `Context` (and with it the `.live.lock` that stops autoheal
//! renaming the file) for the life of the repo handle, so every reopen reported
//! `CorruptInUse` — the daemon was the very holder blocking its own heal. Every
//! memory entry written into those repos in the meantime was lost (673 in
//! tuicommander).
//!
//! These tests pin both halves: the loop condition itself, and the release that
//! breaks it.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use mdkb::cli::handlers::{Context, handle_init, handle_update, run_mutation};
use mdkb::code::indexing::{IndexFacade, run_code_mutation};
use mdkb::store::memory::{self, EntryStatus, EntryType, MemoryEntry, SourceType};
use tempfile::TempDir;

/// Page size the index is created with; corruption is written page-aligned.
const PAGE: u64 = 4096;

/// How many pages at the END of the file to scribble. The tail belongs to the
/// bulk table seeded last, so the damage is real b-tree corruption while the
/// early `memory_entries` pages stay readable — which is what made production
/// salvage 165 of itview's entries rather than none.
const CORRUPT_PAGE_COUNT: u64 = 40;

struct Repo {
    _dir: TempDir,
    root: PathBuf,
}

impl Repo {
    /// A store with a doc collection, one memory entry, and enough pages that a
    /// freshly opened connection cannot have the whole file in its page cache.
    fn seed() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        handle_init(&root).expect("init");

        std::fs::write(root.join("note.md"), "# Note\n\nbody\n").expect("write doc");

        let ctx = Context::open(&root).expect("open");
        memory::add_entry(&ctx.conn, &entry("keeper", "survives the heal")).expect("add memory");

        // Grow the file well past the ~2 MB default page cache, so the pages we
        // corrupt below are read from disk rather than served from memory.
        ctx.conn
            .execute_batch("CREATE TABLE bloat (id INTEGER PRIMARY KEY, blob BLOB)")
            .expect("bloat table");
        let payload = vec![b'x'; 3000];
        for i in 0..3000 {
            ctx.conn
                .execute(
                    "INSERT INTO bloat (id, blob) VALUES (?1, ?2)",
                    (i, &payload),
                )
                .expect("bloat insert");
        }
        // Fold the WAL into the main file so the scribble below lands on the
        // pages the next connection will actually read.
        ctx.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .expect("checkpoint");
        drop(ctx);

        Self { _dir: dir, root }
    }

    fn db_path(&self) -> PathBuf {
        self.root.join(".mdkb/index.sqlite")
    }

    /// Overwrite a band of pages with garbage — a torn b-tree, the same class of
    /// damage `quick_check` reported in production.
    fn corrupt(&self) {
        let len = std::fs::metadata(self.db_path())
            .expect("db metadata")
            .len();
        let span = CORRUPT_PAGE_COUNT * PAGE;
        assert!(len > span * 2, "seeded db is too small to tear its tail");
        let start = (len - span) / PAGE * PAGE;

        let mut file = OpenOptions::new()
            .write(true)
            .open(self.db_path())
            .expect("open db for corruption");
        file.seek(SeekFrom::Start(start)).expect("seek");
        file.write_all(&vec![0xA5_u8; span as usize])
            .expect("scribble");
        file.sync_all().expect("sync");
    }

    fn quarantine_files(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.root.join(".mdkb"))
            .expect("read .mdkb")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.contains(".corrupt-"))
            })
            .collect()
    }
}

fn entry(id: &str, content: &str) -> MemoryEntry {
    MemoryEntry {
        id: id.to_string(),
        title: id.to_string(),
        content: content.to_string(),
        entry_type: EntryType::Topic,
        tags: Vec::new(),
        status: EntryStatus::Active,
        created_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        superseded_by: None,
        access_count: 0,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    }
}

/// Drive one index-wide mutation the way the daemon does, through the slot that
/// owns the long-lived connection.
fn mutate(slot: &mut Option<Context>, root: &Path) -> Option<mdkb::error::Result<()>> {
    let root = root.to_path_buf();
    run_mutation(slot, "doc update", |ctx| {
        handle_update(ctx, &root).map(|_| ())
    })
}

#[test]
fn a_mutation_on_a_corrupt_index_reports_it_as_corruption() {
    let repo = Repo::seed();
    let mut slot = Some(Context::open(&repo.root).expect("open"));
    repo.corrupt();

    let err = mutate(&mut slot, &repo.root)
        .expect("slot was populated")
        .expect_err("a mutation on a torn index must not report success");

    assert!(
        err.is_index_corrupt(),
        "corruption must be typed so holders can react to it, got: {err}"
    );
}

#[test]
fn a_read_open_refuses_to_join_a_known_corrupt_generation() {
    // The condition that made the incident last 13 days: while the connection
    // stays open, autoheal refuses to rename (CorruptInUse) — so a daemon that
    // keeps its handle can never recover, no matter how often it retries.
    let repo = Repo::seed();
    // `held` stands in for the handle the daemon keeps alive for days.
    let held = Context::open(&repo.root).expect("open holder");
    let mut slot = Some(Context::open(&repo.root).expect("open"));
    repo.corrupt();

    // The mutation detects the corruption and invalidates the integrity marker,
    // so the reopen below really does probe rather than trust the throttle.
    let _ = mutate(&mut slot, &repo.root);

    // A read command still goes through `Context::open`, which initializes
    // schemas and whose searches update access counters. It must fail before
    // opening a second connection: joining the malformed generation would both
    // write to it and extend the live-lock veto indefinitely.
    let err = Context::open(&repo.root)
        .expect_err("a known-corrupt generation must never be opened for reads");
    assert!(err.is_index_corrupt(), "corruption must stay typed: {err}");
    assert!(
        err.to_string().contains("still in use"),
        "the error must explain why recovery was deferred: {err}"
    );
    assert!(
        repo.quarantine_files().is_empty(),
        "no quarantine file may appear while the index is held open"
    );
    drop(held);
}

#[test]
fn a_failed_read_open_does_not_become_a_new_recovery_blocker() {
    let repo = Repo::seed();
    let last_holder = Context::open(&repo.root).expect("open last holder");
    let mut slot = Some(Context::open(&repo.root).expect("open detecting holder"));
    repo.corrupt();
    let _ = mutate(&mut slot, &repo.root);
    assert!(slot.is_none(), "the detecting holder must close");

    Context::open(&repo.root).expect_err("the read open must be rejected");
    drop(last_holder);

    let healed = Context::open(&repo.root)
        .expect("the rejected read must not block heal after the final holder closes");
    assert!(healed.rebuilt_from_corruption);
}

#[test]
fn a_corrupt_index_is_released_by_the_mutation_that_detects_it() {
    let repo = Repo::seed();
    let mut slot = Some(Context::open(&repo.root).expect("open"));
    repo.corrupt();

    let _ = mutate(&mut slot, &repo.root);

    assert!(
        slot.is_none(),
        "the holder must close a corrupt index instead of retrying against it"
    );
}

#[test]
fn releasing_the_handle_lets_the_next_open_quarantine_salvage_and_rebuild() {
    let repo = Repo::seed();
    let mut slot = Some(Context::open(&repo.root).expect("open"));
    repo.corrupt();

    let _ = mutate(&mut slot, &repo.root);
    assert!(slot.is_none(), "precondition: the handle was released");

    let healed = Context::open(&repo.root).expect("reopen after release");
    assert!(
        healed.rebuilt_from_corruption,
        "the release must let the next open quarantine and rebuild"
    );
    assert_eq!(
        repo.quarantine_files()
            .iter()
            .filter(|p| p.extension().is_none_or(|e| e != "json"))
            .count(),
        1,
        "exactly one quarantined database"
    );

    let salvaged =
        memory::get_entry_without_tracking(&healed.conn, "keeper").expect("query salvaged memory");
    assert!(
        salvaged.is_some(),
        "memory lives only in this database — the heal must salvage it, \
         which is exactly what 13 days of retrying did not do"
    );
}

// ---------------------------------------------------------------------------
// The same trap on the code index
// ---------------------------------------------------------------------------

/// A repo with enough Rust source to make `code.sqlite` span many pages.
fn seed_code_repo() -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_path_buf();
    std::fs::create_dir_all(root.join(".mdkb")).expect("mkdir");
    let src = root.join("src");
    std::fs::create_dir_all(&src).expect("mkdir src");
    for file in 0..40 {
        let mut body = String::new();
        for func in 0..40 {
            body.push_str(&format!(
                "pub fn f_{file}_{func}(a: u32) -> u32 {{ a + {func} }}\n"
            ));
        }
        std::fs::write(src.join(format!("m{file}.rs")), body).expect("write src");
    }
    (dir, root)
}

/// Tear the middle half of the file.
///
/// The code index has nothing to salvage, so the damage is deliberately broad:
/// SQLite only reports corruption when a statement actually reads a torn page,
/// and a narrow tear can sit unread for a long time — which is exactly why this
/// database is checked at open rather than probed after every mutation.
fn corrupt_middle(path: &Path) {
    let len = std::fs::metadata(path).expect("metadata").len();
    let span = len / 2 / PAGE * PAGE;
    assert!(span > PAGE * 4, "db too small to tear: {len} bytes");
    let start = len / 4 / PAGE * PAGE;
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open for corruption");
    file.seek(SeekFrom::Start(start)).expect("seek");
    file.write_all(&vec![0xA5_u8; span as usize])
        .expect("scribble");
    file.sync_all().expect("sync");
}

#[test]
fn a_corrupt_code_index_is_released_and_rebuilt_instead_of_retried() {
    let (_dir, root) = seed_code_repo();
    let db = root.join(".mdkb/code.sqlite");

    let mut slot = Some(IndexFacade::open_or_create(&db).expect("open code index"));
    let stats = run_code_mutation(&mut slot, "seed", |f| f.index_directory(&root))
        .expect("slot populated")
        .expect("initial index");
    assert!(stats.symbols_indexed > 0, "seed produced no symbols");

    // Fold the WAL into the main file: until it is checkpointed the symbols live
    // in `code.sqlite-wal` and the main file is a bare header, so there is
    // nothing on disk to tear.
    slot.as_ref()
        .expect("slot populated")
        .db()
        .conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");

    corrupt_middle(&db);
    // The probe is throttled by the marker the seed run just wrote; clearing it
    // is what a process restart or a 6-hour-old index would do anyway.
    let _ = std::fs::remove_file(root.join(".mdkb/code.sqlite.integrity-ok"));

    // Force the mutation to read the torn pages: every file is re-checked, and
    // the symbols of anything changed are rewritten.
    std::fs::write(root.join("src/m0.rs"), "pub fn changed() {}\n").expect("touch source");
    let result =
        run_code_mutation(&mut slot, "code update", |f| f.update(&root)).expect("slot populated");

    assert!(
        result.is_err(),
        "a mutation over a torn code index must not report success"
    );
    assert!(
        slot.is_none(),
        "the holder must close a corrupt code index instead of retrying against it"
    );

    // With the handle closed, the open path can finally quarantine and rebuild.
    let mut healed = IndexFacade::open_or_create(&db).expect("reopen code index");
    assert!(
        root.join(".mdkb/quarantine").is_dir(),
        "reopening a released corrupt index must quarantine it"
    );
    let rebuilt = healed.index_directory(&root).expect("rebuild");
    assert!(
        rebuilt.symbols_indexed > 0,
        "the rebuilt index must repopulate from source"
    );
}

/// `mdkb update` reaches the code index through `handle_code_index(root, &[])`,
/// not through the facade directly — so the prune has to be live on that path or
/// the CLI keeps answering with symbols of deleted files (agent2 carried four).
#[test]
fn the_cli_whole_tree_refresh_prunes_files_deleted_from_disk() {
    let (_dir, root) = seed_code_repo();

    let first = mdkb::cli::handlers::handle_code_index(&root, &[]).expect("initial index");
    assert!(first.symbols_indexed > 0, "seed produced no symbols");

    std::fs::remove_file(root.join("src/m0.rs")).expect("delete a source file");

    let after = mdkb::cli::handlers::handle_code_index(&root, &[]).expect("refresh");
    assert_eq!(
        after.files_removed, 1,
        "the CLI refresh must drop the deleted file and say so"
    );

    let db = mdkb::code::indexing::IndexFacade::open_or_create(root.join(".mdkb/code.sqlite"))
        .expect("open index");
    assert!(
        db.find_symbols_by_file("m0.rs", 10).is_empty(),
        "symbols of a deleted file must not survive the refresh"
    );
}
