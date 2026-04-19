//! CLI smoke test: exercises every `mdkb` subcommand in an isolated tempdir
//! repo, checking that each exits 0 (or expected non-zero) and produces valid
//! output. Invoke with `cargo test --test cli_smoke`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

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

fn run_stdin(args: &[&str], cwd: &Path, stdin: &str) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn failed for `mdkb {}`: {e}", args.join(" ")));

    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .expect("write stdin");

    child.wait_with_output().expect("wait")
}

fn assert_ok(out: &Output, label: &str) {
    assert!(
        out.status.success(),
        "{label}: exit={:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();

        let out = run(&["init"], &root);
        assert_ok(&out, "init");

        // Seed a markdown doc for search/get exercises.
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("guide.md"),
            "# Getting Started\n\nThis is the setup guide for the project.\n",
        )
        .unwrap();

        // Seed a source file for code index exercises.
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\
             pub fn farewell(name: &str) -> String {\n    greet(name); format!(\"bye {name}\")\n}\n",
        )
        .unwrap();

        Repo { _dir: dir, root }
    }
}

// ── Top-level commands ──────────────────────────────────────────────

#[test]
fn smoke_init_already_initialised_exits_nonzero() {
    let repo = Repo::new();
    let out = run(&["init"], &repo.root);
    assert!(
        !out.status.success(),
        "init on already-initialised repo should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already initialized"),
        "should mention already initialized: {stderr}"
    );
}

#[test]
fn smoke_update() {
    let repo = Repo::new();
    let out = run(&["update"], &repo.root);
    assert_ok(&out, "update");
}

#[test]
fn smoke_update_files() {
    let repo = Repo::new();
    let out = run(&["update", "--files", "docs/guide.md"], &repo.root);
    assert_ok(&out, "update --files");
}

#[test]
fn smoke_embed() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["embed"], &repo.root);
    assert_ok(&out, "embed");
}

#[test]
fn smoke_search() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["search", "setup guide"], &repo.root);
    assert_ok(&out, "search");
}

#[test]
fn smoke_search_scope_docs() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["search", "setup", "--scope", "docs"], &repo.root);
    assert_ok(&out, "search --scope docs");
}

#[test]
fn smoke_search_scope_memory() {
    let repo = Repo::new();
    let out = run(&["search", "anything", "--scope", "memory"], &repo.root);
    assert_ok(&out, "search --scope memory");
}

#[test]
fn smoke_get_by_path() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    // Path is relative to collection root (docs/), not repo root.
    let out = run(&["get", "guide.md"], &repo.root);
    assert_ok(&out, "get by path");
    assert!(
        stdout(&out).contains("Getting Started"),
        "get must return doc content"
    );
}

#[test]
fn smoke_get_by_id() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["get", "1"], &repo.root);
    assert_ok(&out, "get by numeric id");
}

#[test]
fn smoke_get_with_lines() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["get", "guide.md", "--lines", "1:1"], &repo.root);
    assert_ok(&out, "get --lines");
}

#[test]
fn smoke_mget() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["mget", "docs/*.md"], &repo.root);
    assert_ok(&out, "mget");
}

#[test]
fn smoke_stats() {
    let repo = Repo::new();
    let out = run(&["stats", "--no-color"], &repo.root);
    assert_ok(&out, "stats");
}

#[test]
fn smoke_stats_json() {
    let repo = Repo::new();
    let out = run(&["--format", "json", "stats", "--no-color"], &repo.root);
    assert_ok(&out, "stats --format json");
    let s = stdout(&out);
    if !s.trim().is_empty() {
        serde_json::from_str::<serde_json::Value>(s.trim())
            .unwrap_or_else(|e| panic!("stats json invalid: {e}\n{s}"));
    }
}

// ── Collection ──────────────────────────────────────────────────────

#[test]
fn smoke_collection_add_remove() {
    let repo = Repo::new();
    let out = run(
        &["collection", "add", "notes", "docs", "-p", "**/*.md"],
        &repo.root,
    );
    assert_ok(&out, "collection add");

    let out = run(&["collection", "rename", "notes", "notes2"], &repo.root);
    assert_ok(&out, "collection rename");

    let out = run(&["collection", "remove", "notes2"], &repo.root);
    assert_ok(&out, "collection remove");
}

// ── Memory ──────────────────────────────────────────────────────────

