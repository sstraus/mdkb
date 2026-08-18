//! Story 014-fdf0: memory entries are git-tracked markdown, reconciled in both
//! directions.
//!
//! The DB stays the store of record for *local telemetry* (access counts,
//! confirmations); the markdown file is the durable, shareable projection. These
//! tests pin the two properties that make the directory mergeable at all:
//! reading an entry never touches its file, and a genuine two-sided edit resolves
//! deterministically without destroying the loser.

use std::path::{Path, PathBuf};
use std::process::Command;

use mdkb::cli::handlers::{handle_init, handle_memory_add, handle_memory_show, sync_memory_files};
use mdkb::core::Context;
use mdkb::store::memory::{self, EntryStatus, EntryType, MemoryEntry, SourceType};
use tempfile::TempDir;

// ---------------------------------------------------------------- helpers

struct Env {
    _dir: TempDir,
    root: PathBuf,
    ctx: Context,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        handle_init(&root).expect("init");
        let ctx = Context::open(&root).expect("open");
        Self {
            _dir: dir,
            root,
            ctx,
        }
    }

    fn entries_dir(&self) -> PathBuf {
        self.root.join(".mdkb/memory/entries")
    }

    fn file(&self, id: &str) -> PathBuf {
        self.entries_dir().join(format!("{id}.md"))
    }

    fn read_file(&self, id: &str) -> String {
        std::fs::read_to_string(self.file(id)).expect("read entry file")
    }

    fn add(&self, id: &str, title: &str, content: &str) {
        handle_memory_add(
            &self.ctx, id, title, "topic", None, content, None, None, None, None,
        )
        .expect("memory add");
    }

    fn add_db_only(&self, id: &str, content: &str, source_type: SourceType, updated_at: i64) {
        memory::add_entry(&self.ctx.conn, &entry(id, content, source_type, updated_at))
            .expect("add");
    }

    fn get(&self, id: &str) -> MemoryEntry {
        memory::get_entry_without_tracking(&self.ctx.conn, id)
            .expect("query")
            .expect("entry exists")
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

fn entry(id: &str, content: &str, source_type: SourceType, updated_at: i64) -> MemoryEntry {
    MemoryEntry {
        id: id.to_string(),
        title: format!("Title {id}"),
        content: content.to_string(),
        entry_type: EntryType::Topic,
        tags: vec!["t".to_string()],
        status: EntryStatus::Active,
        created_at: updated_at,
        updated_at,
        superseded_by: None,
        access_count: 0,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type,
        expires_at: None,
        due_at: None,
    }
}

/// Frontmatter block of a projected entry file, as a list of `key` names.
fn frontmatter_keys(text: &str) -> Vec<String> {
    text.lines()
        .skip(1) // opening ---
        .take_while(|l| *l != "---")
        .filter_map(|l| l.split_once(':').map(|(k, _)| k.trim().to_string()))
        .collect()
}

/// A filesystem path in the shape git accepts as a local remote: on
/// Windows, `canonicalize` yields a verbatim `\\?\C:\...` path whose
/// prefix git reads as an SSH hostname. Stripping it is a no-op elsewhere.
fn git_remote_path(p: &Path) -> String {
    p.to_str().unwrap().trim_start_matches(r"\\?\").to_string()
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_init(dir: &Path) {
    git(dir, &["init", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.com"]);
    git(dir, &["config", "user.name", "T"]);
}

fn git_commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-m", msg, "--allow-empty"]);
}

// ------------------------------------------------- AC2 / AC3: read isolation

/// AC2: local telemetry must never reach the file. Projecting machine-local
/// counters would produce irrelevant diffs and merge conflicts.
#[test]
fn projected_file_carries_no_local_telemetry() {
    let env = Env::new();
    env.add("durable", "Durable Title", "body text");
    sync_memory_files(&env.ctx).expect("sync");

    let keys = frontmatter_keys(&env.read_file("durable"));
    for local in [
        "access_count",
        "last_accessed",
        "confirmations",
        "last_confirmed_at",
        "corrections",
        "projected_at",
        "projected_hash",
    ] {
        assert!(
            !keys.contains(&local.to_string()),
            "local telemetry `{local}` must not be projected into the file; got {keys:?}"
        );
    }
    for durable in ["id", "title", "entry_type", "source_type", "status", "tags"] {
        assert!(
            keys.contains(&durable.to_string()),
            "durable field `{durable}` must be projected; got {keys:?}"
        );
    }
}

/// AC3: a read leaves the file byte-identical — including across the
/// reconciliation pass that follows it, which is where a telemetry-carrying
/// projection would actually rewrite the bytes.
#[test]
fn cli_reads_leave_database_telemetry_and_projection_unchanged() {
    let env = Env::new();
    env.add("read-me", "Read Me", "body text");
    sync_memory_files(&env.ctx).expect("initial sync");
    let before = std::fs::read(env.file("read-me")).expect("read projection");
    let telemetry_before = {
        let entry = env.get("read-me");
        (entry.access_count, entry.last_accessed)
    };

    for _ in 0..3 {
        handle_memory_show(&env.ctx, "read-me")
            .expect("show")
            .expect("entry exists");
    }

    assert_eq!(
        std::fs::read(env.file("read-me")).expect("read projection after CLI reads"),
        before,
        "three reads must leave the entry file byte-identical"
    );
    let after_reads = env.get("read-me");
    assert_eq!(
        (after_reads.access_count, after_reads.last_accessed),
        telemetry_before,
        "read-only CLI handlers must not update access telemetry"
    );

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(
        std::fs::read(env.file("read-me")).expect("read projection after reconciliation"),
        before,
        "reconciliation after reads must not rewrite the file"
    );
    assert_eq!(
        s.projected, 0,
        "nothing changed durably, nothing to project"
    );
}

/// The sharper form: when a durable field *does* change, the resulting diff must
/// contain only durable lines. Reads that happened in between must not smuggle
/// themselves into the commit.
#[test]
fn repeated_cli_reads_do_not_affect_a_later_durable_projection_diff() {
    let env = Env::new();
    env.add("diffed", "First Title", "body text");
    sync_memory_files(&env.ctx).expect("initial sync");
    let before = env.read_file("diffed");
    let before_bytes = std::fs::read(env.file("diffed")).expect("read projection");
    let telemetry_before = {
        let entry = env.get("diffed");
        (entry.access_count, entry.last_accessed)
    };

    for _ in 0..5 {
        handle_memory_show(&env.ctx, "diffed").expect("show");
    }
    assert_eq!(
        std::fs::read(env.file("diffed")).expect("read projection after CLI reads"),
        before_bytes,
        "repeated reads must leave projection bytes unchanged"
    );
    let after_reads = env.get("diffed");
    assert_eq!(
        (after_reads.access_count, after_reads.last_accessed),
        telemetry_before,
        "repeated reads must leave access telemetry unchanged"
    );

    env.add("diffed", "Second Title", "body text"); // durable edit
    sync_memory_files(&env.ctx).expect("resync");

    let after = env.read_file("diffed");
    let changed: Vec<&str> = after
        .lines()
        .filter(|l| !before.lines().any(|b| b == *l))
        .collect();
    for line in &changed {
        let key = line.split_once(':').map(|(k, _)| k.trim()).unwrap_or("");
        assert!(
            matches!(key, "title" | "updated_at"),
            "only durable fields may appear in the diff, found `{line}` (all changed: {changed:?})"
        );
    }
    let after_edit = env.get("diffed");
    assert_eq!(
        (after_edit.access_count, after_edit.last_accessed),
        telemetry_before,
        "the durable edit must not retroactively add read telemetry"
    );
}

// ------------------------------------------------------- AC4: file → DB

/// A colleague's entry arrives via `git pull`: file present, no DB row. Today the
/// sync loop iterates DB rows, so this file is structurally invisible.
#[test]
fn a_new_file_from_a_pull_is_imported() {
    let env = Env::new();
    std::fs::create_dir_all(env.entries_dir()).unwrap();
    std::fs::write(
        env.file("from-colleague"),
        "---\nid: from-colleague\ntitle: Their Entry\nentry_type: topic\n\
         source_type: user_statement\nstatus: active\ntags: [shared]\n\
         created_at: 1700000000\nupdated_at: 1700000500\n---\n\nTheir body.\n",
    )
    .unwrap();

    let s = sync_memory_files(&env.ctx).expect("sync");
    assert_eq!(s.imported, 1, "the pulled file must be imported");

    let e = env.get("from-colleague");
    assert_eq!(e.title, "Their Entry");
    assert_eq!(e.content, "Their body.");
    assert_eq!(e.updated_at, 1_700_000_500, "authored timestamp preserved");
    assert_eq!(e.access_count, 0, "telemetry starts fresh on this machine");

    let s2 = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s2.imported, 0, "import is idempotent");
    assert_eq!(s2.projected, 0);
}

/// A file edited by hand (or arriving edited from a merge) updates the DB.
#[test]
fn an_edited_file_updates_the_db_entry() {
    let env = Env::new();
    env.add("edited", "Edited", "original body");
    sync_memory_files(&env.ctx).expect("initial sync");

    let text = env
        .read_file("edited")
        .replace("original body", "hand-edited body");
    std::fs::write(env.file("edited"), text).unwrap();

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.adopted, 1, "the file edit must reach the DB");
    assert_eq!(env.get("edited").content, "hand-edited body");
    assert_eq!(
        sync_memory_files(&env.ctx).expect("third").adopted,
        0,
        "adoption is idempotent"
    );
}

/// An unresolved merge must never be imported as content.
#[test]
fn a_file_with_conflict_markers_is_quarantined_not_imported() {
    let env = Env::new();
    env.add("merged", "Merged", "clean body");
    sync_memory_files(&env.ctx).expect("initial sync");

    std::fs::write(
        env.file("merged"),
        "---\nid: merged\ntitle: Merged\nentry_type: topic\nstatus: active\ntags: []\n\
         created_at: 1700000000\nupdated_at: 1700000900\n---\n\n\
         <<<<<<< HEAD\nmine\n=======\ntheirs\n>>>>>>> other\n",
    )
    .unwrap();

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.quarantined, 1, "the conflicted file must be skipped");
    assert_eq!(s.adopted, 0);
    assert_eq!(
        env.get("merged").content,
        "clean body",
        "the DB must not absorb both sides of an unresolved merge"
    );
}

// --------------------------------------------------------- AC5: conflicts

/// Both sides changed since the last projection. The newer `updated_at` wins and
/// the loser is preserved verbatim — as a full markdown snapshot, because a
/// content diff records neither metadata changes nor anything at all for
/// `auto_extracted` entries.
#[test]
fn conflict_newer_updated_at_wins_and_loser_is_preserved() {
    // File is newer → file wins, DB version preserved.
    let env = Env::new();
    env.add_db_only(
        "clash",
        "db body v1",
        SourceType::UserStatement,
        1_700_000_000,
    );
    sync_memory_files(&env.ctx).expect("initial sync");

    // DB side edits (older).
    let mut db_side = env.get("clash");
    db_side.content = "db body v2".to_string();
    memory::update_entry_at(&env.ctx.conn, &db_side, 1_700_001_000).expect("db edit");

    // File side edits (newer).
    let text = env.read_file("clash");
    let text = text
        .replace("db body v1", "file body v2")
        .replace("updated_at: 1700000000", "updated_at: 1700002000");
    std::fs::write(env.file("clash"), text).unwrap();

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.conflicts, 1, "both sides changed → one conflict");
    assert_eq!(
        env.get("clash").content,
        "file body v2",
        "the newer (file) version wins"
    );

    let revs = memory::get_revisions(&env.ctx.conn, "clash").expect("revisions");
    assert!(
        revs.iter().any(|r| r.diff.contains("db body v2")),
        "the losing DB version must be preserved verbatim; got {:?}",
        revs.iter().map(|r| &r.diff).collect::<Vec<_>>()
    );
}

