//! Integration tests for `mdkb hook session-start` — warmup injection contract.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use mdkb::cli::handlers::{Context, handle_init};
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use tempfile::TempDir;

fn mdkb_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_mdkb");
    Command::new(bin)
}

fn run_session_start_in(dir: &Path, stdin_json: &str) -> (i32, String) {
    let mut child = mdkb_bin()
        .args(["hook", "session-start"])
        .current_dir(dir)
        .env("MDKB_NO_DAEMON", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mdkb");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

fn setup_project_with_entries() -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");

    let ctx = Context::open(&root).expect("open ctx");
    let now = chrono::Utc::now().timestamp();

    let entry = MemoryEntry {
        id: "auth-flow-decision".to_string(),
        title: "OAuth2 refresh token rotation".to_string(),
        content: "use refresh tokens with sliding expiry".to_string(),
        entry_type: EntryType::Decision,
        tags: vec!["auth".to_string(), "oauth2".to_string()],
        status: EntryStatus::Active,
        created_at: now,
        updated_at: now,
        superseded_by: None,
        access_count: 10,
        last_accessed: Some(now),
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type: SourceType::UserStatement,
        expires_at: None,
        due_at: None,
    };
    add_entry(&ctx.conn, &entry).expect("add entry");

    tmp
}

#[allow(clippy::too_many_arguments)]
fn seed(
    ctx: &Context,
    id: &str,
    ty: EntryType,
    content: &str,
    source_type: SourceType,
    access_count: u64,
    age_days: i64,
) {
    let now = chrono::Utc::now().timestamp();
    let ts = now - age_days * 86_400;
    let entry = MemoryEntry {
        id: id.to_string(),
        title: format!("Title {id}"),
        content: content.to_string(),
        entry_type: ty,
        tags: vec!["t".to_string()],
        status: EntryStatus::Active,
        created_at: ts,
        updated_at: ts,
        superseded_by: None,
        access_count,
        last_accessed: None,
        source_path: None,
        confirmations: 0,
        last_confirmed_at: None,
        source_type,
        expires_at: None,
        due_at: None,
    };
    add_entry(&ctx.conn, &entry).expect("seed");
}

/// Extract the warmup block and return the `- ` bullet lines (excludes header/footer).
fn warmup_lines(stdout: &str) -> Vec<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("valid JSON");
    let ctx_block = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .expect("additionalContext string")
        .to_string();
    ctx_block
        .lines()
        .filter(|l| l.starts_with("- "))
        .map(|l| l.to_string())
        .collect()
}

/// 17 entries including 3 empty auto-handoffs and a low-confidence stale entry:
/// warmup must cap at ≤10 lines, drop the empty older handoffs (keep newest),
/// and exclude the sub-0.25-confidence entry.
#[test]
fn session_start_warmup_economy_caps_and_filters() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().to_path_buf();
    handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open ctx");

    // 3 empty auto-handoffs (near-empty bodies), staggered ages. The two older
    // ones carry HIGH access so that if suppression failed they would rank in —
    // making their absence proof of suppression, not of losing the access race.
    // The newest carries the highest access so it survives ranking (it is a
    // candidate, only suppression could remove it).
    seed(
        &ctx,
        "handoff-old",
        EntryType::Handoff,
        "---\ns: 1\n---\n",
        SourceType::UserStatement,
        90,
        3,
    );
    seed(
        &ctx,
        "handoff-mid",
        EntryType::Handoff,
        "---\ns: 2\n---\n",
        SourceType::UserStatement,
        90,
        2,
    );
    seed(
        &ctx,
        "handoff-new",
        EntryType::Handoff,
        "---\ns: 3\n---\n",
        SourceType::UserStatement,
        100,
        1,
    );
    // Low-confidence: old inference entry → below 0.25 floor.
    seed(
        &ctx,
        "stale-inference",
        EntryType::Topic,
        "x",
        SourceType::Inference,
        0,
        40,
    );
    // 13 fresh high-signal topics (17 total).
    for i in 0..13 {
        seed(
            &ctx,
            &format!("topic-{i:02}"),
            EntryType::Topic,
            "meaningful content body",
            SourceType::UserStatement,
            (20 - i) as u64,
            0,
        );
    }

    let (code, stdout) = run_session_start_in(tmp.path(), "");
    assert_eq!(code, 0);
    let lines = warmup_lines(&stdout);

    assert!(
        lines.len() <= 10,
        "warmup must cap at ≤10 lines, got {}: {lines:#?}",
        lines.len()
    );
    let joined = lines.join("\n");
    assert!(joined.contains("handoff-new"), "newest handoff kept");
    assert!(
        !joined.contains("handoff-old"),
        "empty older handoff suppressed"
    );
    assert!(
        !joined.contains("handoff-mid"),
        "empty older handoff suppressed"
    );
    assert!(
        !joined.contains("stale-inference"),
        "sub-floor entry excluded: {joined}"
    );

    // Every emitted line carries the `[type] id: title` shape (id+type+title).
    for l in &lines {
        assert!(
            l.contains('[') && l.contains(']') && l.contains(':'),
            "line missing id+type+title structure: {l}"
        );
    }

    // Token budget (~300, 4 chars/token) — the whole warmup body stays well
    // under a hard 400-token ceiling.
    let total_chars: usize = lines.iter().map(|l| l.chars().count()).sum();
    assert!(
        total_chars / 4 <= 400,
        "warmup body ~{} tokens exceeds budget",
        total_chars / 4
    );
}

#[test]
fn session_start_emits_warmup_with_stored_entries() {
    let tmp = setup_project_with_entries();
    let (code, stdout) = run_session_start_in(tmp.path(), "");

    assert_eq!(code, 0, "hook must always exit 0");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");

    let ctx_block = parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .expect("hookSpecificOutput.additionalContext must be a string");

    assert!(
        ctx_block.contains("## mdkb memory warmup"),
        "context must contain warmup header, got: {ctx_block}"
    );
    assert!(
        ctx_block.contains("auth-flow-decision"),
        "stored memory id must appear in warmup, got: {ctx_block}"
    );
    assert_eq!(
        parsed
            .get("hookSpecificOutput")
            .and_then(|h| h.get("hookEventName"))
            .and_then(|v| v.as_str()),
        Some("SessionStart"),
        "hookEventName must be SessionStart"
    );
}

#[test]
fn session_start_respects_mdkbignore_hooks_marker() {
    let tmp = setup_project_with_entries();
    fs::write(tmp.path().join(".mdkbignore-hooks"), "").expect("write marker");

    let (code, stdout) = run_session_start_in(tmp.path(), "");

    assert_eq!(code, 0);
    assert!(
        stdout.trim().is_empty(),
        "opt-out marker must suppress all output, got: {stdout}"
    );
}

#[test]
fn session_start_on_uninitialized_project_returns_silence() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (code, stdout) = run_session_start_in(tmp.path(), "");

    assert_eq!(code, 0, "hook must never block, even without .mdkb/");
    assert!(
        stdout.trim().is_empty(),
        "no .mdkb/ means no output, got: {stdout}"
    );
}
