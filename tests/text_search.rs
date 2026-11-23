//! Literal source search contract (story 031-2b1d).
use std::path::Path;
use tempfile::TempDir;

#[path = "common/cli.rs"]
mod cli;

fn index(root: &Path) {
    let db = mdkb::code::storage::CodeDb::open_or_create(root.join(".mdkb/code.sqlite")).unwrap();
    mdkb::code::indexing::pipeline::index_directory(root, &db, &Default::default()).unwrap();
}

fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    mdkb::cli::handlers::handle_init(dir.path()).unwrap();
    std::fs::write(
        dir.path().join("App.tsx"),
        "export function App() {\n  return <button>Fork from here</button>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("en.json"),
        "{\n  \"fork\": \"Fork from here\"\n}\n",
    )
    .unwrap();
    index(dir.path());
    dir
}

fn search(root: &Path, query: &str, extra: &[&str]) -> std::process::Output {
    cli::command()
        .current_dir(root)
        .args(["search", "--scope", "text", query])
        .args(extra)
        .output()
        .unwrap()
}

fn grep(root: &Path, query: &str, extra: &[&str]) -> std::process::Output {
    cli::command()
        .current_dir(root)
        .args(["code", "grep", "--literals", query])
        .args(extra)
        .output()
        .unwrap()
}

