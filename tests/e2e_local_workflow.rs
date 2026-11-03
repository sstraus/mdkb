//! Consumer workflow against the real CLI and its persisted indexes.
//!
//! The local workflow provisions the ONNX model before running this target.

#[path = "common/cli.rs"]
mod cli;

use std::fs;
use std::path::Path;
use std::process::Output;

fn run(root: &Path, args: &[&str]) -> String {
    let out: Output = cli::run(args, root);
    assert!(
        out.status.success(),
        "mdkb {} failed: stdout={} stderr={}",
        args.join(" "),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 CLI output")
}

fn count(conn: &rusqlite::Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get(0)).expect(sql)
}

/// Catches: an update reports success while stale document or vector rows survive a deletion.
#[test]
fn cli_update_replaces_and_deletes_searchable_documents_and_vectors() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("docs")).unwrap();
    fs::write(
        root.join("docs/guide.md"),
        "# Solar navigation\n\nUse the sun to navigate.\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/obsolete.md"),
        "# Discontinued product\n\nobsoletequartzmarker\n",
    )
    .unwrap();

    run(root, &["init"]);
    run(
        root,
        &["collection", "add", "docs", "docs", "--pattern", "**/*.md"],
    );
    run(root, &["update"]);
    mdkb::store::vectors::init_sqlite_vec();
    let conn = rusqlite::Connection::open(root.join(".mdkb/index.sqlite")).unwrap();
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents"), 2);
    assert_eq!(
        count(&conn, "SELECT COUNT(*) FROM embeddings"),
        2,
        "update must write document vectors"
    );
    assert!(
        run(root, &["search", "obsoletequartzmarker", "--scope", "docs"]).contains("obsolete.md")
    );

    // Force a distinct file timestamp on file systems with coarse mtime precision.
    std::thread::sleep(std::time::Duration::from_secs(1));
    fs::write(
        root.join("docs/guide.md"),
        "# Lunar navigation\n\nUse the moon to navigate.\n",
    )
    .unwrap();
    fs::remove_file(root.join("docs/obsolete.md")).unwrap();
    run(root, &["update"]);

    assert!(run(root, &["search", "lunar navigation", "--scope", "docs"]).contains("guide.md"));
    let current = run(root, &["get", "docs/guide.md"]);
    assert!(
        current.contains("Lunar navigation"),
        "get must return the new content: {current}"
    );
    assert!(
        !current.contains("Solar navigation"),
        "get must not return stale content: {current}"
    );
    assert!(
        !run(root, &["search", "obsoletequartzmarker", "--scope", "docs"]).contains("obsolete.md")
    );
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM documents"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM embeddings"), 1);
    assert_eq!(count(&conn, "SELECT COUNT(*) FROM vec_documents"), 1);
}

/// Catches: update refreshes documents but leaves removed or renamed code symbols searchable.
#[test]
fn cli_update_keeps_code_search_in_sync_with_source() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "/// Say hello to a new visitor.\npub fn greet_visitor() -> &'static str { \"welcome\" }\n\
         /// Add the prices on an invoice.\npub fn calculate_invoice() -> u64 { 42 }\n",
    )
    .unwrap();
    run(root, &["init"]);
    run(root, &["update"]);
    assert!(run(root, &["code", "find", "greet_visitor"]).contains("greet_visitor"));
    assert!(
        run(
            root,
            &[
                "search",
                "send a welcome message",
                "--scope",
                "code",
                "--limit",
                "1"
            ]
        )
        .contains("greet_visitor"),
        "semantic code search must rank the greeting above the invoice calculation"
    );

    std::thread::sleep(std::time::Duration::from_secs(1));
    fs::write(
        root.join("src/lib.rs"),
        "pub fn farewell_visitor() -> &'static str { \"goodbye\" }\n",
    )
    .unwrap();
    run(root, &["update"]);
    assert!(run(root, &["code", "find", "farewell_visitor"]).contains("farewell_visitor"));
    assert!(!run(root, &["code", "find", "greet_visitor"]).contains("sym#"));
    fs::remove_file(root.join("src/lib.rs")).unwrap();
    run(root, &["update"]);
    assert!(!run(root, &["code", "find", "farewell_visitor"]).contains("sym#"));
}