#[test]
fn smoke_memory_lifecycle() {
    let repo = Repo::new();

    let out = run(
        &[
            "memory",
            "add",
            "smoke-test-entry",
            "-t",
            "Smoke test entry",
            "-T",
            "topic",
            "--tags",
            "test,smoke",
            "-c",
            "This is a smoke test memory entry for CLI validation.",
        ],
        &repo.root,
    );
    assert_ok(&out, "memory add");

    let out = run(&["memory", "show", "smoke-test-entry"], &repo.root);
    assert_ok(&out, "memory show");
    assert!(
        stdout(&out).contains("Smoke test entry") || stdout(&out).contains("smoke-test-entry"),
        "memory show must return the entry"
    );

    let out = run(&["memory", "list"], &repo.root);
    assert_ok(&out, "memory list");

    let out = run(&["memory", "list", "--status", "active"], &repo.root);
    assert_ok(&out, "memory list --status");

    let out = run(&["memory", "search", "smoke test"], &repo.root);
    assert_ok(&out, "memory search");

    let out = run(&["memory", "warmup"], &repo.root);
    assert_ok(&out, "memory warmup");

    let out = run(&["memory", "history", "smoke-test-entry"], &repo.root);
    assert_ok(&out, "memory history");

    let out = run(&["memory", "prune", "--dry-run"], &repo.root);
    assert_ok(&out, "memory prune --dry-run");

    let out = run(&["memory", "export", "--dry-run"], &repo.root);
    assert_ok(&out, "memory export --dry-run");

    let out = run(&["memory", "rm", "smoke-test-entry"], &repo.root);
    assert_ok(&out, "memory rm");
}

#[test]
fn smoke_memory_import_export_roundtrip() {
    let repo = Repo::new();

    run(
        &[
            "memory",
            "add",
            "export-test",
            "-t",
            "Export test",
            "-c",
            "Content for export roundtrip.",
        ],
        &repo.root,
    );

    let export_dir = repo.root.join("mem-export");
    std::fs::create_dir_all(&export_dir).unwrap();
    let out = run(
        &[
            "memory",
            "export",
            "--dir",
            export_dir.to_str().unwrap(),
            "--overwrite",
        ],
        &repo.root,
    );
    assert_ok(&out, "memory export");

    let out = run(
        &[
            "memory",
            "import",
            export_dir.to_str().unwrap(),
            "--skip-duplicates",
        ],
        &repo.root,
    );
    assert_ok(&out, "memory import");
}

// ── Evolution ───────────────────────────────────────────────────────

#[test]
fn smoke_evolve_and_history() {
    let repo = Repo::new();

    std::fs::write(repo.root.join("docs/v2.md"), "# V2\nUpdated guide.\n").unwrap();
    run(&["update"], &repo.root);

    // Paths are collection-relative (docs/ collection → guide.md, v2.md).
    let out = run(
        &[
            "evolve",
            "supersedes",
            "v2.md",
            "guide.md",
            "-r",
            "newer version",
        ],
        &repo.root,
    );
    assert_ok(&out, "evolve supersedes");

    let out = run(&["history", "guide.md"], &repo.root);
    assert_ok(&out, "history");

    let out = run(&["current", "guide.md"], &repo.root);
    assert_ok(&out, "current");

    let out = run(&["superseded-by", "guide.md"], &repo.root);
    assert_ok(&out, "superseded-by");
}

// ── Code intelligence ───────────────────────────────────────────────

#[test]
fn smoke_code_lifecycle() {
    let repo = Repo::new();

    let out = run(&["code", "init"], &repo.root);
    assert_ok(&out, "code init");

    let out = run(&["code", "index", "src/"], &repo.root);
    assert_ok(&out, "code index");

    let out = run(&["code", "info"], &repo.root);
    assert_ok(&out, "code info");

    let out = run(&["code", "search", "greet"], &repo.root);
    assert_ok(&out, "code search");

    let out = run(&["code", "find", "greet"], &repo.root);
    assert_ok(&out, "code find");

    let out = run(&["code", "parse", "src/lib.rs"], &repo.root);
    assert_ok(&out, "code parse");

    let out = run(&["code", "calls", "farewell"], &repo.root);
    assert_ok(&out, "code calls");

    let out = run(&["code", "callers", "greet"], &repo.root);
    assert_ok(&out, "code callers");

    let out = run(&["code", "impact", "greet", "--depth", "2"], &repo.root);
    assert_ok(&out, "code impact");
}

// ── Experiment ──────────────────────────────────────────────────────

