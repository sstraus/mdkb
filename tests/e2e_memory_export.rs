//! End-to-end tests for `mdkb memory export` and directory-based import.

use std::path::PathBuf;

use mdkb::cli::handlers::{
    Context, handle_init, handle_memory_export, handle_memory_import, handle_memory_import_dir,
};
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

    fn add(&self, entry: MemoryEntry) {
        memory::add_entry(&self.ctx.conn, &entry).expect("add_entry");
    }
}

fn make_entry(id: &str, title: &str, entry_type: EntryType) -> MemoryEntry {
    let now = chrono::Utc::now().timestamp();
    MemoryEntry {
        id: id.to_string(),
        title: title.to_string(),
        content: format!("Content for {id}."),
        entry_type,
        tags: vec!["test".to_string()],
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 5,
        last_accessed: Some(now),
        source_path: None,
        confirmations: 2,
        last_confirmed_at: Some(now),
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    }
}

// ---------------------------------------------------------------------------
// Export tests
// ---------------------------------------------------------------------------

#[test]
fn export_writes_one_file_per_entry() {
    let env = Env::new();
    env.add(make_entry("alpha", "Alpha Entry", EntryType::Topic));
    env.add(make_entry("beta", "Beta Entry", EntryType::Decision));
    env.add(make_entry("gamma", "Gamma Entry", EntryType::Problem));

    let out_dir = env.root.join("out");
    let result = handle_memory_export(
        &env.ctx, &out_dir, /* include_expired */ false, /* overwrite */ false,
        /* dry_run */ false,
    )
    .expect("export");

    assert_eq!(result.exported, 3, "should export 3 entries");
    assert_eq!(result.skipped, 0);
    assert!(result.errors.is_empty());

    for id in &["alpha", "beta", "gamma"] {
        let file = out_dir.join(format!("{id}.md"));
        assert!(file.exists(), "{id}.md should exist");

        let text = std::fs::read_to_string(&file).expect("read file");
        let parsed = mdkb::store::memory_file::from_markdown(&text).expect("parse");
        assert_eq!(parsed.meta.id, *id);
    }
}

#[test]
fn export_dry_run_prints_without_writing() {
    let env = Env::new();
    env.add(make_entry("dry-a", "Dry A", EntryType::Topic));

    let out_dir = env.root.join("dry-out");
    let result = handle_memory_export(&env.ctx, &out_dir, false, false, /* dry_run */ true)
        .expect("export dry_run");

    assert_eq!(result.exported, 1, "dry_run still reports 1");
    assert!(!out_dir.exists(), "dir must not be created in dry_run");
}

#[test]
fn export_overwrite_false_skips_existing() {
    let env = Env::new();
    env.add(make_entry("clash", "Clash", EntryType::Topic));
    let out_dir = env.root.join("out2");
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("clash.md"), "old content").unwrap();

    let result = handle_memory_export(&env.ctx, &out_dir, false, false, false).expect("export");

    assert_eq!(result.skipped, 1);
    assert_eq!(result.exported, 0);
    // old file untouched
    let content = std::fs::read_to_string(out_dir.join("clash.md")).unwrap();
    assert_eq!(content, "old content");
}

#[test]
fn export_overwrite_true_replaces_existing() {
    let env = Env::new();
    env.add(make_entry("rewrite", "Rewrite", EntryType::Topic));
    let out_dir = env.root.join("out3");
    std::fs::create_dir_all(&out_dir).unwrap();
    std::fs::write(out_dir.join("rewrite.md"), "old").unwrap();

    let result = handle_memory_export(&env.ctx, &out_dir, false, /* overwrite */ true, false)
        .expect("export");

    assert_eq!(result.exported, 1);
    let content = std::fs::read_to_string(out_dir.join("rewrite.md")).unwrap();
    assert!(content.contains("---"), "file should have frontmatter");
}

#[test]
fn export_excludes_expired_by_default() {
    let env = Env::new();
    env.add(make_entry("alive", "Alive", EntryType::Topic));

    let mut expired = make_entry("dead", "Dead", EntryType::Topic);
    expired.expires_at = Some(chrono::Utc::now().timestamp() - 1); // already expired

    env.add(expired);

    let out_dir = env.root.join("out4");
    let result = handle_memory_export(&env.ctx, &out_dir, false, false, false).expect("export");

    assert_eq!(result.exported, 1, "expired entry skipped");
    assert_eq!(result.skipped, 1);
}

#[test]
fn export_include_expired_flag_includes_them() {
    let env = Env::new();
    env.add(make_entry("alive2", "Alive2", EntryType::Topic));
    let mut expired = make_entry("dead2", "Dead2", EntryType::Topic);
    expired.expires_at = Some(chrono::Utc::now().timestamp() - 1);
    env.add(expired);

    let out_dir = env.root.join("out5");
    let result = handle_memory_export(
        &env.ctx, &out_dir, /* include_expired */ true, false, false,
    )
    .expect("export");

    assert_eq!(result.exported, 2);
}

// ---------------------------------------------------------------------------
// Round-trip: export → import_dir
// ---------------------------------------------------------------------------