#[test]
fn conflict_db_can_win_and_the_file_loser_is_preserved() {
    let env = Env::new();
    env.add_db_only(
        "clash2",
        "db body v1",
        SourceType::UserStatement,
        1_700_000_000,
    );
    sync_memory_files(&env.ctx).expect("initial sync");

    let mut db_side = env.get("clash2");
    db_side.content = "db body v2".to_string();
    memory::update_entry_at(&env.ctx.conn, &db_side, 1_700_009_000).expect("db edit");

    let text = env
        .read_file("clash2")
        .replace("db body v1", "file body v2")
        .replace("updated_at: 1700000000", "updated_at: 1700002000");
    std::fs::write(env.file("clash2"), text).unwrap();

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.conflicts, 1);
    assert_eq!(env.get("clash2").content, "db body v2", "newer DB wins");
    assert!(
        env.read_file("clash2").contains("db body v2"),
        "the winner must be re-projected so the file agrees"
    );
    let revs = memory::get_revisions(&env.ctx.conn, "clash2").expect("revisions");
    assert!(
        revs.iter().any(|r| r.diff.contains("file body v2")),
        "the losing file version must be preserved"
    );
}

/// `save_revision` stores nothing at all unless `source_type` is
/// `user_statement`/`official_docs`. Most of what the memory pipeline writes is
/// `auto_extracted`, so routing conflict losers through it would destroy them
/// silently. The conflict path must preserve regardless of provenance.
#[test]
fn an_auto_extracted_conflict_loser_is_still_preserved() {
    let env = Env::new();
    env.add_db_only(
        "auto",
        "db body v1",
        SourceType::AutoExtracted,
        1_700_000_000,
    );
    sync_memory_files(&env.ctx).expect("initial sync");

    let mut db_side = env.get("auto");
    db_side.content = "db body v2".to_string();
    memory::update_entry_at(&env.ctx.conn, &db_side, 1_700_001_000).expect("db edit");

    let text = env
        .read_file("auto")
        .replace("db body v1", "file body v2")
        .replace("updated_at: 1700000000", "updated_at: 1700002000");
    std::fs::write(env.file("auto"), text).unwrap();

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.conflicts, 1);
    assert_eq!(env.get("auto").content, "file body v2");
    let revs = memory::get_revisions(&env.ctx.conn, "auto").expect("revisions");
    assert!(
        revs.iter().any(|r| r.diff.contains("db body v2")),
        "an auto_extracted loser must be preserved too; got {} revision(s)",
        revs.len()
    );
}

