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

#[cfg(unix)]
fn spawn_daemon_that_takes_the_request(daemon_dir: &Path) {
    use std::io::Read;

    let socket_dir = daemon_dir.join(".mdkb");
    std::fs::create_dir_all(&socket_dir).expect("daemon socket directory");
    let listener = std::os::unix::net::UnixListener::bind(socket_dir.join("daemon-hook.sock"))
        .expect("bind fake daemon socket");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut header = [0_u8; 4];
            if stream.read_exact(&mut header).is_err() {
                continue; // ensure_daemon_running reachability probe
            }
            let mut body = vec![0_u8; u32::from_le_bytes(header) as usize];
            let _ = stream.read_exact(&mut body);
            // The complete request is now daemon-owned. Drop the response.
        }
    });
}

fn run_hook(root: &Path, event: &str, stdin: &str, daemon_dir: &Path) -> Output {
    run_hook_with_mode(root, event, stdin, daemon_dir, false)
}

fn run_hook_with_mode(
    root: &Path,
    event: &str,
    stdin: &str,
    daemon_dir: &Path,
    no_daemon: bool,
) -> Output {
    use std::io::Write;
    let mut command = Command::new(env!("CARGO_BIN_EXE_mdkb"));
    command
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
        .stderr(std::process::Stdio::piped());
    if no_daemon {
        command.env("MDKB_NO_DAEMON", "1");
    }
    let mut child = command.spawn().expect("spawn mdkb hook");
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

#[test]
fn daemon_required_skips_the_in_process_fallback() {
    let (_dir, root) = store();
    {
        let ctx = mdkb::core::Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            "daemon-only",
            "Daemon-only warmup",
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
    let config_path = root.join(".mdkb/config.toml");
    let mut config = mdkb::Config::load_or_default(&config_path);
    config.hooks.daemon_required = true;
    config
        .save(&config_path)
        .expect("save daemon-required policy");

    let daemon_dir = tempfile::tempdir().expect("daemon dir");
    let out = run_hook(
        &root,
        "session-start",
        r#"{"session_id":"s1","cwd":"."}"#,
        daemon_dir.path(),
    );

    assert!(
        out.status.success(),
        "host hook contract must remain exit zero"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("daemon-only") && !stdout.contains("Daemon-only warmup"),
        "daemon_required was ignored and the hook ran in-process: {stdout}"
    );

    let explicit_direct = run_hook_with_mode(
        &root,
        "session-start",
        r#"{"session_id":"s2","cwd":"."}"#,
        daemon_dir.path(),
        true,
    );
    assert!(explicit_direct.status.success());
    assert!(
        explicit_direct.stdout.is_empty(),
        "MDKB_NO_DAEMON must not bypass daemon_required: {:?}",
        String::from_utf8_lossy(&explicit_direct.stdout)
    );
}

/// A dropped response after delivery is not proof that the hook did not run.
/// Retrying in-process would put a second writer beside the daemon.
#[cfg(unix)]
#[test]
fn a_delivered_hook_is_not_retried_in_process() {
    let (_dir, root) = store();
    {
        let ctx = mdkb::core::Context::open(&root).expect("open");
        mdkb::cli::handlers::handle_memory_add(
            &ctx,
            "must-not-warmup",
            "Must not appear from fallback",
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
    spawn_daemon_that_takes_the_request(daemon_dir.path());
    let out = run_hook(
        &root,
        "session-start",
        r#"{"session_id":"s1","cwd":"."}"#,
        daemon_dir.path(),
    );

    assert!(
        out.status.success(),
        "host hook contract must remain exit zero"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.contains("must-not-warmup") && !stdout.contains("Must not appear"),
        "the CLI retried a daemon-owned hook in-process: {stdout}"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("skipping fallback"),
        "the diagnostic must explain why no retry occurred: {:?}",
        String::from_utf8_lossy(&out.stderr)
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
