//! Integration tests for `mdkb hook post-tool-use`.
//!
//! With IPC dispatch (story 051-064), paths are injected into the watcher's
//! mpsc channel instead of written to reindex-queue.jsonl. Tests verify:
//! - exit 0 and silent stdout for all code paths
//! - reindex-queue.jsonl is NOT created (the queue file is abolished)
//! - non-editing tools, missing input, and mdkbignore are still silently ignored

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use mdkb::cli::handlers::handle_init;
use tempfile::TempDir;

fn mdkb_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_mdkb");
    Command::new(bin)
}

fn run_post_tool_use_in(dir: &Path, stdin_json: &str) -> (i32, String) {
    let mut child = mdkb_bin()
        .args(["hook", "post-tool-use"])
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

fn init_project() -> TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    handle_init(tmp.path()).expect("init");
    tmp
}

fn queue_path(root: &Path) -> std::path::PathBuf {
    root.join(".mdkb").join("reindex-queue.jsonl")
}

#[test]
fn post_tool_use_injects_edit_path_silently() {
    let tmp = init_project();
    let canonical_root = std::fs::canonicalize(tmp.path()).unwrap();
    let target = canonical_root.join("src/foo.rs");
    let event = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#,
        target.display()
    );

    let (code, stdout) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0, "must exit 0; stdout={stdout}");
    assert!(stdout.trim().is_empty(), "must produce no stdout; got: {stdout}");
    assert!(!queue_path(tmp.path()).exists(), "reindex-queue.jsonl must not be created");
}

#[test]
fn post_tool_use_injects_write_path_silently() {
    let tmp = init_project();
    let target = tmp.path().join("README.md");
    let event = format!(
        r#"{{"tool_name":"Write","tool_input":{{"file_path":"{}"}}}}"#,
        target.display()
    );

    let (code, stdout) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0, "must exit 0; stdout={stdout}");
    assert!(stdout.trim().is_empty(), "must produce no stdout; got: {stdout}");
    assert!(!queue_path(tmp.path()).exists(), "reindex-queue.jsonl must not be created");
}

#[test]
fn post_tool_use_injects_notebook_edit_path_silently() {
    let tmp = init_project();
    let canonical_root = std::fs::canonicalize(tmp.path()).unwrap();
    let target = canonical_root.join("notebook.ipynb");
    let event = format!(
        r#"{{"tool_name":"NotebookEdit","tool_input":{{"notebook_path":"{}"}}}}"#,
        target.display()
    );

    let (code, stdout) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0, "must exit 0; stdout={stdout}");
    assert!(stdout.trim().is_empty(), "must produce no stdout; got: {stdout}");
    assert!(!queue_path(tmp.path()).exists(), "reindex-queue.jsonl must not be created");
}

#[test]
fn post_tool_use_ignores_non_edit_tools() {
    let tmp = init_project();
    let event = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;

    let (code, stdout) = run_post_tool_use_in(tmp.path(), event);
    assert_eq!(code, 0, "must exit 0; stdout={stdout}");
    assert!(stdout.trim().is_empty(), "non-editing tool must produce no stdout; got: {stdout}");
    assert!(!queue_path(tmp.path()).exists(), "non-editing tool must not create queue file");
}

#[test]
fn post_tool_use_multiple_calls_each_succeed_silently() {
    let tmp = init_project();
    let p1 = tmp.path().join("a.rs");
    let p2 = tmp.path().join("b.rs");

    let e1 = format!(r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#, p1.display());
    let e2 = format!(r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#, p2.display());

    let (c1, s1) = run_post_tool_use_in(tmp.path(), &e1);
    let (c2, s2) = run_post_tool_use_in(tmp.path(), &e2);

    assert_eq!(c1, 0, "first call must exit 0; stdout={s1}");
    assert_eq!(c2, 0, "second call must exit 0; stdout={s2}");
    assert!(!queue_path(tmp.path()).exists(), "reindex-queue.jsonl must not be created");
}

#[test]
fn post_tool_use_respects_mdkbignore_hooks_marker() {
    let tmp = init_project();
    fs::write(tmp.path().join(".mdkbignore-hooks"), "").expect("write marker");
    let target = tmp.path().join("foo.rs");
    let event = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#,
        target.display()
    );

    let (code, stdout) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0, "must exit 0; stdout={stdout}");
    assert!(stdout.trim().is_empty(), "opt-out marker must suppress output; got: {stdout}");
    assert!(!queue_path(tmp.path()).exists(), "opt-out marker must suppress queue creation");
}

#[test]
fn post_tool_use_on_uninitialized_project_is_noop() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let target = tmp.path().join("foo.rs");
    let event = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#,
        target.display()
    );

    let (code, stdout) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0, "must never block; stdout={stdout}");
    assert!(
        stdout.trim().is_empty(),
        "no .mdkb/ means no output, got: {stdout}"
    );
    assert!(!queue_path(tmp.path()).exists(), "no .mdkb/ means no queue file");
}

#[test]
fn post_tool_use_missing_file_path_is_noop() {
    let tmp = init_project();
    let event = r#"{"tool_name":"Edit","tool_input":{}}"#;

    let (code, stdout) = run_post_tool_use_in(tmp.path(), event);
    assert_eq!(code, 0, "must exit 0; stdout={stdout}");
    assert!(!queue_path(tmp.path()).exists(), "missing file_path must not create queue file");
}