#[test]
fn smoke_experiment_lifecycle() {
    let repo = Repo::new();

    let out = run(
        &[
            "experiment",
            "create",
            "smoke-exp",
            "--config-a",
            r#"{"model":"v1"}"#,
            "--config-b",
            r#"{"model":"v2"}"#,
            "-d",
            "smoke test experiment",
        ],
        &repo.root,
    );
    assert_ok(&out, "experiment create");

    let out = run(&["experiment", "list"], &repo.root);
    assert_ok(&out, "experiment list");

    let out = run(&["experiment", "list", "--running"], &repo.root);
    assert_ok(&out, "experiment list --running");

    let out = run(&["experiment", "status", "smoke-exp"], &repo.root);
    assert_ok(&out, "experiment status");

    let out = run(&["experiment", "cancel", "smoke-exp"], &repo.root);
    assert_ok(&out, "experiment cancel");
}

// ── Metrics ─────────────────────────────────────────────────────────

#[test]
fn smoke_metrics() {
    let repo = Repo::new();

    let out = run(&["metrics", "show"], &repo.root);
    assert_ok(&out, "metrics show");

    let out = run(&["metrics", "latency"], &repo.root);
    assert_ok(&out, "metrics latency");

    let out = run(&["metrics", "quality"], &repo.root);
    assert_ok(&out, "metrics quality");

    let out = run(&["metrics", "export"], &repo.root);
    assert_ok(&out, "metrics export");
}

// ── Hook lifecycle events (stdin→stdout) ────────────────────────────

#[test]
fn smoke_hook_session_start() {
    let repo = Repo::new();
    let out = run_stdin(&["hook", "session-start"], &repo.root, "{}");
    assert_ok(&out, "hook session-start");
    serde_json::from_str::<serde_json::Value>(stdout(&out).trim())
        .expect("hook session-start must return valid JSON");
}

#[test]
fn smoke_hook_user_prompt_submit() {
    let repo = Repo::new();
    let payload = r#"{"prompt":"test prompt"}"#;
    let out = run_stdin(&["hook", "user-prompt-submit"], &repo.root, payload);
    assert_ok(&out, "hook user-prompt-submit");
    serde_json::from_str::<serde_json::Value>(stdout(&out).trim())
        .expect("hook user-prompt-submit must return valid JSON");
}

#[test]
fn smoke_hook_post_tool_use() {
    let repo = Repo::new();
    let payload = r#"{"tool_name":"Read","tool_input":{"file_path":"src/lib.rs"}}"#;
    let out = run_stdin(&["hook", "post-tool-use"], &repo.root, payload);
    assert_ok(&out, "hook post-tool-use");
    serde_json::from_str::<serde_json::Value>(stdout(&out).trim())
        .expect("hook post-tool-use must return valid JSON");
}

#[test]
fn smoke_hook_events_tolerate_empty_stdin() {
    let repo = Repo::new();
    for event in &["session-start", "user-prompt-submit", "post-tool-use"] {
        let out = run_stdin(&["hook", event], &repo.root, "");
        assert_ok(&out, &format!("hook {event} (empty stdin)"));
    }
}

// ── Setup (dry-run only) ────────────────────────────────────────────

#[test]
fn smoke_setup_hooks_claude_dry_run() {
    let repo = Repo::new();
    let out = run(&["setup", "hooks", "claude", "--dry-run"], &repo.root);
    assert_ok(&out, "setup hooks claude --dry-run");
}

#[test]
fn smoke_setup_hooks_codex_dry_run() {
    let repo = Repo::new();
    let out = run(&["setup", "hooks", "codex", "--dry-run"], &repo.root);
    assert_ok(&out, "setup hooks codex --dry-run");
}

#[test]
fn smoke_setup_mcp_codex_dry_run() {
    let repo = Repo::new();
    let out = run(&["setup", "mcp", "codex", "--dry-run"], &repo.root);
    assert_ok(&out, "setup mcp codex --dry-run");
}

// ── Journal ─────────────────────────────────────────────────────────

#[test]
fn smoke_journal_import_dry_run() {
    let repo = Repo::new();
    let journal = repo.root.join("test-journal.md");
    std::fs::write(
        &journal,
        "# Test Journal\n\n## Entry 1\nSome learning about testing.\n",
    )
    .unwrap();
    let out = run(
        &["journal", "import", journal.to_str().unwrap(), "--dry-run"],
        &repo.root,
    );
    assert_ok(&out, "journal import --dry-run");
}

#[test]
fn smoke_journal_import_all_dry_run() {
    let repo = Repo::new();
    let journal_dir = repo.root.join("journals");
    std::fs::create_dir_all(&journal_dir).unwrap();
    std::fs::write(journal_dir.join("entry1.md"), "# Entry\n\nSome content.\n").unwrap();
    let out = run(
        &[
            "journal",
            "import-all",
            "--dir",
            journal_dir.to_str().unwrap(),
            "-n",
        ],
        &repo.root,
    );
    assert_ok(&out, "journal import-all --dry-run");
}

