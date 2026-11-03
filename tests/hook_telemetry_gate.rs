//! The hook logs are developer telemetry: off unless `daemon.toml` or
//! `MDKB_HOOK_TELEMETRY` turns them on, and never a row for a tool hook that
//! did nothing (story 032-2a2a).

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use mdkb::cli::handlers::handle_init;
use tempfile::TempDir;

/// Run `mdkb hook <event>` in `repo` against an isolated `HOME`, so the real
/// `~/.mdkb/daemon.toml` of whoever runs the suite cannot decide the outcome.
fn run_hook(repo: &Path, home: &Path, event: &str, telemetry_env: Option<&str>) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mdkb"));
    command
        .args(["hook", event])
        .current_dir(repo)
        .env("HOME", home)
        .env("MDKB_NO_DAEMON", "1")
        .env_remove("MDKB_HOOK_TELEMETRY")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(value) = telemetry_env {
        command.env("MDKB_HOOK_TELEMETRY", value);
    }
    let mut child = command.spawn().expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{}")
        .expect("write stdin");
    child.wait().expect("wait");
}

fn store() -> (TempDir, TempDir) {
    let repo = tempfile::tempdir().expect("repo");
    handle_init(repo.path()).expect("init");
    (repo, tempfile::tempdir().expect("home"))
}

fn events(repo: &Path) -> String {
    std::fs::read_to_string(repo.join(".mdkb/hook-events.jsonl")).unwrap_or_default()
}

/// Catches: telemetry still written by default, in every repo, with no way to
/// turn it off.
#[test]
fn no_flag_means_no_hook_log() {
    let (repo, home) = store();
    run_hook(repo.path(), home.path(), "session-start", None);

    assert!(!repo.path().join(".mdkb/hook-events.jsonl").exists());
    assert!(!repo.path().join(".mdkb/hook-slow.jsonl").exists());
}

/// Catches: a gate that disables logging for the developer who opted in through
/// the documented place, the global `daemon.toml`.
#[test]
fn daemon_toml_flag_turns_the_log_on() {
    let (repo, home) = store();
    std::fs::create_dir_all(home.path().join(".mdkb")).unwrap();
    std::fs::write(
        home.path().join(".mdkb/daemon.toml"),
        "[telemetry]\nhook_events = true\n",
    )
    .unwrap();
    run_hook(repo.path(), home.path(), "session-start", None);

    assert!(events(repo.path()).contains(r#""event":"session_start""#));
}

/// Catches: the per-repo `.mdkb/config.toml` being honoured, which would put
/// the switch back in every repo.
#[test]
fn repo_config_cannot_turn_the_log_on() {
    let (repo, home) = store();
    let config = repo.path().join(".mdkb/config.toml");
    let mut toml = std::fs::read_to_string(&config).unwrap_or_default();
    toml.push_str("\n[telemetry]\nhook_events = true\n");
    std::fs::write(&config, toml).unwrap();
    run_hook(repo.path(), home.path(), "session-start", None);

    assert!(!repo.path().join(".mdkb/hook-events.jsonl").exists());
}

/// Catches: the 79% of rows that were a tool hook doing nothing coming back
/// once the flag is on. The control row proves the log is live.
#[test]
fn a_skipped_tool_hook_writes_no_row_even_with_the_flag_on() {
    let (repo, home) = store();
    run_hook(repo.path(), home.path(), "pre-tool-use", Some("1"));
    run_hook(repo.path(), home.path(), "post-tool-use", Some("1"));
    run_hook(repo.path(), home.path(), "session-start", Some("1"));

    let text = events(repo.path());
    assert!(
        text.contains(r#""event":"session_start""#),
        "control: {text}"
    );
    let noop_rows = text
        .lines()
        .filter(|l| l.contains("_tool_use") && l.contains(r#""outcome":"skipped""#))
        .count();
    assert_eq!(noop_rows, 0, "skipped tool-hook rows were written: {text}");
}