#[test]
fn round_trip_export_import_dir_restores_authored_fields() {
    let env = Env::new();
    let now = chrono::Utc::now().timestamp();
    let orig = MemoryEntry {
        id: "rt-entry".to_string(),
        title: "Round-trip".to_string(),
        content: "Some content here.".to_string(),
        entry_type: EntryType::Decision,
        tags: vec!["a".to_string(), "b".to_string()],
        status: EntryStatus::Active,
        created_at: now - 100,
        updated_at: now,
        superseded_by: None,
        access_count: 7,
        last_accessed: Some(now - 10),
        source_path: Some("docs/foo.md".to_string()),
        confirmations: 3,
        last_confirmed_at: Some(now - 5),
        source_type: SourceType::OfficialDocs,
        expires_at: None,
        due_at: None,
    };
    env.add(orig.clone());

    // Export
    let out_dir = env.root.join("rt-out");
    handle_memory_export(&env.ctx, &out_dir, false, false, false).expect("export");

    // Delete original from DB
    memory::delete_entry(&env.ctx.conn, "rt-entry").expect("delete");

    // Import from directory
    let imp = handle_memory_import_dir(&env.ctx, &out_dir, false, false).expect("import_dir");
    assert_eq!(imp.imported, 1);
    assert_eq!(imp.errors, Vec::<String>::new());

    let restored = memory::get_entry_without_tracking(&env.ctx.conn, "rt-entry")
        .expect("get")
        .expect("entry should exist");

    // Authored fields preserved
    assert_eq!(restored.title, orig.title);
    assert_eq!(restored.content, orig.content);
    assert_eq!(restored.entry_type, orig.entry_type);
    assert_eq!(restored.tags, orig.tags);
    assert_eq!(restored.source_type, orig.source_type);
    assert_eq!(restored.source_path, orig.source_path);
    assert_eq!(restored.created_at, orig.created_at);
    assert_eq!(restored.updated_at, orig.updated_at);
    assert_eq!(restored.expires_at, orig.expires_at);
    assert_eq!(restored.due_at, orig.due_at);

    // Derived counters are reset
    assert_eq!(restored.access_count, 0, "access_count must be reset");
    assert_eq!(restored.last_accessed, None, "last_accessed must be reset");
    assert_eq!(restored.confirmations, 0, "confirmations must be reset");
    assert_eq!(restored.last_confirmed_at, None);
}

#[test]
fn import_dir_skip_duplicates() {
    let env = Env::new();
    env.add(make_entry("dup", "Dup", EntryType::Topic));

    let out_dir = env.root.join("dup-out");
    handle_memory_export(&env.ctx, &out_dir, false, false, false).expect("export");

    // Import without skip — should error
    let imp = handle_memory_import_dir(&env.ctx, &out_dir, false, false).expect("import");
    assert_eq!(
        imp.errors.len(),
        1,
        "duplicate without --skip-duplicates → error"
    );

    // Import with skip
    let imp2 = handle_memory_import_dir(&env.ctx, &out_dir, false, true).expect("import2");
    assert_eq!(imp2.skipped, 1);
    assert!(imp2.errors.is_empty());
}

#[test]
fn import_dir_dry_run_does_not_write() {
    let env = Env::new();
    env.add(make_entry("dry-import", "Dry Import", EntryType::Topic));
    let out_dir = env.root.join("dry-import-out");
    handle_memory_export(&env.ctx, &out_dir, false, false, false).expect("export");
    memory::delete_entry(&env.ctx.conn, "dry-import").expect("delete");

    let imp = handle_memory_import_dir(&env.ctx, &out_dir, /* dry_run */ true, false)
        .expect("import dry");
    assert_eq!(imp.imported, 1, "dry_run reports 1");

    let check = memory::get_entry_without_tracking(&env.ctx.conn, "dry-import").expect("get");
    assert!(check.is_none(), "dry_run must not write to DB");
}

#[test]
fn import_dir_missing_directory_returns_io_error() {
    let env = Env::new();
    let missing = env.root.join("does-not-exist");
    let err = handle_memory_import_dir(&env.ctx, &missing, false, false)
        .expect_err("missing dir must return Err");
    let msg = format!("{err}");
    assert!(
        msg.contains("read_dir"),
        "error should mention read_dir: {msg}"
    );
}

#[test]
fn import_dir_malformed_markdown_recorded_in_errors() {
    let env = Env::new();
    let bad_dir = env.root.join("bad");
    std::fs::create_dir_all(&bad_dir).unwrap();
    // No frontmatter → parse error
    std::fs::write(bad_dir.join("broken.md"), "not valid frontmatter").unwrap();

    let result = handle_memory_import_dir(&env.ctx, &bad_dir, false, false).expect("call ok");
    assert_eq!(result.imported, 0);
    assert_eq!(result.errors.len(), 1, "one parse error expected");
    assert!(
        result.errors[0].contains("broken.md"),
        "error must reference file: {:?}",
        result.errors[0]
    );
}

#[test]
fn import_dir_validation_failure_recorded() {
    let env = Env::new();
    let bad_dir = env.root.join("invalid-id");
    std::fs::create_dir_all(&bad_dir).unwrap();
    // Valid YAML frontmatter but id contains forbidden characters.
    let text = "---\nid: \"bad id with spaces\"\ntitle: \"T\"\nentry_type: topic\ntags: []\nsource_type: user_statement\ncreated_at: 1700000000\nupdated_at: 1700000000\n---\nbody\n";
    std::fs::write(bad_dir.join("invalid.md"), text).unwrap();

    let result = handle_memory_import_dir(&env.ctx, &bad_dir, false, false).expect("call ok");
    assert_eq!(result.imported, 0);
    assert_eq!(result.errors.len(), 1);
}

#[test]
fn json_import_still_works_after_dir_import_added() {
    let env = Env::new();

    let json = r#"{"entries":[{"id":"json-compat","title":"Json Compat","content":"still works","entryType":"topic","sourceType":"user_statement","tags":[]}]}"#;
    let json_path = env.root.join("import.json");
    std::fs::write(&json_path, json).unwrap();

    let result =
        handle_memory_import(&env.ctx, json_path.to_str().unwrap(), false, false).expect("import");
    assert_eq!(result.imported, 1);
}
