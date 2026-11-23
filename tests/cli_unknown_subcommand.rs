//! An unrecognized subcommand must name the valid subcommands of the level
//! it was given to, so a caller does not spend a second request on `--help`
//! to find the right one (story 118-20a3).
//!
//! clap already handles the neighboring cases well: a near-miss typo gets a
//! "did you mean" tip, and a missing required argument names it by hand. The
//! gap is a subcommand clap cannot suggest anything close to — it prints
//! "unrecognized subcommand" and a bare usage line, without the list that
//! would let the caller pick correctly on the next try.
//!
//! These tests spawn the real binary: the fix lives in error-path formatting
//! that only clap's own rendering pipeline reproduces faithfully.

use std::process::Output;

#[path = "common/cli.rs"]
mod cli;
use cli::run;

/// clap prints usage errors on stderr; a bad `init` is not needed to reach
/// them — argument parsing runs, and `main` exits on failure, before the
/// binary ever touches `.mdkb/`.
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

#[test]
fn unknown_subcommand_lists_the_valid_subcommands() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["memory", "nosuchthing"], dir.path());
    let text = stderr(&out);

    assert!(
        !out.status.success(),
        "`memory nosuchthing` must fail:\n{text}"
    );
    assert!(
        text.contains("unknown command 'nosuchthing' in 'mdkb memory'"),
        "the error must still name the offending subcommand:\n{text}"
    );
    assert_eq!(out.status.code(), Some(2), "a usage error exits 2:\n{text}");
    assert!(
        text.trim_end().ends_with("Run: mdkb memory --help"),
        "no close name, so the error points at the group's help:\n{text}"
    );
    for name in ["add", "show", "list", "search", "rm", "audit"] {
        assert!(
            text.contains(name),
            "the error must list `{name}` as a valid `memory` subcommand:\n{text}"
        );
    }
}

/// The list must come from the exact command the invalid word was given to,
/// not the top-level command list. `setup remove mcp` only has `claude` and
/// `codex`; `memory`-only names like `audit` must not leak in.
#[test]
fn unknown_subcommand_list_is_scoped_to_the_nested_command() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["setup", "remove", "mcp", "bogus"], dir.path());
    let text = stderr(&out);

    assert!(
        !out.status.success(),
        "`setup remove mcp bogus` must fail:\n{text}"
    );
    for name in ["claude", "codex"] {
        assert!(
            text.contains(name),
            "the error must list `{name}` as a valid `setup remove mcp` subcommand:\n{text}"
        );
    }
    assert!(
        !text.contains("audit"),
        "the list must not leak subcommands from an unrelated command:\n{text}"
    );
}

/// A near-miss typo names the closest command and the command line that works.
#[test]
fn near_miss_typo_still_gets_the_did_you_mean_tip() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["memory", "lst"], dir.path());
    let text = stderr(&out);

    assert!(!out.status.success(), "`memory lst` must fail:\n{text}");
    assert!(
        text.contains("use `mdkb memory list`")
            && text.trim_end().ends_with("Run: mdkb memory list"),
        "a near-miss typo must name the nearest command:\n{text}"
    );
}

/// A missing required argument is named, without a subcommand list.
#[test]
fn missing_argument_error_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(&["memory", "show"], dir.path());
    let text = stderr(&out);

    assert!(
        !out.status.success(),
        "`memory show` with no ID must fail:\n{text}"
    );
    assert!(
        text.contains("missing required argument <ID>"),
        "a missing argument must still be named by hand:\n{text}"
    );
    assert!(
        !text.contains("commands:"),
        "a missing-argument error must not gain a subcommand list:\n{text}"
    );
}
