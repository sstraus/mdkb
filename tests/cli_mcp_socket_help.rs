//! `mdkb mcp --help` must state where `--socket` works.
//!
//! The flag drives the daemon proxy, and the daemon is Unix-only. A help page
//! that names the flag without naming that scope sends a Windows user to try
//! it, hit a refusal, and come back — the round trip this test exists to
//! prevent. The reader of `--help` is usually an agent that will never open
//! the source, so the scope has to be in the help text itself.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn run_help() -> Output {
    let dir = tempfile::tempdir().unwrap();
    Command::new(PathBuf::from(env!("CARGO_BIN_EXE_mdkb")))
        .args(["mcp", "--help"])
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn failed for `mdkb mcp --help`")
}

/// Proves: the `--socket` help text names its platform scope.
///
/// This is the test for the second of the two review notes on PR #2. It reads
/// the real `--help` output rather than the doc comment, so deleting the
/// clause from the source fails here.
#[test]
fn mcp_help_states_that_socket_is_unix_only() {
    let out = run_help();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "`mdkb mcp --help` must succeed:\n{text}"
    );
    assert!(
        text.contains("--socket"),
        "help must still document the flag:\n{text}"
    );
    assert!(
        text.contains("Unix only"),
        "help for --socket must name its platform scope so a Windows user is \
         not sent on a round trip:\n{text}"
    );
}
