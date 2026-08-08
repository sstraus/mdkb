//! End-to-end smoke tests for the reminder feature.

use mdkb::cli::handlers::handle_init;
use mdkb::core::Context;
use mdkb::store::memory::{
    EntryStatus, EntryType, MemoryEntry, SourceType, add_entry, delete_entry, get_warmup_index,
};
use tempfile::TempDir;

fn setup() -> (TempDir, Context) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open");
    (tmp, ctx)
}

fn make_reminder(id: &str, due_at: Option<i64>) -> MemoryEntry {
    let now = chrono::Utc::now().timestamp();
    MemoryEntry {
        id: id.to_string(),
        title: format!("Reminder {id}"),
        content: "reminder body".to_string(),
        entry_type: EntryType::Reminder,
        tags: vec![],
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
        due_at,
    }
}

#[test]
fn test_reminder_appears_in_warmup_when_due() {
    let (_tmp, ctx) = setup();
    let now = chrono::Utc::now().timestamp();

    // Create a reminder already overdue
    let due = now - 2;
    add_entry(&ctx.conn, &make_reminder("overdue-rem", Some(due))).unwrap();

    let warmup = get_warmup_index(&ctx.conn, 50).unwrap();
    assert!(
        warmup
            .iter()
            .any(|l| l.starts_with("[reminder:DUE] overdue-rem:")),
        "overdue reminder must appear with [reminder:DUE] prefix, got: {:?}",
        warmup
    );
}

#[test]
fn test_reminder_absent_from_warmup_when_future() {
    let (_tmp, ctx) = setup();
    let now = chrono::Utc::now().timestamp();

    add_entry(&ctx.conn, &make_reminder("future-rem", Some(now + 3600))).unwrap();

    let warmup = get_warmup_index(&ctx.conn, 50).unwrap();
    assert!(
        !warmup.iter().any(|l| l.contains("future-rem")),
        "future reminder must not appear in warmup, got: {:?}",
        warmup
    );
}

#[test]
fn test_reminder_gone_from_warmup_after_delete() {
    let (_tmp, ctx) = setup();
    let now = chrono::Utc::now().timestamp();

    add_entry(&ctx.conn, &make_reminder("del-rem", Some(now - 1))).unwrap();

    let before = get_warmup_index(&ctx.conn, 50).unwrap();
    assert!(
        before.iter().any(|l| l.contains("del-rem")),
        "must be present before delete"
    );

    delete_entry(&ctx.conn, "del-rem").unwrap();

    let after = get_warmup_index(&ctx.conn, 50).unwrap();
    assert!(
        !after.iter().any(|l| l.contains("del-rem")),
        "must be absent after delete, got: {:?}",
        after
    );
}
