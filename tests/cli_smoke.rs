//! CLI smoke test: exercises every `mdkb` subcommand in an isolated tempdir
//! repo, checking that each exits 0 (or expected non-zero) and produces valid
//! output. Invoke with `cargo test --test cli_smoke`.
//!
//! Every spawn goes through `cli::command()`, which gives the binary a
//! throwaway `HOME` and `MDKB_NO_DAEMON` (story 067-5ab6). Until then this
//! suite sent 78 mutation and hook requests per run to the developer's real
//! daemon socket, and would have spawned a daemon under the developer's
//! account when none was listening. The one test that needs a daemon,
//! `smoke_memory_add_routed_through_a_daemon_renders_like_the_direct_path`,
//! starts its own under its own `HOME`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

#[path = "common/cli.rs"]
mod cli;
use cli::run;

fn run_env(args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Output {
    cli::command()
        .args(args)
        .envs(env.iter().copied())
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("spawn failed for `mdkb {}`: {e}", args.join(" ")))
}

fn run_stdin(args: &[&str], cwd: &Path, stdin: &str) -> Output {
    let mut child = cli::command()
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
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

// ── Namespaces ──────────────────────────────────────────────────────

/// Write one entry under `env`, then prove the default store never sees it
/// while the namespaced store does. This is the whole contract: a consumer's
/// test suite cannot pollute the store its sessions warm up from.
fn assert_namespaced_write_is_invisible_to_the_default_store(env: &[(&str, &str)], id: &str) {
    let repo = Repo::new();
    let title = format!("Namespaced {id}");

    let out = run_env(
        &["memory", "add", id, "-t", &title, "-c", "throwaway"],
        &repo.root,
        env,
    );
    assert_ok(&out, "namespaced memory add");

    // The write landed in its own store, under the project's .mdkb/.
    assert!(
        repo.root
            .join(".mdkb/namespaces/test/index.sqlite")
            .is_file(),
        "namespaced write must create .mdkb/namespaces/test/index.sqlite"
    );

    // The default store is untouched: not listed, not warmed up, not shown.
    for args in [
        ["memory", "list", "--format", "json"].as_slice(),
        ["memory", "warmup"].as_slice(),
    ] {
        let out = run(args, &repo.root);
        assert_ok(&out, &format!("default-store `{}`", args.join(" ")));
        assert!(
            !stdout(&out).contains(id),
            "default-store `{}` must not surface {id}: {}",
            args.join(" "),
            stdout(&out)
        );
    }
    let out = run(&["memory", "show", id], &repo.root);
    assert!(
        !out.status.success(),
        "default-store `memory show {id}` must not find a namespaced entry"
    );
    let out = run_env(
        &["hook", "session-start"],
        &repo.root,
        &[("MDKB_NO_DAEMON", "1")],
    );
    assert_hook_output_valid(&out, "session-start");
    assert!(
        !stdout(&out).contains(id),
        "SessionStart warmup must not surface a namespaced entry: {}",
        stdout(&out)
    );

    // Reads under the same namespace see the entry, so the consumer's own
    // round-trip test still passes.
    let out = run_env(&["memory", "show", id], &repo.root, env);
    assert_ok(&out, "namespaced memory show");
    assert!(stdout(&out).contains(&title));
}

#[test]
fn smoke_explicit_namespace_isolates_writes_from_the_default_store() {
    assert_namespaced_write_is_invisible_to_the_default_store(
        &[("MDKB_NAMESPACE", "test")],
        "explicit-namespace-entry",
    );
}

/// A consumer test suite does not opt in. `node --test` (the runner the wiz
/// bridge tests use) marks its children with NODE_TEST_CONTEXT; mdkb reads that
/// and routes the write to the test namespace on its own.
#[test]
fn smoke_test_runner_environment_selects_the_test_namespace_unasked() {
    assert_namespaced_write_is_invisible_to_the_default_store(
        &[("NODE_TEST_CONTEXT", "child-v8")],
        "auto-namespace-entry",
    );
}

#[test]
fn smoke_namespace_name_is_validated() {
    let repo = Repo::new();
    let out = run_env(
        &["memory", "add", "x", "-t", "x", "-c", "x"],
        &repo.root,
        &[("MDKB_NAMESPACE", "../escape")],
    );
    assert!(
        !out.status.success(),
        "a namespace that walks out of .mdkb/namespaces must be refused"
    );
    assert!(
        !repo.root.join(".mdkb/escape").exists() && !repo.root.join("escape").exists(),
        "a refused namespace must create nothing"
    );
}

/// Hook telemetry is a write. It goes to the store the hook ran against, not
/// to the default store — nothing escapes a namespace, however low the stakes.
#[test]
fn smoke_namespaced_hook_logs_telemetry_into_its_own_store() {
    let repo = Repo::new();
    let out = run_env(
        &["hook", "session-start"],
        &repo.root,
        &[("MDKB_NO_DAEMON", "1"), ("MDKB_NAMESPACE", "test")],
    );
    assert_ok(&out, "namespaced hook session-start");
    assert!(
        repo.root
            .join(".mdkb/namespaces/test/hook-events.jsonl")
            .is_file(),
        "telemetry must land in the namespaced store"
    );
    assert!(
        !repo.root.join(".mdkb/hook-events.jsonl").exists(),
        "the default store must not receive a namespaced hook's telemetry"
    );
}

/// The quarantine banner reports the store the session actually opened. A
/// namespaced session announcing the DEFAULT store's corruption is worse than
/// saying nothing: the operator would go looking in the wrong place.
#[test]
fn smoke_quarantine_banner_reports_the_active_store_only() {
    let repo = Repo::new();
    let ns_env = [("MDKB_NO_DAEMON", "1"), ("MDKB_NAMESPACE", "test")];
    // Create the namespaced store, then plant a quarantine marker in the
    // DEFAULT store only.
    assert_ok(
        &run_env(&["hook", "session-start"], &repo.root, &ns_env),
        "namespaced hook (creates the store)",
    );
    std::fs::write(repo.root.join(".mdkb/index.sqlite.corrupt-1700000000"), b"").unwrap();

    // Control: the default store reports its own quarantine.
    let out = run_env(
        &["hook", "session-start"],
        &repo.root,
        &[("MDKB_NO_DAEMON", "1")],
    );
    assert_ok(&out, "default hook session-start");
    assert!(
        stdout(&out).contains("CORRUPT"),
        "control: the default store must report its quarantine: {}",
        stdout(&out)
    );

    // The namespaced session must not.
    let out = run_env(&["hook", "session-start"], &repo.root, &ns_env);
    assert_ok(&out, "namespaced hook session-start");
    assert!(
        !stdout(&out).contains("CORRUPT"),
        "a namespaced session must not report the default store's quarantine: {}",
        stdout(&out)
    );

    // And it does report its own.
    std::fs::write(
        repo.root
            .join(".mdkb/namespaces/test/index.sqlite.corrupt-1700000001"),
        b"",
    )
    .unwrap();
    let out = run_env(&["hook", "session-start"], &repo.root, &ns_env);
    assert_ok(&out, "namespaced hook session-start with own quarantine");
    assert!(
        stdout(&out).contains("CORRUPT"),
        "a namespaced session must report its own quarantine: {}",
        stdout(&out)
    );
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

/// `mdkb dup` before anyone ran `mdkb code index`.
///
/// `mdkb init` creates an empty `code.sqlite`, so this is the state a fresh
/// repository is really in — the audit must say the index is missing and exit
/// 0, because an audit with nothing to audit is not a failure.
#[test]
fn smoke_dup_without_a_code_index() {
    let repo = Repo::new();
    let out = run(&["dup"], &repo.root);
    assert_ok(&out, "dup without a code index");
    let text = stdout(&out);
    assert!(text.contains("No code index"), "dup said: {text}");
    assert!(
        text.contains("mdkb code index"),
        "and must say what to run: {text}"
    );
}

/// Review mode's one failure that must never be silent.
///
/// An empty duplication report reads as "your change duplicated nothing". A
/// ref git cannot resolve must therefore be an error, not an empty report —
/// otherwise a typo in the ref is indistinguishable from a clean review.
///
/// The repository is indexed first on purpose: without an index `dup` reports
/// that instead, and this test would pass for the wrong reason.
#[test]
fn smoke_dup_since_an_unknown_ref_fails_rather_than_reporting_nothing() {
    let repo = Repo::new();
    assert_ok(&run(&["code", "index", "src"], &repo.root), "code index");

    let out = run(&["dup", "--since", "no-such-ref-anywhere"], &repo.root);

    assert!(
        !out.status.success(),
        "an unresolvable ref must not report an empty audit: {}",
        stdout(&out)
    );
}

/// A ref beginning with `-` never reaches git.
///
/// On this surface clap refuses it first, which is the outer of two guards;
/// the inner one — `reject_option_like_ref`, which protects the MCP path where
/// no argument parser is involved — is pinned by the unit tests in `git.rs`.
#[test]
fn smoke_dup_since_rejects_an_option_like_ref() {
    let repo = Repo::new();
    let out = run(&["dup", "--since", "--upload-pack=evil"], &repo.root);
    assert!(!out.status.success(), "must refuse: {}", stdout(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--upload-pack"),
        "and must name what it refused: {stderr}"
    );
}

/// The semantic pass is opt-in, and both switches that turn it on reach the
/// handler.
///
/// `mdkb dup` used to load a 160M-parameter model on every run: measured on
/// this repository the semantic half took 817 s of an 818 s run. It now runs
/// only when `--semantic` or a `--threshold` override asks for it.
///
/// The model is made unreachable on purpose — an empty cache directory plus an
/// endpoint on a closed port. A run that asks for the semantic pass then warns
/// and degrades to the structural half; a run that does not ask stays silent.
/// That warning is what separates the three cases here without downloading
/// weights or reaching the network.
#[test]
fn smoke_dup_semantic_pass_is_opt_in() {
    let repo = Repo::new();
    assert_ok(&run(&["code", "index", "src"], &repo.root), "code index");

    let cache_dir = repo.root.join("fastembed-empty");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let cache = cache_dir.to_str().unwrap();
    let offline = [
        ("FASTEMBED_CACHE_DIR", cache),
        // fastembed prefers HF_HOME over the cache directory it is handed, so
        // a developer with one exported would otherwise hit their real cache.
        ("HF_HOME", cache),
        ("HF_ENDPOINT", "http://127.0.0.1:1"),
    ];
    const REACHED_FOR_A_MODEL: &str = "duplication model unavailable";

    let plain = run_env(&["dup"], &repo.root, &offline);
    assert_ok(&plain, "dup");
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(
        !stderr.contains(REACHED_FOR_A_MODEL),
        "a default run must not reach for a model: {stderr}"
    );

    for args in [
        ["dup", "--semantic"].as_slice(),
        ["dup", "--threshold", "0.8"].as_slice(),
    ] {
        let label = args.join(" ");
        let out = run_env(args, &repo.root, &offline);
        assert_ok(&out, &label);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(REACHED_FOR_A_MODEL),
            "`mdkb {label}` must ask for the semantic pass: {stderr}"
        );
        let text = stdout(&out);
        assert!(
            text.starts_with("# Duplication"),
            "and must still report: {text}"
        );
    }
}

/// `mdkb coupling` before anyone ran `mdkb code index`.
///
/// Same contract as `dup`: nothing to correlate is not a failure. The tempdir
/// is not a git repository either, so this also pins that a missing history
/// exits 0 rather than surfacing git's own error.
#[test]
fn smoke_coupling_without_a_code_index() {
    let repo = Repo::new();
    let out = run(&["coupling"], &repo.root);
    assert_ok(&out, "coupling without a code index");
    let text = stdout(&out);
    assert!(text.contains("Hidden Coupling"), "coupling said: {text}");
    assert!(
        text.contains("No code index") || text.contains("No git history"),
        "and must name what is missing: {text}"
    );
}

/// The three overrides must reach the handler, not just parse. A bad `--ref`
/// is the one that proves it: the default path never names a revision, so an
/// unreachable one can only fail if the flag was actually threaded through.
#[test]
fn smoke_coupling_accepts_its_overrides() {
    let repo = Repo::new();
    let out = run(
        &[
            "coupling",
            "--min-cochanges",
            "3",
            "--since",
            "1 year ago",
            "--ref",
            "HEAD",
        ],
        &repo.root,
    );
    assert_ok(&out, "coupling with every override");
}

/// `--format` is declared `global = true`, so every subcommand advertises it in
/// its own `--help`. Both audits used to print their prose whatever was asked
/// for — the flag parsed, was accepted, and was dropped. A flag a program
/// accepts and ignores is worse than one it rejects.
#[test]
fn smoke_dup_honours_the_global_format_flag() {
    let repo = Repo::new();
    assert_ok(&run(&["code", "index", "src"], &repo.root), "code index");

    let json = stdout(&run(&["dup", "--format", "json"], &repo.root));
    let value: serde_json::Value = serde_json::from_str(json.trim())
        .unwrap_or_else(|e| panic!("`dup --format json` must emit JSON ({e}), got: {json}"));
    assert!(value["findings"].is_array(), "{value}");
    assert!(value["clusters"].is_number(), "{value}");

    let csv = stdout(&run(&["dup", "--format", "csv"], &repo.root));
    assert!(
        csv.starts_with("cluster_hash,cluster_name,"),
        "`dup --format csv` must emit a CSV header, got: {csv}"
    );

    // text and markdown stay the prose report, which is markdown already.
    let text = stdout(&run(&["dup"], &repo.root));
    assert!(text.starts_with("# Duplication"), "{text}");
    assert_eq!(
        text,
        stdout(&run(&["dup", "--format", "markdown"], &repo.root))
    );
}

#[test]
fn smoke_coupling_honours_the_global_format_flag() {
    let repo = Repo::new();
    assert_ok(&run(&["code", "index", "src"], &repo.root), "code index");

    let json = stdout(&run(&["coupling", "--format", "json"], &repo.root));
    let value: serde_json::Value = serde_json::from_str(json.trim())
        .unwrap_or_else(|e| panic!("`coupling --format json` must emit JSON ({e}), got: {json}"));
    assert!(value["findings"].is_array(), "{value}");

    let csv = stdout(&run(&["coupling", "--format", "csv"], &repo.root));
    assert!(
        csv.starts_with("file_a,file_b,cochanges"),
        "`coupling --format csv` must emit a CSV header, got: {csv}"
    );
}

/// `{"clusters": 0, "findings": []}` reads as "nothing is duplicated here",
/// which is exactly the answer an unindexed repository must not be able to
/// give. The prose is the honest payload in every format.
#[test]
fn smoke_dup_format_json_without_an_index_says_so_rather_than_returning_an_empty_list() {
    let repo = Repo::new();

    let out = run(&["dup", "--format", "json"], &repo.root);

    assert_ok(&out, "dup --format json without a code index");
    let text = stdout(&out);
    assert!(text.contains("No code index"), "{text}");
    assert!(
        serde_json::from_str::<serde_json::Value>(text.trim()).is_err(),
        "an empty JSON payload here would be a lie: {text}"
    );
}

/// The scope spelling is the same audit as the subcommand, on the CLI too —
/// `search --scope duplicates` exists because that is how MCP asks for it.
#[test]
fn smoke_search_scope_duplicates_matches_dup() {
    let repo = Repo::new();
    let scoped = run(&["search", "", "--scope", "duplicates"], &repo.root);
    assert_ok(&scoped, "search --scope duplicates");
    assert_eq!(
        stdout(&scoped),
        stdout(&run(&["dup"], &repo.root)),
        "the two spellings must not drift"
    );
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

    let out = run(
        &["collection", "update", "notes2", "-p", "**/*.markdown"],
        &repo.root,
    );
    assert_ok(&out, "collection update");
    let listed = run(&["collection", "list"], &repo.root);
    assert!(
        stdout(&listed).contains("**/*.markdown"),
        "collection update must change the stored pattern, got: {}",
        stdout(&listed)
    );

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

/// A daemon serving one repository, under a `HOME` that belongs to this test.
///
/// The hermetic default keeps every other test away from any daemon. This is
/// the one place a daemon is wanted: the routed CLI path renders the daemon's
/// typed result through `print_routed_result`, and before story 067-5ab6 that
/// renderer was never exercised — every routed call in this suite reached the
/// developer's daemon, was refused (the tempdir is outside its whitelist) and
/// fell back to the direct path.
#[cfg(unix)]
struct Daemon {
    child: std::process::Child,
    home: tempfile::TempDir,
}

#[cfg(unix)]
impl Daemon {
    fn serving(repo_root: &Path) -> Self {
        let home = tempfile::tempdir().expect("daemon HOME");
        let mdkb_dir = home.path().join(".mdkb");
        std::fs::create_dir_all(&mdkb_dir).expect("daemon home dir");
        // Default-deny confines the daemon to `HOME`; the repository lives in
        // a tempdir, so it has to be admitted explicitly.
        let parent = repo_root.parent().expect("repo parent");
        std::fs::write(
            mdkb_dir.join("daemon.toml"),
            format!("whitelist_dirs = [{:?}]\n", parent.display().to_string()),
        )
        .expect("write daemon.toml");

        let child = cli::command()
            .args(["serve", "--daemon"])
            .env("HOME", home.path())
            .env_remove("MDKB_NO_DAEMON")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn mdkb serve --daemon");
        let daemon = Self { child, home };

        let socket = daemon.hook_socket();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !socket.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "daemon did not bind {} within 20s",
                socket.display()
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        daemon
    }

    fn hook_socket(&self) -> PathBuf {
        self.home.path().join(".mdkb/daemon-hook.sock")
    }

    /// The `mdkb` invocation a user gets with this daemon running: routed.
    /// `-v` because the fallback branch announces itself at info level and the
    /// routed branch does not, so the log is what tells the two apart. (A bare
    /// `RUST_LOG=info` does not raise the level: the CLI adds its own `warn`
    /// directive on top of the environment filter, and that one wins.)
    fn client(&self) -> std::process::Command {
        let mut cmd = cli::command();
        cmd.arg("-v")
            .env("HOME", self.home.path())
            .env_remove("MDKB_NO_DAEMON");
        cmd
    }
}

#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `main.rs` has three exits for a routed mutation: the daemon's result goes
/// through `print_routed_result`; an `Undetermined` failure exits non-zero; an
/// `Unstarted` failure logs `writing in-process` and runs the direct path. Exit
/// 0 without that log line is therefore the routed renderer — and the control
/// run below, with the daemon gone, proves the line does appear when the
/// fallback runs, so the negative assertion cannot pass by accident.
#[cfg(unix)]
#[test]
fn smoke_memory_add_routed_through_a_daemon_renders_like_the_direct_path() {
    const FALLBACK: &str = "writing in-process";
    let repo = Repo::new();
    let daemon = Daemon::serving(&repo.root);

    let routed = daemon
        .client()
        .args([
            "memory",
            "add",
            "routed-entry",
            "-t",
            "Routed",
            "-c",
            "body",
        ])
        .current_dir(&repo.root)
        .output()
        .expect("routed memory add");
    assert_ok(&routed, "routed memory add");
    let routed_stderr = String::from_utf8_lossy(&routed.stderr);
    assert!(
        !routed_stderr.contains(FALLBACK),
        "with a daemon serving the repo the CLI must not write in-process: {routed_stderr}"
    );
    assert_eq!(
        stdout(&routed),
        "Added memory entry 'routed-entry'\n",
        "the routed renderer must print the same line as the direct one"
    );
    let shown = run(&["memory", "show", "routed-entry"], &repo.root);
    assert_ok(&shown, "memory show after routed add");
    assert!(
        stdout(&shown).contains("Routed"),
        "the daemon's write must be visible to a direct reader: {}",
        stdout(&shown)
    );

    // Control: same command, daemon gone, spawning forbidden.
    let home = daemon.home.path().to_path_buf();
    drop(daemon);
    let direct = cli::command()
        .args([
            "-v",
            "memory",
            "add",
            "direct-entry",
            "-t",
            "Direct",
            "-c",
            "body",
        ])
        .env("HOME", &home)
        .env_remove("MDKB_NO_DAEMON")
        .current_dir(&repo.root)
        .output()
        .expect("direct memory add");
    assert_ok(&direct, "direct memory add");
    let direct_stderr = String::from_utf8_lossy(&direct.stderr);
    assert!(
        direct_stderr.contains(FALLBACK),
        "with no daemon the fallback must announce itself, or the assertion \
         above proves nothing: {direct_stderr}"
    );
    assert_eq!(stdout(&direct), "Added memory entry 'direct-entry'\n");
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
    assert!(
        stdout(&out).contains("[tier "),
        "code calls must expose the resolution tier: {}",
        stdout(&out)
    );

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

    let out = run(&["setup", "developer", "--dry-run"], &repo.root);
    assert_ok(&out, "setup developer dry-run");
    assert!(stdout(&out).contains("query_events = true"));
    assert!(!repo.root.join(".mdkb/telemetry.key").exists());

    let out = run(
        &["setup", "developer", "--retention-days", "14"],
        &repo.root,
    );
    assert_ok(&out, "setup developer");
    let config = std::fs::read_to_string(repo.root.join(".mdkb/config.toml")).unwrap();
    assert!(config.contains("query_events = true"));
    assert!(config.contains("retention_days = 14"));
    assert!(repo.root.join(".mdkb/telemetry.key").is_file());

    let out = run(&["--format", "json", "metrics", "status"], &repo.root);
    assert_ok(&out, "metrics status");
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["retention_days"], 14);
    assert_eq!(status["key_present"], true);

    let out = run(&["metrics", "show"], &repo.root);
    assert_ok(&out, "metrics show");

    let out = run(&["metrics", "latency"], &repo.root);
    assert_ok(&out, "metrics latency");

    let out = run(&["metrics", "quality"], &repo.root);
    assert_ok(&out, "metrics quality");

    let out = run(&["metrics", "export"], &repo.root);
    assert_ok(&out, "metrics export");

    let out = run(&["metrics", "purge"], &repo.root);
    assert!(!out.status.success(), "purge without --yes must refuse");

    let out = run(&["metrics", "purge", "--yes"], &repo.root);
    assert_ok(&out, "metrics purge --yes");
}

// ── Hook lifecycle events (stdin→stdout) ────────────────────────────

#[test]
fn smoke_hook_session_start() {
    let repo = Repo::new();
    let out = run_stdin(&["hook", "session-start"], &repo.root, "{}");
    assert_ok(&out, "hook session-start");
    assert_hook_output_valid(&out, "hook session-start");
}

#[test]
fn smoke_hook_user_prompt_submit() {
    let repo = Repo::new();
    let payload = r#"{"prompt":"test prompt"}"#;
    let out = run_stdin(&["hook", "user-prompt-submit"], &repo.root, payload);
    assert_ok(&out, "hook user-prompt-submit");
    assert_hook_output_valid(&out, "hook user-prompt-submit");
}

#[test]
fn smoke_hook_post_tool_use() {
    let repo = Repo::new();
    let payload = r#"{"tool_name":"Read","tool_input":{"file_path":"src/lib.rs"}}"#;
    let out = run_stdin(&["hook", "post-tool-use"], &repo.root, payload);
    assert_ok(&out, "hook post-tool-use");
    assert_hook_output_valid(&out, "hook post-tool-use");
}

#[test]
fn smoke_hook_stop() {
    let repo = Repo::new();
    // Mining is off by default → the hook is silent and exits clean.
    let payload = r#"{"transcript_path":"/nonexistent","session_id":"s1"}"#;
    let out = run_stdin(&["hook", "stop"], &repo.root, payload);
    assert_ok(&out, "hook stop");
    assert_hook_output_valid(&out, "hook stop");
}

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

/// Unix only: the daemon has no Windows port, and `mdkb daemon status` reports
/// "Daemon commands require Unix" there. The refusal is the correct behaviour,
/// so the smoke test for a working daemon runs where a daemon exists.
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
        &["dup", "--help"],
        &["coupling", "--help"],
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

/// The first error most users meet. It must read as one line of English, not
/// as the Rust `Debug` dump of the error struct (`Error { kind:
/// DatabaseNotFound { .. }, backtrace: <disabled> }`) that `fn main() ->
/// Result<()>` printed, which threw away every `#[error]` Display string.
#[test]
fn smoke_no_init_error_is_one_display_line() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(&["memory", "list"], tmp.path());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "one line on stderr, got: {stderr}");
    assert!(
        lines[0].contains("database not initialized"),
        "the Display text must reach the user: {stderr}"
    );
    assert!(
        !stderr.contains("Error {") && !stderr.contains("backtrace"),
        "a Rust debug payload leaked: {stderr}"
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

#[test]
fn smoke_eval_recall_bm25_runs_without_a_repo_or_a_model() {
    // The eval seeds its own scratch store, so it needs no `init` in cwd; bm25
    // mode needs no model, so this is deterministic on every machine.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(&["eval", "recall", "--mode", "bm25"], tmp.path());
    assert_ok(&out, "eval recall --mode bm25");
    let text = stdout(&out);
    assert!(
        text.contains("bm25") && text.contains("recall@5:"),
        "expected a bm25 recall line: {text}"
    );
    assert!(
        text.contains("missed (bm25):"),
        "held-out queries must produce misses in bm25 mode: {text}"
    );

    let out = run(
        &["--format", "json", "eval", "recall", "--mode", "bm25"],
        tmp.path(),
    );
    assert_ok(&out, "eval recall --format json");
    let runs: serde_json::Value = serde_json::from_str(stdout(&out).trim()).expect("json array");
    assert_eq!(runs[0]["mode"], "bm25");
    assert_eq!(runs[0]["report"]["n"], 36);
    assert!(
        runs[0]["report"]["misses"]
            .as_array()
            .is_some_and(|m| !m.is_empty())
    );
}

#[test]
fn smoke_eval_recall_min_recall_fails_the_run() {
    // `--min-recall` is what lets CI fail: a floor no mode can reach exits 1
    // and names the mode and its score.
    let tmp = tempfile::tempdir().unwrap();
    let out = run(
        &["eval", "recall", "--mode", "bm25", "--min-recall", "1.01"],
        tmp.path(),
    );
    assert!(!out.status.success(), "a floor above 1.0 must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("below 1.01") && stderr.contains("bm25"),
        "error must name the floor and the mode: {stderr}"
    );
}

#[test]
fn smoke_eval_judge_bm25_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run(&["eval", "judge", "--mode", "bm25"], tmp.path());
    assert_ok(&out, "eval judge --mode bm25");
    let text = stdout(&out);
    assert!(
        text.contains("bm25") && text.contains("accuracy:"),
        "expected a bm25 accuracy line: {text}"
    );
}