fn text(out: std::process::Output) -> String {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Catches: labels and i18n values cannot be retrieved by symbol-only scopes.
#[test]
fn literal_scope_finds_jsx_label_and_i18n_json_value() {
    let dir = fixture();
    let found = text(search(dir.path(), "Fork from here", &[]));
    assert!(found.contains("App.tsx:2:"), "{found}");
    assert!(found.contains("en.json:2:"), "{found}");
    assert!(found.contains("App"), "enclosing component: {found}");
}

/// Catches: the literal index not being reachable as structured data. Default
/// `search` no longer carries literals; `code grep --literals` does.
#[test]
fn code_grep_literals_returns_the_indexed_literals_as_structured_data() {
    let dir = fixture();
    let found = text(cli::run(
        &[
            "--format",
            "json",
            "code",
            "grep",
            "--literals",
            "Fork from here",
        ],
        dir.path(),
    ));
    let value: serde_json::Value = serde_json::from_str(&found).unwrap();
    assert_eq!(value["hits"].as_array().unwrap().len(), 2, "{found}");
}

/// Catches: regex metacharacters or implicit case folding admit unrelated strings.
#[test]
fn literals_are_fixed_strings_with_opt_in_unicode_case_folding() {
    let dir = fixture();
    std::fs::write(
        dir.path().join("extra.ts"),
        "const a = 'Open [draft].*';\nconst b = 'Open XdraftYzz';\nconst c = 'Éclair menu';\n",
    )
    .unwrap();
    index(dir.path());
    let found = text(grep(dir.path(), "Open [draft].*", &[]));
    assert!(found.contains("extra.ts:1:"), "{found}");
    assert!(!found.contains("extra.ts:2:"), "{found}");
    assert!(!text(grep(dir.path(), "fork from here", &[])).contains("App.tsx:"));
    assert!(text(grep(dir.path(), "fork from here", &["-i"])).contains("App.tsx:2:"));
    assert!(text(grep(dir.path(), "éclair", &["-i"])).contains("extra.ts:3:"));
}

/// Catches: stale literal rows survive targeted reindex or file deletion.
#[test]
fn literal_rows_follow_incremental_file_replacement_and_deletion() {
    let dir = fixture();
    let file = dir.path().join("App.tsx");
    let db = mdkb::code::storage::CodeDb::open(dir.path().join(".mdkb/code.sqlite")).unwrap();
    db.delete_by_file("App.tsx").unwrap();
    std::fs::write(
        &file,
        "export function App() { return <button>Resume from here</button>; }\n",
    )
    .unwrap();
    mdkb::code::indexing::pipeline::index_files(&[file], dir.path(), &db, &Default::default())
        .unwrap();
    drop(db);
    assert!(!text(search(dir.path(), "Fork from here", &[])).contains("App.tsx:"));
    assert!(text(search(dir.path(), "Resume from here", &[])).contains("App.tsx:1:"));
    let db = mdkb::code::storage::CodeDb::open(dir.path().join(".mdkb/code.sqlite")).unwrap();
    db.delete_by_file("App.tsx").unwrap();
    drop(db);
    assert!(!text(search(dir.path(), "Resume from here", &[])).contains("App.tsx:"));
}

/// Catches: scanning comments, JSON keys, ignored files or generated bundles bloats results.
#[test]
fn literal_index_excludes_non_values_ignored_and_generated_content() {
    let dir = fixture();
    std::fs::write(dir.path().join(".gitignore"), "ignored.ts\n").unwrap();
    std::fs::write(
        dir.path().join("ignored.ts"),
        "const x = 'Forbidden needle';\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("comments.ts"),
        "// Forbidden needle\nconst x = 'ok';\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("keys.json"),
        "{\"Forbidden needle\": \"ok\"}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("generated.ts"),
        "// @generated\nconst x = 'Forbidden needle';\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("bundle.min.js"),
        "const x = 'Forbidden needle';\n",
    )
    .unwrap();
    index(dir.path());
    let found = text(search(dir.path(), "Forbidden needle", &[]));
    assert!(!found.contains(":1:"), "{found}");
    assert!(!found.contains("generated.ts:"), "{found}");
}

/// Catches: limits are silently ignored or --root searches the caller's store.
#[test]
fn literal_search_honors_limit_and_explicit_root() {
    let dir = fixture();
    let outside = TempDir::new().unwrap();
    let found = text(cli::run(
        &[
            "--format",
            "json",
            "search",
            "--scope",
            "text",
            "Fork from here",
            "--limit",
            "1",
            "--root",
            dir.path().to_str().unwrap(),
        ],
        outside.path(),
    ));
    let value: serde_json::Value = serde_json::from_str(&found).unwrap();
    assert_eq!(value["hits"].as_array().unwrap().len(), 1, "{found}");
    assert_eq!(value["truncated"], true);
    assert!(
        value["indexed_at"].is_number(),
        "index age must be visible: {found}"
    );
    let empty = text(search(
        dir.path(),
        "Fork",
        &["--limit", "0", "--format", "json"],
    ));
    let value: serde_json::Value = serde_json::from_str(&empty).unwrap();
    assert_eq!(value["hits"], serde_json::json!([]));
}

/// Catches: an older read-only code index is migrated or breaks default document search.
#[test]
fn older_literal_free_index_is_read_only_and_requests_a_refresh() {
    let dir = fixture();
    let path = dir.path().join(".mdkb/code.sqlite");
    let db = mdkb::code::storage::CodeDb::open(&path).unwrap();
    db.conn()
        .execute_batch("DROP TABLE code_literals; PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();
    let out = search(dir.path(), "Fork from here", &[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("mdkb update --code"));
    let output = text(cli::run(
        &["--format", "json", "search", "Fork from here"],
        dir.path(),
    ));
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert!(value["text"].is_null());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// Catches: --root comma lists silently omit literal matches from a second repository.
#[test]
fn explicit_roots_search_literals_in_each_repository() {
    let alpha = fixture();
    let beta = fixture();
    let roots = format!("{},{}", alpha.path().display(), beta.path().display());
    let output = text(search(alpha.path(), "Fork from here", &["--root", &roots]));
    assert_eq!(output.matches("App.tsx:2:").count(), 2, "{output}");
    assert!(
        output.contains(
            mdkb::domain::canonicalize_plain(alpha.path())
                .unwrap()
                .to_str()
                .unwrap()
        )
    );
    assert!(
        output.contains(
            mdkb::domain::canonicalize_plain(beta.path())
                .unwrap()
                .to_str()
                .unwrap()
        )
    );
}

/// Catches: a stale sidecar is reported as active, or active work looks idle.
#[test]
fn literal_search_reports_live_background_refresh_and_released_lock() {
    use fs4::fs_std::FileExt;
    let dir = fixture();
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.path().join(".mdkb/code-refresh.lock"))
        .unwrap();
    FileExt::lock_exclusive(&lock).unwrap();
    let active = text(search(dir.path(), "Fork from here", &[]));
    assert!(
        active.contains("Session-start code refresh: in progress."),
        "{active}"
    );
    drop(lock);
    let idle = text(search(dir.path(), "Fork from here", &[]));
    assert!(
        idle.contains("Session-start code refresh: not running."),
        "{idle}"
    );
    assert!(idle.contains("unchanged files retain their age"), "{idle}");
    let json = text(search(dir.path(), "Fork from here", &["--format", "json"]));
    assert!(
        serde_json::from_str::<serde_json::Value>(&json)
            .unwrap()
            .get("background_refresh_active")
            .is_none()
    );
}
