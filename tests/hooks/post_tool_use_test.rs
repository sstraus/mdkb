//! Integration tests for `mdkb hook post-tool-use` — reindex queue.

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

fn read_queue_lines(root: &Path) -> Vec<serde_json::Value> {
    let content = fs::read_to_string(queue_path(root)).unwrap_or_default();
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("queue line must be valid JSON"))
        .collect()
}

#[test]
fn post_tool_use_enqueues_edit_path() {
    let tmp = init_project();
    let canonical_root = std::fs::canonicalize(tmp.path()).unwrap();
    let target = canonical_root.join("src/foo.rs");
    let event = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#,
        target.display()
    );

    let (code, _stdout) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0);

    let entries = read_queue_lines(tmp.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].get("path").and_then(|v| v.as_str()),
        Some(target.to_string_lossy().as_ref())
    );
    assert!(
        entries[0].get("at").and_then(|v| v.as_i64()).is_some(),
        "timestamp must be present"
    );
}

#[test]
fn post_tool_use_enqueues_write_path() {
    let tmp = init_project();
    let target = tmp.path().join("README.md");
    let event = format!(
        r#"{{"tool_name":"Write","tool_input":{{"file_path":"{}"}}}}"#,
        target.display()
    );

    let (code, _) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0);

    let entries = read_queue_lines(tmp.path());
    assert_eq!(entries.len(), 1);
}

#[test]
fn post_tool_use_enqueues_notebook_edit_path() {
    let tmp = init_project();
    let canonical_root = std::fs::canonicalize(tmp.path()).unwrap();
    let target = canonical_root.join("notebook.ipynb");
    let event = format!(
        r#"{{"tool_name":"NotebookEdit","tool_input":{{"notebook_path":"{}"}}}}"#,
        target.display()
    );

    let (code, _) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0);

    let entries = read_queue_lines(tmp.path());
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].get("path").and_then(|v| v.as_str()),
        Some(target.to_string_lossy().as_ref())
    );
}

#[test]
fn post_tool_use_ignores_non_edit_tools() {
    let tmp = init_project();
    let event = r#"{"tool_name":"Bash","tool_input":{"command":"ls"}}"#;

    let (code, _) = run_post_tool_use_in(tmp.path(), event);
    assert_eq!(code, 0);

    assert!(
        !queue_path(tmp.path()).exists() || read_queue_lines(tmp.path()).is_empty(),
        "non-editing tools must not produce queue entries"
    );
}

#[test]
fn post_tool_use_appends_to_existing_queue() {
    let tmp = init_project();
    let p1 = tmp.path().join("a.rs");
    let p2 = tmp.path().join("b.rs");

    let e1 = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#,
        p1.display()
    );
    let e2 = format!(
        r#"{{"tool_name":"Edit","tool_input":{{"file_path":"{}"}}}}"#,
        p2.display()
    );

    run_post_tool_use_in(tmp.path(), &e1);
    run_post_tool_use_in(tmp.path(), &e2);

    let entries = read_queue_lines(tmp.path());
    assert_eq!(
        entries.len(),
        2,
        "queue must be append-only, got: {entries:?}"
    );
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

    let (code, _) = run_post_tool_use_in(tmp.path(), &event);
    assert_eq!(code, 0);

    assert!(
        !queue_path(tmp.path()).exists(),
        "opt-out marker must suppress queueing"
    );
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
    assert_eq!(code, 0, "must never block");
    let parsed: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");
    assert!(parsed.is_object());
    assert!(
        !queue_path(tmp.path()).exists(),
        "no .mdkb/ means no queue file"
    );
}

#[test]
fn post_tool_use_missing_file_path_is_noop() {
    let tmp = init_project();
    let event = r#"{"tool_name":"Edit","tool_input":{}}"#;

    let (code, _) = run_post_tool_use_in(tmp.path(), event);
    assert_eq!(code, 0);
    assert!(
        !queue_path(tmp.path()).exists() || read_queue_lines(tmp.path()).is_empty(),
        "missing file_path must not produce queue entries"
    );
}