// -------------------------------------------- AC6: breaker vs git deletions

/// A committed bulk deletion is intent recorded in history — it must propagate
/// even above the cap, or a colleague's `git rm` never reaches anyone.
#[test]
fn committed_bulk_deletions_archive_above_the_cap() {
    let env = Env::new();
    git_init(&env.root);
    let ids: Vec<String> = (0..12).map(|i| format!("gone-{i}")).collect();
    for id in &ids {
        env.add_db_only(id, "body", SourceType::UserStatement, 1_700_000_000);
    }
    sync_memory_files(&env.ctx).expect("initial sync");
    git_commit_all(&env.root, "add entries");

    for id in &ids {
        std::fs::remove_file(env.file(id)).unwrap();
    }
    git_commit_all(&env.root, "colleague removes entries");

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(
        s.archived, 12,
        "deletions recorded in HEAD are intent, not loss"
    );
    assert_eq!(s.archive_skipped, 0);
}

/// The same 12 files missing while HEAD still lists them is a broken worktree,
/// not intent — the DATA-2 breaker must still hold.
#[test]
fn files_missing_but_still_in_head_do_not_archive() {
    let env = Env::new();
    git_init(&env.root);
    let ids: Vec<String> = (0..12).map(|i| format!("lost-{i}")).collect();
    for id in &ids {
        env.add_db_only(id, "body", SourceType::UserStatement, 1_700_000_000);
    }
    sync_memory_files(&env.ctx).expect("initial sync");
    git_commit_all(&env.root, "add entries");

    // Files vanish from the worktree without a commit — `rm -rf`, a bad restore.
    for id in &ids {
        std::fs::remove_file(env.file(id)).unwrap();
    }

    let s = sync_memory_files(&env.ctx).expect("resync");
    assert_eq!(s.archived, 0, "HEAD says these files should exist");
    assert_eq!(s.archive_skipped, 12);
    for id in &ids {
        assert_eq!(env.status_of(id).as_deref(), Some("active"));
    }
}

