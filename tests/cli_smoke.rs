//! CLI smoke test: exercises every `mdkb` subcommand in an isolated tempdir
//! repo, checking that each exits 0 (or expected non-zero) and produces valid
//! output. Invoke with `cargo test --test cli_smoke`.

#[cfg(unix)]
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

#[cfg(unix)] // only the unix-gated hook/daemon smoke tests call this
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

#[cfg(unix)] // only the unix-gated hook/daemon smoke tests call this
fn assert_hook_output_valid(out: &Output, label: &str) {
    let s = stdout(out);
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .unwrap_or_else(|e| panic!("{label} must return empty or valid JSON, got: {e}"));
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
fn smoke_serve_http_without_token_refused() {
    let repo = Repo::new();
    // A tokenless network server authenticates nothing; starting one must be a
    // hard error (SEC-2). It fails before binding, so the process exits
    // immediately rather than blocking on the accept loop.
    for flag in ["--http", "--https"] {
        let out = run(&["serve", flag], &repo.root);
        assert!(
            !out.status.success(),
            "serve {flag} with no token should exit non-zero"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("--token") && stderr.contains("--allow-no-auth"),
            "serve {flag} error should point to --token/--allow-no-auth: {stderr}"
        );
    }
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
fn smoke_update_force() {
    let repo = Repo::new();
    run(&["update"], &repo.root);
    // --force reindexes already-indexed files (applies config changes).
    let out = run(&["update", "--force"], &repo.root);
    assert_ok(&out, "update --force");
}

/// One run of `update`, one machine-readable document.
///
/// `update` has three phases and each used to render itself, so `--format json`
/// emitted a JSON object, the literal line `Code index:`, another JSON object
/// and then an English sentence about sessions. A human reads that fine; the
/// parser the caller asked for by typing `--format json` cannot read it at all.
/// The fixture seeds both a markdown doc and a source file precisely so more
/// than one phase reports and the concatenation would reappear.
#[test]
fn smoke_update_machine_formats_emit_a_single_document() {
    let repo = Repo::new();

    let out = run(&["--format", "json", "update"], &repo.root);
    assert_ok(&out, "update --format json");
    let json = stdout(&out);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap_or_else(|e| {
        panic!("`update --format json` must be one JSON document ({e}):\n{json}")
    });
    assert!(
        parsed.get("docs").is_some(),
        "the document phase must be reported:\n{json}"
    );
    assert!(
        parsed.get("code").is_some(),
        "the fixture has a source file, so the code phase must be reported \
         inside the same document:\n{json}"
    );

    let out = run(&["--format", "csv", "update", "--force"], &repo.root);
    assert_ok(&out, "update --format csv");
    let csv = stdout(&out);
    let rows: Vec<&str> = csv.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        rows.len(),
        2,
        "csv must be one header and one row, not a table per phase:\n{csv}"
    );
    assert_eq!(
        rows[0].matches(',').count(),
        rows[1].matches(',').count(),
        "the row must have as many fields as the header:\n{csv}"
    );
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

// ── Schema ─────────────────────────────────────────────────────────

/// Unix-only for now: on Windows `mdkb schema` crashes with a main-thread
/// stack overflow (exit 0xC00000FD) — (Windows main-thread stack is 1 MiB vs 
/// 8 MiB on Linux). Tracked for its own fix; ungate when the command survives.
#[cfg(unix)]
#[test]
fn smoke_schema_full() {
    let repo = Repo::new();
    let out = run(&["schema"], &repo.root);
    assert_ok(&out, "schema");
    let s = stdout(&out);
    let v: serde_json::Value =
        serde_json::from_str(s.trim()).unwrap_or_else(|e| panic!("schema json invalid: {e}\n{s}"));
    assert_eq!(v["name"], "mdkb", "root command name");
    assert!(
        v["subcommands"].as_array().is_some_and(|a| !a.is_empty()),
        "schema must list subcommands"
    );
}

/// Unix-only for now: on Windows `mdkb schema` crashes with a main-thread
/// stack overflow (exit 0xC00000FD) (Windows main-thread stack is 1 MiB vs 8 MiB on Linux). 
/// Tracked for its own fix; ungate when the command survives.
#[cfg(unix)]
#[test]
fn smoke_schema_subcommand() {
    let repo = Repo::new();
    let out = run(&["schema", "search"], &repo.root);
    assert_ok(&out, "schema search");
    let s = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(s.trim())
        .unwrap_or_else(|e| panic!("schema search json invalid: {e}\n{s}"));
    assert_eq!(v["name"], "search", "subcommand name");
    let has_query = v["args"]
        .as_array()
        .is_some_and(|args| args.iter().any(|a| a["name"] == "query"));
    assert!(has_query, "search schema must expose the query arg: {s}");
}

#[test]
fn smoke_schema_unknown_command_exits_nonzero() {
    let repo = Repo::new();
    let out = run(&["schema", "no-such-command"], &repo.root);
    assert!(
        !out.status.success(),
        "schema on unknown command should exit non-zero"
    );
}

// ── Compact ────────────────────────────────────────────────────────

#[test]
fn smoke_compact() {
    let repo = Repo::new();
    let out = run(&["compact"], &repo.root);
    assert_ok(&out, "compact");
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

    // confirm (+1) — reachable in-process, no daemon.
    let out = run(
        &[
            "memory",
            "confirm",
            "smoke-test-entry",
            "--outcome",
            "confirmed",
        ],
        &repo.root,
    );
    assert_ok(&out, "memory confirm");
    assert!(
        stdout(&out).contains("Confirmed"),
        "confirm must report success: {}",
        stdout(&out)
    );

    // confirm --format json exposes the new confirmation count.
    let out = run(
        &[
            "--format",
            "json",
            "memory",
            "confirm",
            "smoke-test-entry",
            "--outcome",
            "confirmed",
        ],
        &repo.root,
    );
    assert_ok(&out, "memory confirm --format json");
    let v: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("confirm json");
    assert_eq!(v["confirmations"], 2, "two confirms → count 2");

    // refuted below zero floors at 0 rather than going negative.
    for _ in 0..5 {
        let out = run(
            &[
                "memory",
                "confirm",
                "smoke-test-entry",
                "--outcome",
                "refuted",
            ],
            &repo.root,
        );
        assert_ok(&out, "memory confirm refuted");
    }
    let out = run(
        &[
            "--format",
            "json",
            "memory",
            "confirm",
            "smoke-test-entry",
            "--outcome",
            "refuted",
        ],
        &repo.root,
    );
    assert_ok(&out, "memory confirm refuted floor");
    let v: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("confirm json");
    assert_eq!(v["confirmations"], 0, "confirmations floor at 0");

    // Unknown id is a clean non-zero error, not a panic.
    let out = run(
        &[
            "memory",
            "confirm",
            "no-such-entry",
            "--outcome",
            "confirmed",
        ],
        &repo.root,
    );
    assert!(
        !out.status.success(),
        "confirming an unknown id must fail cleanly"
    );

    // Invalid outcome rejected.
    let out = run(
        &[
            "memory",
            "confirm",
            "smoke-test-entry",
            "--outcome",
            "maybe",
        ],
        &repo.root,
    );
    assert!(!out.status.success(), "invalid outcome must be rejected");

    let out = run(&["memory", "prune", "--dry-run"], &repo.root);
    assert_ok(&out, "memory prune --dry-run");

    let out = run(&["memory", "export", "--dry-run"], &repo.root);
    assert_ok(&out, "memory export --dry-run");

    let out = run(&["memory", "sync"], &repo.root);
    assert_ok(&out, "memory sync");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Conflicts:"),
        "memory sync must report its outcome: {stdout}"
    );

    let out = run(&["memory", "rm", "smoke-test-entry"], &repo.root);
    assert_ok(&out, "memory rm");
}

#[test]
fn smoke_memory_link() {
    let repo = Repo::new();

    for (id, title) in [("link-src", "Source"), ("link-dst", "Dest")] {
        let out = run(
            &["memory", "add", id, "-t", title, "-c", "content"],
            &repo.root,
        );
        assert_ok(&out, "memory add for link");
    }

    // Happy path: source supports dst.
    let out = run(
        &["memory", "link", "link-src", "supports", "link-dst"],
        &repo.root,
    );
    assert_ok(&out, "memory link");

    // --doc + --agent variant.
    let out = run(
        &[
            "memory",
            "link",
            "link-src",
            "derived_from",
            "docs/spec.md",
            "--doc",
            "--agent",
            "scout",
        ],
        &repo.root,
    );
    assert_ok(&out, "memory link --doc --agent");

    // Invalid relation must exit non-zero and list the closed set.
    let out = run(
        &["memory", "link", "link-src", "mentions", "link-dst"],
        &repo.root,
    );
    assert!(
        !out.status.success(),
        "invalid relation must exit non-zero, got stdout: {}",
        stdout(&out)
    );
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

// ── Knowledge graph ─────────────────────────────────────────────────

#[test]
fn smoke_graph() {
    let repo = Repo::new();

    std::fs::write(
        repo.root.join("docs/project.md"),
        "---\nowner: alice\nthemes:\n  - growth\n---\nSee [[guide]] for setup.\n",
    )
    .unwrap();
    run(&["update"], &repo.root);

    // links: by path, text and json.
    let links = run(&["graph", "links", "project.md"], &repo.root);
    assert_ok(&links, "graph links");
    // Human-readable endpoints: the source shows its path, never a numeric id.
    let links_out = stdout(&links);
    assert!(
        links_out.contains("project.md --"),
        "edge source must be the doc path, got: {links_out}"
    );
    assert!(
        !links_out.contains("[1]") && !links_out.contains("[2]"),
        "edge output must not leak numeric doc ids, got: {links_out}"
    );
    let out = run(
        &["--format", "json", "graph", "links", "project.md"],
        &repo.root,
    );
    assert_ok(&out, "graph links json");
    serde_json::from_str::<serde_json::Value>(stdout(&out).trim()).expect("links json valid");

    // links by document id (resolve_document_id accepts numeric ids).
    assert_ok(
        &run(&["graph", "links", "1"], &repo.root),
        "graph links by id",
    );

    // backlinks: by raw slug (dangling 'alice') and relation filter.
    assert_ok(
        &run(&["graph", "backlinks", "alice"], &repo.root),
        "graph backlinks",
    );
    assert_ok(
        &run(&["graph", "backlinks", "alice", "-r", "owner"], &repo.root),
        "graph backlinks --relation",
    );

    // neighbors: text and json, with depth. Output must carry the relation (via).
    let nbrs = run(
        &["graph", "neighbors", "project.md", "--depth", "2"],
        &repo.root,
    );
    assert_ok(&nbrs, "graph neighbors");
    assert!(
        stdout(&nbrs).contains("via"),
        "neighbors must report the connecting relation, got: {}",
        stdout(&nbrs)
    );
    let out = run(
        &["--format", "json", "graph", "neighbors", "project.md"],
        &repo.root,
    );
    assert_ok(&out, "graph neighbors json");
    serde_json::from_str::<serde_json::Value>(stdout(&out).trim()).expect("neighbors json valid");

    // path: project -> guide (the [[guide]] wikilink resolves to guide.md).
    assert_ok(
        &run(&["graph", "path", "project.md", "guide.md"], &repo.root),
        "graph path",
    );

    // Regression: a numeric document id used as the *target* must resolve like
    // the start argument does, not be treated as a literal slug. Make the owner
    // edge point at a real document so a path exists, then address it by id.
    std::fs::write(
        repo.root.join("docs/alice.md"),
        "---\ntitle: Alice\n---\nOwner.\n",
    )
    .unwrap();
    run(&["update"], &repo.root);

    // Sanity: path to the owner by path-form target is found (project -> alice).
    let by_path = run(&["graph", "path", "project.md", "alice.md"], &repo.root);
    assert_ok(&by_path, "graph path by target path");
    assert!(
        stdout(&by_path).contains("->"),
        "expected project.md -> alice.md, got: {}",
        stdout(&by_path)
    );

    // The two docs have ids 1 and 2; exactly one is alice.md and reachable via
    // the owner edge. Before the fix BOTH numeric targets yielded "No path
    // found" because the target was matched as a literal slug, never an id.
    let by_id_1 = stdout(&run(&["graph", "path", "project.md", "1"], &repo.root));
    let by_id_2 = stdout(&run(&["graph", "path", "project.md", "2"], &repo.root));
    assert!(
        by_id_1.contains("->") || by_id_2.contains("->"),
        "numeric-id path target must resolve to a document; id1={by_id_1:?} id2={by_id_2:?}"
    );

    // Bare-slug parity: links/neighbors/path must accept a slug without the .md
    // extension, exactly as backlinks does. Before the fix these errored with
    // DocumentNotFound while `backlinks alice` succeeded.
    assert_ok(
        &run(&["graph", "links", "project"], &repo.root),
        "graph links by bare slug",
    );
    assert_ok(
        &run(&["graph", "neighbors", "project"], &repo.root),
        "graph neighbors by bare slug",
    );
    let by_slug = run(&["graph", "path", "project", "alice"], &repo.root);
    assert_ok(&by_slug, "graph path by bare slugs");
    assert!(
        stdout(&by_slug).contains("->"),
        "expected project -> alice via bare slugs, got: {}",
        stdout(&by_slug)
    );
}

#[test]
fn smoke_graph_dangling_and_hubs() {
    let repo = Repo::new();
    std::fs::write(
        repo.root.join("docs/project.md"),
        "---\nowner: alice\nrelated:\n  - teams/wiz\n---\nbody\n",
    )
    .unwrap();
    run(&["update"], &repo.root);

    // dangling: teams/wiz and alice resolve to no document → both reported.
    let dangling = run(&["graph", "dangling"], &repo.root);
    assert_ok(&dangling, "graph dangling");
    assert!(
        stdout(&dangling).contains("teams/wiz"),
        "dangling must list the unresolved ref, got: {}",
        stdout(&dangling)
    );
    // json shape stable.
    let dj = run(&["--format", "json", "graph", "dangling"], &repo.root);
    assert_ok(&dj, "graph dangling json");
    serde_json::from_str::<serde_json::Value>(stdout(&dj).trim()).expect("dangling json valid");

    // hubs: project.md is the source of the edges → appears with out-degree.
    let hubs = run(&["graph", "hubs", "--limit", "5"], &repo.root);
    assert_ok(&hubs, "graph hubs");
    assert!(
        stdout(&hubs).contains("project.md"),
        "hubs must rank the linking doc, got: {}",
        stdout(&hubs)
    );
    let hj = run(&["--format", "json", "graph", "hubs"], &repo.root);
    assert_ok(&hj, "graph hubs json");
    serde_json::from_str::<serde_json::Value>(stdout(&hj).trim()).expect("hubs json valid");
}

#[test]
fn smoke_collection_list() {
    let repo = Repo::new();
    run(&["update"], &repo.root);

    let out = run(&["collection", "list"], &repo.root);
    assert_ok(&out, "collection list");
    assert!(
        stdout(&out).contains("docs"),
        "collection list must show the docs collection, got: {}",
        stdout(&out)
    );

    let json = run(&["--format", "json", "collection", "list"], &repo.root);
    assert_ok(&json, "collection list json");
    let v: serde_json::Value =
        serde_json::from_str(stdout(&json).trim()).expect("collection list json valid");
    assert!(v.is_array(), "collection list json is an array");
}

#[test]
fn smoke_graph_collection_prefixed_ref() {
    let repo = Repo::new();
    std::fs::write(
        repo.root.join("docs/project.md"),
        "---\nowner: alice\n---\nbody\n",
    )
    .unwrap();
    run(&["update"], &repo.root);

    // The docs collection lives at ./docs — a collection-prefixed reference
    // (docs/project.md) must resolve like the bare path (project.md).
    let prefixed = run(&["graph", "links", "docs/project.md"], &repo.root);
    assert_ok(&prefixed, "graph links with collection-prefixed ref");

    // A truly unresolvable ref lists the accepted forms it tried.
    let missing = run(&["graph", "links", "nope/missing.md"], &repo.root);
    assert!(
        !missing.status.success(),
        "unresolvable ref must fail nonzero"
    );
    let err = String::from_utf8_lossy(&missing.stderr);
    assert!(
        err.contains("tried:"),
        "NotFound must enumerate tried forms, got: {err}"
    );
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

#[test]
fn smoke_code_find_caps_output_and_reports_total() {
    let repo = Repo::new();

    // A boilerplate name matches once per file. An uncapped list is what makes
    // `search --scope symbols tests` dump hundreds of lines into a context
    // window, so the cap must hold — and the dropped matches must still be
    // reported, or a capped list reads as the complete set.
    for n in 0..5 {
        std::fs::write(
            repo.root.join(format!("src/mod{n}.rs")),
            "#[cfg(test)]\nmod tests {\n    fn case() {}\n}\n",
        )
        .unwrap();
    }
    run(&["code", "init"], &repo.root);
    assert_ok(&run(&["code", "index", "src/"], &repo.root), "code index");

    let out = run(&["code", "find", "tests", "--limit", "2"], &repo.root);
    assert_ok(&out, "code find --limit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("sym#").count(),
        2,
        "--limit 2 must print 2 symbols, got: {stdout}"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Showing 2 of 5"),
        "truncation must name the total, got: {stderr}"
    );

    // `search --scope symbols` shares the handler, and used to drop --limit.
    let out = run(
        &["search", "tests", "--scope", "symbols", "--limit", "3"],
        &repo.root,
    );
    assert_ok(&out, "search --scope symbols --limit");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("sym#").count(),
        3,
        "search --scope symbols must honour --limit, got: {stdout}"
    );

    // Nothing dropped, nothing to report.
    let out = run(&["code", "find", "tests", "--limit", "10"], &repo.root);
    assert_ok(&out, "code find under the cap");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Showing"),
        "no truncation notice when nothing was dropped, got: {stderr}"
    );
}

#[test]
fn smoke_kind_filter_fills_the_limit() {
    let repo = Repo::new();

    // 30 functions and 3 structs all match "probe". Filtering after a capped
    // fetch would read the first few rows — nearly all functions — and return
    // fewer than 3 structs, or none at all. The filter has to run before the
    // cap, so a kind filter still fills the requested limit.
    let mut source = String::new();
    for n in 0..30 {
        source.push_str(&format!("pub fn probe_fn_{n}() {{}}\n"));
    }
    for n in 0..3 {
        source.push_str(&format!("pub struct probe_st_{n};\n"));
    }
    std::fs::write(repo.root.join("src/probes.rs"), source).unwrap();

    run(&["code", "init"], &repo.root);
    assert_ok(&run(&["code", "index", "src/"], &repo.root), "code index");

    let out = run(
        &[
            "code", "search", "probe", "--kind", "struct", "--limit", "3",
        ],
        &repo.root,
    );
    assert_ok(&out, "code search --kind");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("sym#").count(),
        3,
        "kind filter must fill the limit, got: {stdout}"
    );
    assert!(
        !stdout.contains("Function"),
        "kind=struct must exclude functions, got: {stdout}"
    );
}

