//! Integration tests for memory file/DB reconciliation (story 048): DB is the
//! source of truth; `mdkb update` projects every entry to a markdown file and a
//! manually deleted (previously-projected) file archives the entry.

use std::path::PathBuf;

use mdkb::cli::handlers::{handle_init, handle_update, sync_memory_files};
use mdkb::core::Context;
use mdkb::store::memory::{self, EntryStatus, EntryType, MemoryEntry, SourceType};
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    root: PathBuf,
    ctx: Context,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        handle_init(&root).expect("init");
        let ctx = Context::open(&root).expect("open");
        Self {
            _dir: dir,
            root,
            ctx,
        }
    }

    /// Insert a DB-only entry (bypasses the disk-write of handle_memory_add).
    fn add_db_only(&self, id: &str) {
        let now = chrono::Utc::now().timestamp();
        memory::add_entry(
            &self.ctx.conn,
            &MemoryEntry {
                id: id.to_string(),
                title: format!("Title {id}"),
                content: format!("Body for {id}"),
                entry_type: EntryType::Topic,
                tags: vec!["t".to_string()],
                status: EntryStatus::Active,
                created_at: now,
                updated_at: now,
                superseded_by: None,
                access_count: 0,
                last_accessed: None,
                source_path: None,
                confirmations: 0,
                last_confirmed_at: None,
                source_type: SourceType::UserStatement,
                expires_at: None,
                due_at: None,
            },
        )
        .expect("add");
    }

    fn entries_dir(&self) -> PathBuf {
        self.root.join(".mdkb/memory/entries")
    }

    fn file_count(&self) -> usize {
        std::fs::read_dir(self.entries_dir())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
                    .count()
            })
            .unwrap_or(0)
    }

    fn active_count(&self) -> i64 {
        self.ctx
            .conn
            .query_row(
                "SELECT COUNT(*) FROM memory_entries WHERE status = 'active'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn status_of(&self, id: &str) -> Option<String> {
        self.ctx
            .conn
            .query_row(
                "SELECT status FROM memory_entries WHERE id = ?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .ok()
    }
}

#[test]
fn sync_backfills_db_only_entries_and_matches_counts() {
    let env = Env::new();
    for id in ["db-1", "db-2", "db-3"] {
        env.add_db_only(id);
    }
    assert_eq!(env.file_count(), 0, "DB-only entries start with no files");

    let s = sync_memory_files(&env.ctx).expect("sync");
    assert_eq!(s.projected, 3, "all three DB-only entries projected");
    assert_eq!(s.archived, 0);

    assert_eq!(
        env.file_count() as i64,
        env.active_count(),
        "file count equals active DB count after sync"
    );
    for id in ["db-1", "db-2", "db-3"] {
        assert!(env.entries_dir().join(format!("{id}.md")).exists());
    }
}

#[test]
fn sync_is_idempotent() {
    let env = Env::new();
    env.add_db_only("a");
    let first = sync_memory_files(&env.ctx).expect("sync 1");
    assert_eq!(first.projected, 1);
    let second = sync_memory_files(&env.ctx).expect("sync 2");
    assert_eq!(
        second.projected, 0,
        "already-projected entry not re-projected"
    );
    assert_eq!(second.archived, 0);
}

#[test]
fn deleting_a_projected_file_archives_the_entry() {
    let env = Env::new();
    env.add_db_only("gone");
    env.add_db_only("kept");
    sync_memory_files(&env.ctx).expect("initial sync");
    assert_eq!(env.status_of("gone").as_deref(), Some("active"));

    // Human deletes one projected file.
    std::fs::remove_file(env.entries_dir().join("gone.md")).unwrap();

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.archived, 1, "the deleted-file entry is archived");
    assert_eq!(env.status_of("gone").as_deref(), Some("archived"));
    assert_eq!(
        env.status_of("kept").as_deref(),
        Some("active"),
        "untouched entry stays active"
    );
}

#[test]
fn never_projected_entry_is_backfilled_not_archived() {
    // The critical distinction: a DB-only entry that never had a file must be
    // BACKFILLED, never archived (an archive would silently drop the audit's 5
    // DB-only entries).
    let env = Env::new();
    env.add_db_only("legacy");
    // No file, projected_at NULL → must create the file, keep active.
    let s = sync_memory_files(&env.ctx).expect("sync");
    assert_eq!(s.projected, 1);
    assert_eq!(s.archived, 0);
    assert_eq!(env.status_of("legacy").as_deref(), Some("active"));
    assert!(env.entries_dir().join("legacy.md").exists());
}

#[test]
fn bulk_file_loss_above_cap_is_not_archived() {
    // DATA-2 guard: if a large set of projected files vanishes at once (git
    // checkout/clean, backup restore), that is directory loss, not intent — the
    // whole corpus must NOT be silently archived. Cap is 10, so 11 missing trips it.
    let env = Env::new();
    let ids: Vec<String> = (0..11).map(|i| format!("bulk-{i}")).collect();
    for id in &ids {
        env.add_db_only(id);
    }
    sync_memory_files(&env.ctx).expect("initial sync");
    assert_eq!(env.file_count(), 11, "all projected to files");

    // Simulate the whole entries dir vanishing.
    for id in &ids {
        std::fs::remove_file(env.entries_dir().join(format!("{id}.md"))).unwrap();
    }

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.archived, 0, "bulk loss must not archive");
    assert_eq!(s.archive_skipped, 11, "all archival candidates skipped");
    for id in &ids {
        assert_eq!(
            env.status_of(id).as_deref(),
            Some("active"),
            "{id} must stay active after suspected bulk loss"
        );
    }
}

#[test]
fn deletions_at_cap_still_archive() {
    // The boundary: exactly the cap (10) deletions is still treated as deliberate
    // — the guard trips only ABOVE the cap, so it never blocks normal per-entry pruning.
    let env = Env::new();
    let ids: Vec<String> = (0..10).map(|i| format!("del-{i}")).collect();
    for id in &ids {
        env.add_db_only(id);
    }
    sync_memory_files(&env.ctx).expect("initial sync");

    for id in &ids {
        std::fs::remove_file(env.entries_dir().join(format!("{id}.md"))).unwrap();
    }

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.archived, 10, "at-cap deletions still archive");
    assert_eq!(s.archive_skipped, 0);
    for id in &ids {
        assert_eq!(env.status_of(id).as_deref(), Some("archived"));
    }
}

#[test]
fn update_runs_sync_and_reports_counts() {
    let env = Env::new();
    env.add_db_only("via-update");
    let r = handle_update(&env.ctx, &env.root).expect("update");
    assert_eq!(r.memory_files_projected, 1);
    assert!(env.entries_dir().join("via-update.md").exists());
    assert_eq!(env.file_count() as i64, env.active_count());
}
