//! Integration tests for hook-call stats (reserved pseudo-session) and the
//! opt-in, privacy-safe query_events telemetry (story 045).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use mdkb::cli::handlers::{Context, handle_init};
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use mdkb::store::stats::{self, QueryEvent};
use rusqlite::Connection;
use tempfile::TempDir;

fn mdkb_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mdkb"))
}

fn run_hook(dir: &Path, event: &str, stdin_json: &str) {
    let mut child = mdkb_bin()
        .args(["hook", event])
        .current_dir(dir)
        .env("MDKB_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .expect("write stdin");
    child.wait().expect("wait");
}

fn seed_repo() -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    handle_init(tmp.path()).expect("init");
    let ctx = Context::open(tmp.path()).expect("open");
    let now = chrono::Utc::now().timestamp();
    add_entry(
        &ctx.conn,
        &MemoryEntry {
            id: "jwt-rotation".to_string(),
            title: "JWT refresh rotation".to_string(),
            content: "sliding expiry refresh token rotation handles jwt expiration".to_string(),
            entry_type: EntryType::Decision,
            tags: vec!["jwt".to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 5,
            last_accessed: Some(now),
            source_path: None,
            confirmations: 1,
            last_confirmed_at: Some(now),
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        },
    )
    .expect("seed");
    tmp
}

fn open_index(dir: &Path) -> Connection {
    Connection::open(dir.join(".mdkb/index.sqlite")).expect("open index")
}

fn hooks_call_count(conn: &Connection, tool: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM call_log cl
         JOIN sessions s ON s.id = cl.session_id
         WHERE s.agent = 'hooks' AND cl.tool_name = ?1",
        [tool],
        |r| r.get(0),
    )
    .expect("count hook calls")
}

fn query_event_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM query_events", [], |r| r.get(0))
        .expect("count query events")
}

#[test]
fn hook_dispatch_records_call_under_hooks_pseudo_session() {
    let tmp = seed_repo();
    run_hook(tmp.path(), "session-start", "");

    let conn = open_index(tmp.path());
    // Exactly one reserved hooks session exists.
    let sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE agent = 'hooks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sessions, 1, "one reserved hooks pseudo-session");
    assert!(
        hooks_call_count(&conn, "session_start") >= 1,
        "session_start hook must be recorded under the hooks session"
    );

    // A second hook reuses the same pseudo-session (no proliferation).
    run_hook(tmp.path(), "session-start", "");
    let conn = open_index(tmp.path());
    let sessions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE agent = 'hooks'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(sessions, 1, "hooks session is reused, not recreated");
    assert!(hooks_call_count(&conn, "session_start") >= 2);
}

#[test]
fn query_events_off_by_default_records_nothing() {
    let tmp = seed_repo();
    // Default config → telemetry.query_events = false.
    run_hook(
        tmp.path(),
        "user-prompt-submit",
        r#"{"prompt":"how did we handle jwt refresh token rotation expiration"}"#,
    );
    let conn = open_index(tmp.path());
    assert_eq!(
        query_event_count(&conn),
        0,
        "query_events must stay empty without opt-in"
    );
}

#[test]
fn query_events_on_records_hash_but_never_text() {
    let tmp = seed_repo();
    // Opt in via config. Also disable the sigil gate (default on) so the plain
    // prompt below triggers recall — the query_event this test asserts on.
    std::fs::write(
        tmp.path().join(".mdkb/config.toml"),
        "[telemetry]\nquery_events = true\n\n[hooks]\nuser_prompt_submit_require_sigil = false\n",
    )
    .expect("write config");

    run_hook(
        tmp.path(),
        "user-prompt-submit",
        r#"{"prompt":"how did we handle jwt refresh token rotation expiration"}"#,
    );

    let conn = open_index(tmp.path());
    assert!(
        query_event_count(&conn) >= 1,
        "opt-in must record a query_event"
    );
    let (hash, text): (String, String) = conn
        .query_row(
            "SELECT query_hash, query_text FROM query_events ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(!hash.is_empty(), "query_hash must be populated");
    assert_eq!(text, "", "query_text must NEVER be persisted");
}

// ---- store-level guarantees (deterministic, no binary) ----

#[test]
fn record_query_event_never_persists_query_text() {
    let conn = Connection::open_in_memory().unwrap();
    mdkb::store::schema::init_schema(&conn).unwrap();
    mdkb::store::stats::init_stats_schema(&conn).unwrap();
    let ev = QueryEvent {
        query_hash: "abc123".to_string(),
        query_text: "SECRET password hunter2 do not store".to_string(),
        search_type: "recall".to_string(),
        result_count: 3,
        latency_ms: 5,
        top_score: None,
        session_id: None,
    };
    stats::record_query_event(&conn, &ev).unwrap();
    let stored: String = conn
        .query_row("SELECT query_text FROM query_events", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, "", "the caller's text must be dropped, not stored");
}

#[test]
fn find_or_create_agent_session_is_stable() {
    let conn = Connection::open_in_memory().unwrap();
    mdkb::store::schema::init_schema(&conn).unwrap();
    mdkb::store::stats::init_stats_schema(&conn).unwrap();
    let a = stats::find_or_create_agent_session(&conn, "hooks").unwrap();
    let b = stats::find_or_create_agent_session(&conn, "hooks").unwrap();
    assert_eq!(a, b, "same agent → same reserved session");
    // A different agent gets a distinct row.
    let c = stats::find_or_create_agent_session(&conn, "cron").unwrap();
    assert_ne!(a, c);
}
