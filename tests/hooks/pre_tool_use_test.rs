//! Integration tests for `mdkb hook pre-tool-use` — definition-search redirect.
//!
//! Two behaviours are covered against the real binary:
//! 1. A definition Grep/Bash for an *indexed* symbol injects real `file:line`
//!    hits from the code index ("act, not suggest").
//! 2. A definition search for an *unknown* symbol falls back to the generic
//!    `mdkb search --scope symbols` suggestion.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn mdkb_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_mdkb"))
}

fn run(args: &[&str], cwd: &Path) -> (i32, String) {
    let out = mdkb_bin()
        .args(args)
        .current_dir(cwd)
        .env("MDKB_NO_DAEMON", "1")
        .stderr(Stdio::null())
        .output()
        .expect("run mdkb");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn run_pre_tool_use(cwd: &Path, stdin_json: &str) -> (i32, String) {
    let mut child = mdkb_bin()
        .args(["hook", "pre-tool-use"])
        .current_dir(cwd)
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
    let out = child.wait_with_output().expect("wait");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

/// Init a project and index a source file carrying a known `fn` symbol.
fn seed_indexed_project(symbol: &str) -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (code, _) = run(&["init"], root);
    assert_eq!(code, 0, "init must succeed");

    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    // A blank leading line means the symbol is defined on line 2.
    std::fs::write(
        src.join("lib.rs"),
        format!("\npub fn {symbol}(x: i32) -> i32 {{\n    x + 1\n}}\n"),
    )
    .unwrap();

    let (code, _) = run(&["code", "init"], root);
    assert_eq!(code, 0, "code init must succeed");
    // Index from the project root so stored paths are root-relative
    // (`src/lib.rs`) — matching how `mdkb code index` runs in production and
    // keeping the injected `file:line` openable from the repo root.
    let (code, _) = run(&["code", "index"], root);
    assert_eq!(code, 0, "code index must succeed");
    tmp
}

fn additional_context(stdout: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    parsed
        .get("hookSpecificOutput")
        .and_then(|h| h.get("additionalContext"))
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn assert_context_only_response(stdout: &str) {
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("expected JSON, got: {stdout}"));
    let hook_output = parsed
        .get("hookSpecificOutput")
        .unwrap_or_else(|| panic!("expected hookSpecificOutput, got: {stdout}"));
    assert!(
        hook_output.get("permissionDecision").is_none(),
        "context-only responses must omit permissionDecision, got: {stdout}"
    );
    assert!(
        hook_output.get("updatedInput").is_none(),
        "context-only responses must not rewrite tool input, got: {stdout}"
    );
}

#[test]
fn pre_tool_use_injects_code_index_hits_for_indexed_symbol() {
    let symbol = "frobnicate_widget";
    let tmp = seed_indexed_project(symbol);

    let event = format!(r#"{{"tool_name":"Grep","tool_input":{{"pattern":"fn {symbol}"}}}}"#);
    let (code, stdout) = run_pre_tool_use(tmp.path(), &event);

    assert_eq!(code, 0, "hook must always exit 0");
    let ctx = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected additionalContext, got: {stdout}"));

    assert!(
        ctx.contains("src/lib.rs:2"),
        "must inject real file:line for the indexed symbol, got: {ctx}"
    );
    assert!(
        ctx.contains("(Function)"),
        "must label the symbol kind, got: {ctx}"
    );
    assert!(
        !ctx.contains("search --scope symbols"),
        "must NOT fall back to the suggestion when a hit exists, got: {ctx}"
    );
    assert_context_only_response(&stdout);
}

#[test]
fn pre_tool_use_caps_hits_at_five() {
    // Six identically-named symbols across six files; the injected block must be
    // truncated to 5 hits (regression guard on `symbols.truncate(5)`).
    let symbol = "dup_symbol";
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    assert_eq!(run(&["init"], root).0, 0);
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..6 {
        std::fs::write(
            src.join(format!("m{i}.rs")),
            format!("pub fn {symbol}() -> i32 {{ {i} }}\n"),
        )
        .unwrap();
    }
    assert_eq!(run(&["code", "init"], root).0, 0);
    assert_eq!(run(&["code", "index"], root).0, 0);

    let event = format!(r#"{{"tool_name":"Grep","tool_input":{{"pattern":"fn {symbol}"}}}}"#);
    let (code, stdout) = run_pre_tool_use(root, &event);
    assert_eq!(code, 0);
    let ctx = additional_context(&stdout).unwrap_or_else(|| panic!("expected hits, got: {stdout}"));
    let hit_lines = ctx.lines().filter(|l| l.starts_with("- ")).count();
    assert_eq!(
        hit_lines, 5,
        "must cap injected hits at 5, got {hit_lines} in:\n{ctx}"
    );
}

#[test]
fn pre_tool_use_injects_code_index_hits_via_bash_grep() {
    let symbol = "frobnicate_widget";
    let tmp = seed_indexed_project(symbol);

    let event =
        format!(r#"{{"tool_name":"Bash","tool_input":{{"command":"rg \"fn {symbol}\" src/"}}}}"#);
    let (code, stdout) = run_pre_tool_use(tmp.path(), &event);

    assert_eq!(code, 0);
    let ctx = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected additionalContext, got: {stdout}"));
    assert!(
        ctx.contains("src/lib.rs:2"),
        "Bash definition search must inject file:line too, got: {ctx}"
    );
    assert_context_only_response(&stdout);
}

#[test]
fn pre_tool_use_falls_back_to_suggestion_for_unknown_symbol() {
    let tmp = seed_indexed_project("frobnicate_widget");

    let event = r#"{"tool_name":"Grep","tool_input":{"pattern":"fn no_such_symbol_here"}}"#;
    let (code, stdout) = run_pre_tool_use(tmp.path(), event);

    assert_eq!(code, 0);
    let ctx = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected fallback suggestion, got: {stdout}"));
    assert!(
        ctx.contains("search --scope symbols") && ctx.contains("no_such_symbol_here"),
        "unknown symbol must fall back to the suggestion string, got: {ctx}"
    );
    assert_context_only_response(&stdout);
}

#[test]
fn pre_tool_use_flag_off_falls_back_to_suggestion() {
    // With `code_hits_in_pretooluse = false`, even an indexed symbol must yield
    // only the suggestion — proving the feature is independently killable.
    let symbol = "frobnicate_widget";
    let tmp = seed_indexed_project(symbol);
    std::fs::write(
        tmp.path().join(".mdkb/config.toml"),
        "[hooks]\ncode_hits_in_pretooluse = false\n",
    )
    .unwrap();

    let event = format!(r#"{{"tool_name":"Grep","tool_input":{{"pattern":"fn {symbol}"}}}}"#);
    let (code, stdout) = run_pre_tool_use(tmp.path(), &event);

    assert_eq!(code, 0);
    let ctx =
        additional_context(&stdout).unwrap_or_else(|| panic!("expected suggestion, got: {stdout}"));
    assert!(
        ctx.contains("search --scope symbols") && !ctx.contains("src/lib.rs:"),
        "flag off must fall back to the suggestion, not inject hits, got: {ctx}"
    );
}

#[test]
fn pre_tool_use_missing_code_index_still_suggests() {
    // A project with no code index DB at all must behave exactly as before
    // (suggestion only) and the hook must NOT recreate the index on the hot path.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (code, _) = run(&["init"], root);
    assert_eq!(code, 0);
    // `init` seeds an empty code index; remove it to model a never-indexed repo.
    // Assert the precondition so this test fails loudly (rather than vacuously
    // passing) if `init` ever stops creating the DB.
    let db = root.join(".mdkb/code.sqlite");
    assert!(
        db.exists(),
        "init must seed code.sqlite for this test to be meaningful"
    );
    std::fs::remove_file(&db).unwrap();

    let event = r#"{"tool_name":"Grep","tool_input":{"pattern":"fn some_symbol"}}"#;
    let (code, stdout) = run_pre_tool_use(root, event);

    assert_eq!(code, 0);
    let ctx = additional_context(&stdout)
        .unwrap_or_else(|| panic!("expected suggestion without index, got: {stdout}"));
    assert!(
        ctx.contains("search --scope symbols"),
        "without a code index the hook must still emit the suggestion, got: {ctx}"
    );
    assert!(
        !db.exists(),
        "hook must NOT create a code index on the hot path"
    );
}