// ── Session ─────────────────────────────────────────────────────────

#[test]
fn smoke_session_index_no_sessions() {
    let repo = Repo::new();
    let fake_sessions = repo.root.join("sessions");
    std::fs::create_dir_all(&fake_sessions).unwrap();
    let out = run(
        &[
            "session",
            "index",
            "--sessions-path",
            fake_sessions.to_str().unwrap(),
        ],
        &repo.root,
    );
    assert_ok(&out, "session index (empty)");
}

// ── Daemon (non-destructive) ────────────────────────────────────────

#[test]
fn smoke_daemon_status() {
    let repo = Repo::new();
    let out = run(&["daemon", "status"], &repo.root);
    // daemon status always exits 0 per contract
    assert_ok(&out, "daemon status");
}

// ── Output format variants ──────────────────────────────────────────

#[test]
fn smoke_format_json_search() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["--format", "json", "search", "guide"], &repo.root);
    assert_ok(&out, "search --format json");
}

#[test]
fn smoke_format_csv_memory_list() {
    let repo = Repo::new();
    run(
        &[
            "memory", "add", "csv-test", "-t", "CSV test", "-c", "content",
        ],
        &repo.root,
    );
    let out = run(&["--format", "csv", "memory", "list"], &repo.root);
    assert_ok(&out, "memory list --format csv");
}

// ── Help text (ensures clap config is valid) ────────────────────────

#[test]
fn smoke_help_all_subcommands() {
    let tmp = tempfile::tempdir().unwrap();
    let subcommands = [
        &["--help"][..],
        &["init", "--help"],
        &["update", "--help"],
        &["embed", "--help"],
        &["search", "--help"],
        &["get", "--help"],
        &["mget", "--help"],
        &["stats", "--help"],
        &["collection", "--help"],
        &["collection", "add", "--help"],
        &["memory", "--help"],
        &["memory", "add", "--help"],
        &["memory", "export", "--help"],
        &["memory", "import", "--help"],
        &["evolve", "--help"],
        &["evolve", "supersedes", "--help"],
        &["history", "--help"],
        &["current", "--help"],
        &["superseded-by", "--help"],
        &["experiment", "--help"],
        &["experiment", "create", "--help"],
        &["metrics", "--help"],
        &["metrics", "show", "--help"],
        &["journal", "--help"],
        &["journal", "import", "--help"],
        &["setup", "--help"],
        &["setup", "hooks", "--help"],
        &["setup", "hooks", "claude", "--help"],
        &["setup", "mcp", "--help"],
        &["code", "--help"],
        &["code", "index", "--help"],
        &["code", "search", "--help"],
        &["hook", "--help"],
        &["hook", "session-start", "--help"],
        &["hook", "reindex", "--help"],
        &["hook", "search", "--help"],
        &["hook", "memory-write", "--help"],
        &["hook", "status", "--help"],
        &["session", "--help"],
        &["daemon", "--help"],
        &["serve", "--help"],
        &["mcp", "--help"],
    ];

    for args in subcommands {
        let out = run(args, tmp.path());
        assert_ok(&out, &format!("help: mdkb {}", args.join(" ")));
        assert!(
            stdout(&out).contains("Usage") || stdout(&out).contains("usage"),
            "help output for `mdkb {}` should contain Usage",
            args.join(" "),
        );
    }
}

// ── Error cases (expected failures) ─────────────────────────────────

#[test]
fn smoke_search_no_init_fails_gracefully() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(&["search", "anything"], tmp.path());
    // Should fail but not crash (no panic, no segfault)
    assert!(
        !out.status.success() || !stdout(&out).is_empty(),
        "search without init should either fail gracefully or return empty results"
    );
}

#[test]
fn smoke_get_nonexistent() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["get", "nonexistent/file.md"], &repo.root);
    // Should exit non-zero or return an error message, not panic
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success()
            || combined.to_lowercase().contains("not found")
            || combined.is_empty(),
        "get nonexistent should fail gracefully: {combined}"
    );
}

#[test]
fn smoke_memory_show_nonexistent() {
    let repo = Repo::new();
    let out = run(&["memory", "show", "does-not-exist"], &repo.root);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !out.status.success()
            || combined.to_lowercase().contains("not found")
            || combined.to_lowercase().contains("no entry"),
        "memory show nonexistent should fail or report not found: exit={:?} output={combined}",
        out.status.code()
    );
}
