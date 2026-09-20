//! A SessionStart that emits nothing must say why.
//!
//! Story 127-fcaf: every early return in `hook_session_start_inner` answered
//! `{}`, the dispatcher turned any `{}` into `outcome=skipped`, and the hook
//! client prints nothing for a skip. So the morning the daemon ran a v27 binary
//! against a v28 store, 21 hook runs produced empty stdout, exit 0 and a log
//! row identical to the one written when hooks are switched off on purpose.
//!
//! These run the real binary, because the host sees the process, not the
//! function: stdout silence is still the contract for a hook with nothing to
//! add, and the distinction has to live in the event row and on stderr.

use std::path::Path;
use std::process::{Command, Output};

/// Run `mdkb hook session-start` in `root` with no daemon anywhere.
///
/// `HOME` points at an empty directory (the daemon socket lives under
/// `$HOME/.mdkb`) and spawning is forbidden, which is what "no daemon" looks
/// like from a hook's side without leaving a background process behind.
fn run_session_start(root: &Path, home: &Path) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["hook", "session-start"])
        .current_dir(root)
        .env("HOME", home)
        .env("MDKB_NO_SPAWN", "1")
        .env("MDKB_NO_DAEMON", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mdkb hook session-start");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"s-outcome","cwd":"."}"#)
        .expect("write the hook event");
    child.wait_with_output().expect("wait")
}

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    (dir, root)
}

/// The `session_start` rows the run appended.
fn session_start_rows(root: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(root.join(".mdkb/hook-events.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v.get("event").and_then(serde_json::Value::as_str) == Some("session_start"))
        .collect()
}

/// The positive control, end to end: a repo whose store opens emits its
/// context to stdout and records `fired`. Without this, "nothing on stdout"
/// could always be explained away as "nothing to say".
#[test]
fn a_healthy_repo_emits_its_context_and_records_fired() {
    let (_dir, root) = store();
    {
        let ctx = mdkb::core::Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            "distinctive-warmup-id",
            "Distinctive warmup title",
            "topic",
            None,
            "body",
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

    let home = tempfile::tempdir().expect("daemon home");
    let out = run_session_start(&root, home.path());

    assert!(out.status.success(), "the hook contract is exit 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.trim().is_empty(),
        "a healthy repo must emit context; stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("distinctive-warmup-id"),
        "the emitted context must carry the warmup entry: {stdout}"
    );

    let rows = session_start_rows(&root);
    assert_eq!(rows.len(), 1, "one row per run: {rows:?}");
    assert_eq!(rows[0]["outcome"], "fired", "{}", rows[0]);
}

/// The defect itself: the store refuses to open, and the run must not look
/// like a hook that was switched off.
///
/// A schema version from the future is exactly what the v27/v28 mismatch was —
/// `refuse_future_schema` rejects the open before any autoheal touches it.
#[test]
fn a_store_that_refuses_to_open_is_recorded_as_failed_not_skipped() {
    let (_dir, root) = store();
    {
        let ctx = mdkb::core::Context::open(&root).expect("open");
        ctx.conn
            .execute("UPDATE schema_version SET version = ?", [9999])
            .expect("write a schema version this binary cannot serve");
    }

    let home = tempfile::tempdir().expect("daemon home");
    let out = run_session_start(&root, home.path());

    assert!(out.status.success(), "the hook contract is still exit 0");
    assert!(
        out.stdout.is_empty(),
        "a broken store must stay silent on stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // Default verbosity is WARN on stderr, so an operator sees this without
    // turning anything on — `-vv` showed nothing at all before.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("the store would not open"),
        "the failure must be visible at default verbosity: {stderr:?}"
    );
    assert!(
        stderr.contains("v9999"),
        "the warning must carry the underlying error: {stderr:?}"
    );

    let rows = session_start_rows(&root);
    assert_eq!(rows.len(), 1, "one row per run: {rows:?}");
    assert_eq!(
        rows[0]["outcome"], "failed",
        "a store that will not open is a fault, not a skip: {}",
        rows[0]
    );
    let reason = rows[0]["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("9999"),
        "the row must name what went wrong: {}",
        rows[0]
    );
}

/// Hooks switched off on purpose stay silent on stdout — and say so in the
/// row, so the two silences are never the same record.
#[test]
fn a_disabled_hook_is_recorded_as_disabled() {
    let (_dir, root) = store();
    let config = root.join(".mdkb/config.toml");
    let mut toml = std::fs::read_to_string(&config).expect("read config");
    toml.push_str("\n[hooks]\nsession_start_enabled = false\n");
    std::fs::write(&config, toml).expect("write config");

    let home = tempfile::tempdir().expect("daemon home");
    let out = run_session_start(&root, home.path());

    assert!(out.status.success(), "the hook contract is exit 0");
    assert!(
        out.stdout.is_empty(),
        "a hook that is off must print nothing: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    let rows = session_start_rows(&root);
    assert_eq!(rows.len(), 1, "one row per run: {rows:?}");
    assert_eq!(rows[0]["outcome"], "disabled", "{}", rows[0]);
    assert!(
        rows[0].get("reason").is_none(),
        "being off on purpose is not a failure reason: {}",
        rows[0]
    );
}