/// A file returning (branch switch back, restore) revives its archived entry.
#[test]
fn a_returning_file_revives_its_archived_entry() {
    let env = Env::new();
    env.add_db_only("back", "body", SourceType::UserStatement, 1_700_000_000);
    sync_memory_files(&env.ctx).expect("initial sync");
    let saved = env.read_file("back");

    std::fs::remove_file(env.file("back")).unwrap();
    sync_memory_files(&env.ctx).expect("archive pass");
    assert_eq!(env.status_of("back").as_deref(), Some("archived"));

    std::fs::write(env.file("back"), saved).unwrap();
    let s = sync_memory_files(&env.ctx).expect("revive pass");
    assert_eq!(s.revived, 1);
    assert_eq!(env.status_of("back").as_deref(), Some("active"));
}

// ------------------------------------------------------------- migration

/// Every store that exists today has projected files carrying telemetry
/// frontmatter and no recorded hash. The first pass must strip the telemetry
/// (one mechanical commit) — and must NOT read the unknown bytes as a
/// conflict, which would fabricate one for every pre-existing entry.
#[test]
fn a_pre_hash_projection_is_reprojected_not_conflicted() {
    let env = Env::new();
    env.add_db_only(
        "legacy",
        "legacy body",
        SourceType::UserStatement,
        1_700_000_000,
    );
    // A v17-era projection: file written, projected_at set, hash unknown.
    std::fs::write(
        env.file("legacy"),
        "---\nid: legacy\ntitle: Title legacy\nentry_type: topic\nsource_type: user_statement\n\
         status: active\ntags: [t]\ncreated_at: 1700000000\nupdated_at: 1700000000\n\
         access_count: 41\nlast_accessed: 1700000500\nconfirmations: 7\n---\n\nlegacy body\n",
    )
    .unwrap();
    env.ctx
        .conn
        .execute(
            "UPDATE memory_entries SET projected_at = 1700000100, projected_hash = NULL WHERE id = 'legacy'",
            [],
        )
        .unwrap();

    let s = sync_memory_files(&env.ctx).expect("first run of the new code");
    assert_eq!(s.conflicts, 0, "unknown bytes are not evidence of an edit");
    assert_eq!(s.projected, 1, "the legacy file is re-projected once");

    let text = env.read_file("legacy");
    assert!(!text.contains("access_count"), "telemetry stripped: {text}");
    assert!(text.contains("legacy body"), "content preserved: {text}");
    assert_eq!(
        env.get("legacy").content,
        "legacy body",
        "the DB is untouched — this is a projection rewrite, not an import"
    );

    let second = sync_memory_files(&env.ctx).expect("second run");
    assert_eq!(second.projected, 0, "the rewrite happens exactly once");
    assert_eq!(second.conflicts, 0);
}

