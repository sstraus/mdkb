//! Enum-valued CLI flags must publish their accepted values and reject the rest
//! as a usage error.
//!
//! The audience for `--help` is overwhelmingly an agent that has never run the
//! command before and will not read the source. A flag that names a closed set
//! without listing it forces a guess, and a guess that fails at runtime with a
//! Rust debug payload (`Error { kind: InvalidQuery("Invalid entry type:
//! pattern"), .. }`) teaches nothing about what would have worked.
//!
//! Every assertion here derives its expectations from the domain enums, so a
//! variant added to `EntryType`, `SourceType` or `MemoryRelation` and forgotten
//! in the CLI fails this file rather than shipping.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use mdkb::store::memory::{EntryType, SourceType};
use mdkb::store::memory_graph::MemoryRelation;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_mdkb"))
}

fn run(args: &[&str], cwd: &Path) -> Output {
    Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| panic!("spawn failed for `mdkb {}`: {e}", args.join(" ")))
}

/// clap prints `--help` on stdout and usage errors on stderr; a caller only
/// cares that the values are somewhere in the output.
fn combined(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn entry_types() -> Vec<String> {
    EntryType::ALL.iter().map(ToString::to_string).collect()
}

fn source_types() -> Vec<String> {
    SourceType::ALL.iter().map(ToString::to_string).collect()
}

fn relations() -> Vec<String> {
    MemoryRelation::ALL
        .iter()
        .map(|r| r.as_str().to_string())
        .collect()
}

/// A help page names every accepted value, and no value that is not accepted.
fn assert_help_lists(args: &[&str], values: &[String], flag: &str) {
    let dir = tempfile::tempdir().unwrap();
    let out = run(args, dir.path());
    assert!(
        out.status.success(),
        "`mdkb {}` must succeed",
        args.join(" ")
    );
    let text = combined(&out);
    for value in values {
        assert!(
            text.contains(value.as_str()),
            "`mdkb {}` must list `{value}` as an accepted {flag} value:\n{text}",
            args.join(" ")
        );
    }
}

/// A rejected value fails as a *usage* error: it names the accepted set and
/// never leaks a Rust debug-formatted error payload.
fn assert_usage_error(args: &[&str], values: &[String], bad: &str) {
    let dir = tempfile::tempdir().unwrap();
    run(&["init"], dir.path());
    let out = run(args, dir.path());
    let text = combined(&out);

    assert!(
        !out.status.success(),
        "`{bad}` must be rejected by `mdkb {}`:\n{text}",
        args.join(" ")
    );
    assert!(
        !text.contains("InvalidQuery") && !text.contains("backtrace:"),
        "a bad value must not surface a Rust debug payload:\n{text}"
    );
    for value in values {
        assert!(
            text.contains(value.as_str()),
            "the rejection must name `{value}` as an accepted value:\n{text}"
        );
    }
}

#[test]
fn memory_add_help_lists_the_entry_types() {
    assert_help_lists(&["memory", "add", "--help"], &entry_types(), "--entry-type");
}

#[test]
fn memory_add_help_lists_the_source_types() {
    assert_help_lists(
        &["memory", "add", "--help"],
        &source_types(),
        "--source-type",
    );
}

#[test]
fn search_help_lists_the_entry_types() {
    assert_help_lists(&["search", "--help"], &entry_types(), "--entry-type");
}

#[test]
fn hook_memory_write_help_lists_the_entry_types() {
    assert_help_lists(
        &["hook", "memory-write", "--help"],
        &entry_types(),
        "--entry-type",
    );
}

#[test]
fn memory_link_help_lists_the_relations() {
    assert_help_lists(&["memory", "link", "--help"], &relations(), "relation");
}

/// The value from the original report: `pattern` looks plausible (the old help
/// text for `hook memory-write` even claimed it) and is not a variant.
#[test]
fn memory_add_rejects_an_unknown_entry_type_as_a_usage_error() {
    assert_usage_error(
        &[
            "memory",
            "add",
            "x",
            "--title",
            "T",
            "--content",
            "C",
            "--entry-type",
            "pattern",
        ],
        &entry_types(),
        "pattern",
    );
}

#[test]
fn memory_add_rejects_an_unknown_source_type_as_a_usage_error() {
    assert_usage_error(
        &[
            "memory",
            "add",
            "x",
            "--title",
            "T",
            "--content",
            "C",
            "--source-type",
            "hearsay",
        ],
        &source_types(),
        "hearsay",
    );
}

#[test]
fn search_rejects_an_unknown_entry_type_as_a_usage_error() {
    assert_usage_error(
        &["search", "q", "--entry-type", "pattern"],
        &entry_types(),
        "pattern",
    );
}

#[test]
fn memory_link_rejects_an_unknown_relation_as_a_usage_error() {
    assert_usage_error(
        &["memory", "link", "a", "mentions", "b"],
        &relations(),
        "mentions",
    );
}

/// No help text may enumerate a closed set by hand: the copy drifts from the
/// enum and then actively lies. `hook memory-write --entry-type` documented
/// `pattern`, which has never been a variant, and `search --entry-type` omitted
/// `handoff`. The accepted values belong in `[possible values: ...]`, which clap
/// derives from the parser.
#[test]
fn help_text_never_hand_lists_the_entry_types() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        ["memory", "add", "--help"],
        ["search", "--help", ""],
        ["hook", "memory-write", "--help"],
    ] {
        let args: Vec<&str> = args.iter().copied().filter(|a| !a.is_empty()).collect();
        let text = combined(&run(&args, dir.path()));
        let doc = text
            .lines()
            .find(|l| l.contains("--entry-type"))
            .unwrap_or_else(|| panic!("no --entry-type line in `mdkb {}`", args.join(" ")))
            .to_string();
        // The description column, i.e. everything before clap's bracketed
        // metadata, must not enumerate variants itself.
        let description = doc.split('[').next().unwrap_or_default();
        let listed = EntryType::ALL
            .iter()
            .filter(|t| description.contains(&t.to_string()))
            .count();
        assert!(
            listed <= 1,
            "`mdkb {}` hand-lists entry types in its description instead of \
             deriving them: {description}",
            args.join(" ")
        );
    }
}
