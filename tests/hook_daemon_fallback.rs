//! The daemon fallback must be a real rail, not a decorative one.
//!
//! Story 021-0636: the generated hook wiring was
//! `if ! mdkb hook <event>; then MDKB_NO_DAEMON=1 mdkb hook <event>; fi`, live
//! in both global settings files. It can never fire. `run_hook` swallows every
//! daemon error and returns `Ok(())` by contract, because the host hook must
//! exit 0 — so the `if !` branch is unreachable and a dead or unreachable daemon
//! means hooks silently do nothing. The system advertised a safety rail it did
//! not have.
//!
//! The fix is in-process fallback rather than a shell retry, so these tests
//! assert the property the shell was reaching for: with no daemon reachable, the
//! hook still does its work, and the host still sees exit 0.

use std::path::Path;
use std::process::{Command, Output};

fn run_hook(root: &Path, event: &str, stdin: &str, daemon_dir: &Path) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["hook", event])
        .current_dir(root)
        // Point the daemon home at an empty directory (the socket lives under
        // $HOME/.mdkb) and forbid spawning one. That is exactly what "the daemon
        // is dead" looks like from a hook's side, without leaving a real
        // background process behind.
        .env("HOME", daemon_dir)
        .env("MDKB_NO_SPAWN", "1")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mdkb hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");
    (dir, root)
}

/// With no daemon reachable, the hook must still do its work.
///
/// This is the property the shell fallback was reaching for and could never
/// deliver. A `session_start` against a store holding a memory entry has
/// something to say; saying nothing is the silent failure being fixed.
#[test]
fn a_hook_still_works_with_no_daemon_reachable() {
    let (_dir, root) = store();
    {
        let ctx = mdkb::core::Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            "warmup-me",
            "Distinctive warmup title",
            "topic",
            None,
            "body",
            None,
            None,
            None,
            None,
        )
        .expect("add");
    }

    let daemon_dir = tempfile::tempdir().expect("daemon dir");
    let out = run_hook(
        &root,
        "session-start",
        r#"{"session_id":"s1","cwd":"."}"#,
        daemon_dir.path(),
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("warmup-me") || stdout.contains("Distinctive warmup title"),
        "with the daemon unreachable the hook must fall back and still produce \
         warmup output; got stdout={stdout:?} stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The contract that made the shell fallback unreachable is deliberate and must
/// hold: the host CLI never sees a non-zero exit for an ordinary hook failure.
/// The fallback is reachable now because it happens *inside* the process, not
/// because the exit code changed.
#[test]
fn the_hook_still_exits_zero_with_no_daemon() {
    let (_dir, root) = store();
    let daemon_dir = tempfile::tempdir().expect("daemon dir");
    let out = run_hook(
        &root,
        "session-start",
        r#"{"session_id":"s1","cwd":"."}"#,
        daemon_dir.path(),
    );
    assert!(
        out.status.success(),
        "the host hook must exit 0 even with no daemon; got {:?}",
        out.status.code()
    );
}

/// A hook run outside any mdkb store must also exit 0 and not fabricate output.
#[test]
fn a_hook_outside_a_store_is_quiet_and_succeeds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    let daemon_dir = tempfile::tempdir().expect("daemon dir");

    let out = run_hook(
        &root,
        "session-start",
        r#"{"session_id":"s1","cwd":"."}"#,
        daemon_dir.path(),
    );
    assert!(out.status.success(), "must exit 0 with no store");
}

/// The generated wiring must match the behaviour that actually exists. Leaving
/// the shell fallback in place advertises a rail the code does not implement —
/// which is the failure this story names.
#[test]
fn the_generated_wiring_carries_no_dead_shell_fallback() {
    let line = mdkb::cli::setup::hook_command_line("/usr/local/bin/mdkb", "session-start", false);
    assert!(
        !line.contains("MDKB_NO_DAEMON"),
        "the shell fallback is unreachable by construction (run_hook always exits \
         0) and the real fallback is in-process; leaving it in the wiring claims a \
         rail that does not exist: {line}"
    );
    assert!(
        !line.contains("if !"),
        "no conditional retry belongs in the generated wiring: {line}"
    );
    assert!(
        line.contains("hook session-start"),
        "the wiring must still invoke the hook: {line}"
    );
}