// ------------------------------------------------------ AC1 / AC8: gitignore

#[test]
fn init_writes_a_store_gitignore_that_tracks_only_entries() {
    let env = Env::new();
    let ignore = env.root.join(".mdkb/.gitignore");
    assert!(ignore.exists(), "`mdkb init` must write .mdkb/.gitignore");

    git_init(&env.root);
    env.add("tracked", "Tracked", "body");
    sync_memory_files(&env.ctx).expect("sync");
    git(&env.root, &["add", "-A"]);

    let tracked = git(&env.root, &["ls-files"]);
    let mut listed: Vec<&str> = tracked
        .lines()
        .filter(|l| l.starts_with(".mdkb/"))
        .collect();
    listed.sort_unstable();
    assert_eq!(
        listed,
        vec![".mdkb/.gitignore", ".mdkb/memory/entries/tracked.md"],
        "only the ignore file and the entry projection may be tracked"
    );
}

/// The store `.gitignore` is inert while a parent rule excludes `.mdkb/`
/// wholesale — git never descends into an excluded directory. Detect and report;
/// never rewrite a file mdkb does not own.
#[test]
fn a_parent_gitignore_shadowing_the_store_is_reported() {
    let env = Env::new();
    git_init(&env.root);
    std::fs::write(env.root.join(".gitignore"), ".mdkb/\n").unwrap();
    env.add("shadowed", "Shadowed", "body");

    let s = sync_memory_files(&env.ctx).expect("sync");
    let warning = s
        .gitignore_shadowed
        .as_deref()
        .expect("shadowing must be detected");
    assert!(
        warning.contains(".gitignore") && warning.contains(".mdkb/"),
        "the warning must name the offending rule: {warning}"
    );
}