#[test]
fn smoke_search_scope_symbols_is_fuzzy() {
    let repo = Repo::new();
    run(&["code", "init"], &repo.root);
    assert_ok(&run(&["code", "index", "src/"], &repo.root), "code index");

    // The MCP server answers scope=symbols with a substring match. The CLI used
    // to answer the same scope with exact name equality, so an agent got
    // different results from the same query depending on the surface it used.
    let out = run(&["search", "gree", "--scope", "symbols"], &repo.root);
    assert_ok(&out, "search --scope symbols partial name");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("greet"),
        "scope=symbols must match substrings like the MCP server does, got: {stdout}"
    );
}

#[test]
fn smoke_search_scope_code_is_semantic() {
    let repo = Repo::new();
    run(&["code", "init"], &repo.root);
    assert_ok(&run(&["code", "index", "src/"], &repo.root), "code index");

    // `--scope code` used to run the same substring search as `--scope symbols`
    // while the help promised semantic search. Disabling semantic search is the
    // cheap proof it now takes the semantic path: a substring search would
    // happily return `greet` and ignore the setting.
    std::fs::write(
        repo.root.join(".mdkb/config.toml"),
        "[code.semantic_search]\nenabled = false\n",
    )
    .unwrap();

    let out = run(&["search", "greet", "--scope", "code"], &repo.root);
    assert!(
        !out.status.success(),
        "disabled semantic search must fail, not fall back to substring search"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("Semantic code search is disabled"),
        "error must name the disabled setting, got: {stderr}"
    );
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

/// Unix-only: the command under test refuses on other platforms by design
/// ("Daemon commands require Unix" / "Hook commands require Unix domain
/// sockets"), so a passing smoke run is impossible there.
#[cfg(unix)]
#[test]
fn smoke_hook_session_start() {
    let repo = Repo::new();
    let out = run_stdin(&["hook", "session-start"], &repo.root, "{}");
    assert_ok(&out, "hook session-start");
    assert_hook_output_valid(&out, "hook session-start");
}

/// Unix-only: refuses off-Unix by design — see the note on
/// `smoke_hook_session_start`.
#[cfg(unix)]
#[test]
fn smoke_hook_user_prompt_submit() {
    let repo = Repo::new();
    let payload = r#"{"prompt":"test prompt"}"#;
    let out = run_stdin(&["hook", "user-prompt-submit"], &repo.root, payload);
    assert_ok(&out, "hook user-prompt-submit");
    assert_hook_output_valid(&out, "hook user-prompt-submit");
}

/// Unix-only: refuses off-Unix by design — see the note on
/// `smoke_hook_session_start`.
#[cfg(unix)]
#[test]
fn smoke_hook_post_tool_use() {
    let repo = Repo::new();
    let payload = r#"{"tool_name":"Read","tool_input":{"file_path":"src/lib.rs"}}"#;
    let out = run_stdin(&["hook", "post-tool-use"], &repo.root, payload);
    assert_ok(&out, "hook post-tool-use");
    assert_hook_output_valid(&out, "hook post-tool-use");
}

/// Unix-only: refuses off-Unix by design — see the note on
/// `smoke_hook_session_start`.
#[cfg(unix)]
#[test]
fn smoke_hook_stop() {
    let repo = Repo::new();
    // Mining is off by default → the hook is silent and exits clean.
    let payload = r#"{"transcript_path":"/nonexistent","session_id":"s1"}"#;
    let out = run_stdin(&["hook", "stop"], &repo.root, payload);
    assert_ok(&out, "hook stop");
    assert_hook_output_valid(&out, "hook stop");
}

/// Unix-only: refuses off-Unix by design — see the note on
/// `smoke_hook_session_start`.
#[cfg(unix)]
#[test]
fn smoke_hook_events_tolerate_empty_stdin() {
    let repo = Repo::new();
    for event in &[
        "session-start",
        "user-prompt-submit",
        "post-tool-use",
        "stop",
    ] {
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
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("Codex CLI is not installed") {
        return;
    }
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

/// Unix-only: refuses off-Unix by design — see the note on
/// `smoke_hook_session_start`.
#[cfg(unix)]
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
        &["memory", "link", "--help"],
        &["memory", "export", "--help"],
        &["memory", "import", "--help"],
        &["evolve", "--help"],
        &["evolve", "supersedes", "--help"],
        &["history", "--help"],
        &["current", "--help"],
        &["superseded-by", "--help"],
        &["graph", "--help"],
        &["graph", "links", "--help"],
        &["graph", "neighbors", "--help"],
        &["graph", "path", "--help"],
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
        &["compact", "--help"],
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
fn smoke_get_comma_all_fail_exits_nonzero() {
    // BUG-E1: `get a,b` with per-id errors must exit non-zero, not 0.
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["get", "nope1.md,nope2.md"], &repo.root);
    assert!(
        !out.status.success(),
        "comma-separated get where every id fails must exit non-zero"
    );
}

#[test]
fn smoke_get_comma_partial_success_exits_nonzero() {
    // A valid id mixed with a bad one still fails overall (the bad id errored).
    let repo = Repo::new();
    run(&["update"], &repo.root);
    let out = run(&["get", "guide.md,does-not-exist.md"], &repo.root);
    assert!(
        !out.status.success(),
        "batch get must exit non-zero if any id fails, even with a partial success"
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

#[test]
fn smoke_format_json_error_is_json() {
    let tmp = tempfile::tempdir().unwrap();
    // search without init should error; with --format json the error should be JSON on stderr
    let out = run(&["--format", "json", "search", "anything"], tmp.path());
    assert!(!out.status.success(), "should fail without init");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr should be JSON: {e}\nstderr: {stderr}"));
    assert!(
        parsed.get("error").is_some(),
        "JSON error should have 'error' key: {parsed}"
    );
}