#[test]
fn no_shadow_reported_when_only_the_store_gitignore_applies() {
    let env = Env::new();
    git_init(&env.root);
    env.add("fine", "Fine", "body");
    let s = sync_memory_files(&env.ctx).expect("sync");
    assert!(s.gitignore_shadowed.is_none(), "{:?}", s.gitignore_shadowed);
}

// ------------------------------------------------------------- AC7: E2E

/// Two clones of one repo, each adding an entry and editing a shared one, must
/// converge with no manual import step anywhere.
#[test]
fn two_clones_converge_without_a_manual_import() {
    let origin_dir = tempfile::tempdir().expect("tempdir");
    let origin = origin_dir.path().canonicalize().unwrap();

    // --- clone A: the repo of record ---
    handle_init(&origin).expect("init");
    {
        let ctx = Context::open(&origin).expect("open A");
        handle_memory_add(
            &ctx, "shared", "Shared", "topic", None, "v1", None, None, None, None,
        )
        .expect("add shared");
        sync_memory_files(&ctx).expect("sync A");
    }
    git_init(&origin);
    git_commit_all(&origin, "seed");

    // --- clone B ---
    let b_dir = tempfile::tempdir().expect("tempdir");
    let b_parent = b_dir.path().canonicalize().unwrap();
    let origin_for_git = git_remote_path(&origin);
    git(&b_parent, &["clone", &origin_for_git, "b"]);
    let b = b_parent.join("b");
    git(&b, &["config", "user.email", "t@example.com"]);
    git(&b, &["config", "user.name", "T"]);

    // B has the files but an empty DB — reconciliation must import them.
    {
        let ctx = Context::open(&b).expect("open B");
        let s = sync_memory_files(&ctx).expect("sync B");
        assert_eq!(s.imported, 1, "B imports A's entry from the checkout");
        assert_eq!(
            memory::get_entry_without_tracking(&ctx.conn, "shared")
                .unwrap()
                .unwrap()
                .content,
            "v1"
        );

        handle_memory_add(
            &ctx, "only-b", "Only B", "topic", None, "b body", None, None, None, None,
        )
        .expect("add only-b");
        sync_memory_files(&ctx).expect("sync B 2");
    }
    git_commit_all(&b, "B adds an entry");

    // --- A adds its own, then pulls B ---
    {
        let ctx = Context::open(&origin).expect("open A2");
        handle_memory_add(
            &ctx, "only-a", "Only A", "topic", None, "a body", None, None, None, None,
        )
        .expect("add only-a");
        sync_memory_files(&ctx).expect("sync A2");
    }
    git_commit_all(&origin, "A adds an entry");
    git(
        &origin,
        &["pull", &git_remote_path(&b), "main", "--no-rebase"],
    );

    let a_ids = {
        let ctx = Context::open(&origin).expect("open A3");
        let s = sync_memory_files(&ctx).expect("sync A3");
        assert_eq!(s.imported, 1, "A imports B's entry with no manual import");
        active_ids(&ctx)
    };

    // --- B pulls back ---
    git(
        &b,
        &["pull", &git_remote_path(&origin), "main", "--no-rebase"],
    );
    let b_ids = {
        let ctx = Context::open(&b).expect("open B3");
        sync_memory_files(&ctx).expect("sync B3");
        active_ids(&ctx)
    };

    assert_eq!(
        a_ids,
        vec![
            "only-a".to_string(),
            "only-b".to_string(),
            "shared".to_string()
        ],
        "A must hold every entry"
    );
    assert_eq!(a_ids, b_ids, "both clones converge on the same entry set");
}

fn active_ids(ctx: &Context) -> Vec<String> {
    let mut stmt = ctx
        .conn
        .prepare("SELECT id FROM memory_entries WHERE status = 'active' ORDER BY id")
        .unwrap();
    let ids: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    ids
}
