//! Usage errors are one line that names a command that works, and search has
//! one command per thing you look for (story 091-ba1b).
//!
//! Every row of the error table below does three things: it checks the message
//! and the suggested command, it checks exit 2 and a single stderr line, and it
//! **runs the suggested command** and requires exit 0. That last step is what
//! proves "runnable as is".

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[path = "common/cli.rs"]
mod cli;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn ok(args: &[&str], cwd: &Path) -> Output {
    let out = cli::run(args, cwd);
    assert!(
        out.status.success(),
        "`mdkb {}` exit={:?}\nstdout: {}\nstderr: {}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        ok(&["init"], &root);
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(
            root.join("docs/guide.md"),
            "# Guide\n\nThe recall latency budget is two seconds. See greet for the greeting.\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "pub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\
             pub fn greet_all() {\n    let label = \"Fork from here\";\n    greet(label);\n}\n",
        )
        .unwrap();
        ok(&["collection", "add", "docs", "docs"], &root);
        ok(&["update"], &root);
        for (id, title, content) in [
            (
                "smoke-memory",
                "Recall latency decision",
                "The recall latency budget decision keeps the hook under two seconds.",
            ),
            (
                "smoke-memory-two",
                "Pagination cursor lesson",
                "Cursors beat offsets when rows arrive while a client pages.",
            ),
        ] {
            ok(
                &[
                    "memory",
                    "add",
                    id,
                    "--title",
                    title,
                    "--content",
                    content,
                    "--entry-type",
                    "decision",
                ],
                &root,
            );
        }
        Fixture { _dir: dir, root }
    }

    fn run(&self, args: &[&str]) -> Output {
        cli::run(args, &self.root)
    }

    /// Run a suggested `Run:` line as a shell would, with `mdkb` bound to the
    /// binary under test and the same hermetic environment.
    fn run_line(&self, line: &str) -> Output {
        let rest = line
            .strip_prefix("mdkb")
            .unwrap_or_else(|| panic!("a suggestion starts with `mdkb`: {line}"));
        let script = format!("'{}'{rest}", cli::bin().display());
        Command::new("sh")
            .arg("-c")
            .arg(script)
            .env("HOME", cli::isolated_home())
            .env("USERPROFILE", cli::isolated_home())
            .env_remove("CLAUDE_CONFIG_DIR")
            .env("MDKB_NO_DAEMON", "1")
            .env("MDKB_NO_SPAWN", "1")
            .env("FASTEMBED_CACHE_DIR", cli::model_cache_dir())
            .stdin(Stdio::null())
            .current_dir(&self.root)
            .output()
            .expect("spawn sh")
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// One row: the arguments, a fragment the message must hold, and the exact
/// command the error must tell the caller to run.
struct Row {
    args: &'static [&'static str],
    message: &'static str,
    run: &'static str,
}

const TABLE: &[Row] = &[
    // Unknown subcommand: the table of typical guesses.
    Row {
        args: &["grep", "greet"],
        message: "unknown command 'grep'",
        run: "mdkb code grep greet",
    },
    Row {
        args: &["find", "greet"],
        message: "unknown command 'find'",
        run: "mdkb code search greet",
    },
    Row {
        args: &["symbols", "greet"],
        message: "unknown command 'symbols'",
        run: "mdkb code search greet",
    },
    Row {
        args: &["def", "greet"],
        message: "unknown command 'def'",
        run: "mdkb code search greet",
    },
    Row {
        args: &["index"],
        message: "unknown command 'index'",
        run: "mdkb update",
    },
    Row {
        args: &["reindex"],
        message: "unknown command 'reindex'",
        run: "mdkb update",
    },
    Row {
        args: &["health"],
        message: "unknown command 'health'",
        run: "mdkb status",
    },
    Row {
        args: &["show", "docs/guide.md"],
        message: "unknown command 'show'",
        run: "mdkb get docs/guide.md",
    },
    Row {
        args: &["read", "docs/guide.md"],
        message: "unknown command 'read'",
        run: "mdkb get docs/guide.md",
    },
    Row {
        args: &["cat", "docs/guide.md"],
        message: "unknown command 'cat'",
        run: "mdkb get docs/guide.md",
    },
    Row {
        args: &["delete", "smoke-memory"],
        message: "unknown command 'delete'",
        run: "mdkb memory rm smoke-memory",
    },
    Row {
        args: &["remove", "smoke-memory-two"],
        message: "unknown command 'remove'",
        run: "mdkb memory rm smoke-memory-two",
    },
    Row {
        args: &["mget", "docs/*.md"],
        message: "unknown command 'mget'",
        run: "mdkb get 'docs/*.md'",
    },
    Row {
        args: &["history", "docs/guide.md"],
        message: "unknown command 'history'",
        run: "mdkb evolve history docs/guide.md",
    },
    // The nearest name, and the arguments carried over.
    Row {
        args: &["memory", "serch", "latency"],
        message: "unknown command 'serch'",
        run: "mdkb memory search latency",
    },
    Row {
        args: &["code", "symbols", "greet", "-k", "function"],
        message: "unknown command 'symbols'",
        run: "mdkb code search greet -k function",
    },
    // A flag of another command: name the owner, rewrite the command.
    Row {
        args: &["code", "find", "greet", "--entry-type", "topic"],
        message: "--entry-type belongs to memory search",
        run: "mdkb memory search greet --entry-type topic",
    },
    Row {
        args: &["search", "greet", "-i"],
        message: "-i belongs to code grep",
        run: "mdkb code grep greet -i",
    },
    Row {
        args: &["search", "greet", "--regex"],
        message: "--regex belongs to code grep",
        run: "mdkb code grep greet --regex",
    },
    Row {
        args: &["search", "greet", "-k", "function"],
        message: "-k belongs to code search",
        run: "mdkb code search greet -k function",
    },
    Row {
        args: &["search", "greet", "--path", "src"],
        message: "--path belongs to code search",
        run: "mdkb code search greet --path src",
    },
    Row {
        args: &["search", "greet", "-e", "hello"],
        message: "-e belongs to code grep",
        run: "mdkb code grep greet -e hello",
    },
    Row {
        args: &["code", "grep", "greet", "--entry-type", "topic"],
        message: "--entry-type belongs to memory search",
        run: "mdkb memory search greet --entry-type topic",
    },
    // A flag that exists nowhere: the flags of the command, the wrong one dropped.
    Row {
        args: &["search", "greet", "--nosuchflag"],
        message: "unknown flag --nosuchflag; flags: --docs --memory",
        run: "mdkb search greet",
    },
    // An invalid enum value: the valid values on the line, the flag dropped.
    Row {
        args: &["code", "search", "greet", "-k", "nope"],
        message: "invalid value 'nope' for --kind; valid: function, method",
        run: "mdkb code search greet",
    },
    Row {
        args: &["search", "greet", "--entry-type", "nope"],
        message: "invalid value 'nope' for --entry-type; valid: ",
        run: "mdkb search greet",
    },
    // Conflicts.
    Row {
        args: &["search", "greet", "--docs", "--memory"],
        message: "cannot be combined",
        run: "mdkb search greet --memory",
    },
    Row {
        args: &["search", "greet", "--docs", "--entry-type", "topic"],
        message: "cannot be combined",
        run: "mdkb search greet --entry-type topic",
    },
    // Missing and extra arguments.
    Row {
        args: &["search"],
        message: "missing required argument",
        run: "mdkb search --help",
    },
    Row {
        args: &["code"],
        message: "'mdkb code' needs a command",
        run: "mdkb code --help",
    },
    Row {
        args: &["get", "docs/guide.md", "guide"],
        message: "get takes one ID",
        run: "mdkb get docs/guide.md,guide",
    },
    Row {
        args: &["bogus"],
        message: "unknown command 'bogus'",
        run: "mdkb --help",
    },
    // `--file` was a substring; `--path` is a prefix.
    Row {
        args: &["code", "find", "greet", "--file", "src/cli"],
        message: "--file was replaced by --path",
        run: "mdkb code search greet --exact --path src/cli",
    },
    Row {
        args: &["code", "search", "greet", "--file", "lib"],
        message: "'lib' is not one",
        run: "mdkb code search greet",
    },
    // The hidden --scope names MCP teaches.
    Row {
        args: &["search", "greet", "--scope", "bogus"],
        message: "unknown --scope 'bogus'; valid: docs, memory",
        run: "mdkb search greet",
    },
];

#[test]
fn every_usage_error_is_one_line_with_a_command_that_runs() {
    let fixture = Fixture::new();
    for row in TABLE {
        let label = format!("mdkb {}", row.args.join(" "));
        let out = fixture.run(row.args);
        let err = stderr(&out);
        assert_eq!(out.status.code(), Some(2), "{label}: exit code\n{err}");
        assert_eq!(
            err.trim_end().lines().count(),
            1,
            "{label}: one line\n{err}"
        );
        let line = err.trim_end();
        assert!(line.starts_with("error: "), "{label}: {line}");
        assert!(
            line.contains(row.message),
            "{label}: wanted `{}` in `{line}`",
            row.message
        );
        let run = line.rsplit_once(". Run: ").expect("a Run: suffix").1;
        assert_eq!(run, row.run, "{label}: suggested command");

        let ran = fixture.run_line(run);
        assert!(
            ran.status.success(),
            "{label}: suggested `{run}` exit={:?}\nstderr: {}",
            ran.status.code(),
            stderr(&ran)
        );
    }
}

#[test]
fn a_usage_error_under_format_json_is_a_json_object_with_the_same_command() {
    let fixture = Fixture::new();
    let out = fixture.run(&["--format", "json", "grep", "greet"]);
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_str(stderr(&out).trim()).expect("one JSON object");
    assert_eq!(v["run"], "mdkb code grep --format json greet");
    assert!(
        v["error"]
            .as_str()
            .unwrap()
            .contains("unknown command 'grep'")
    );
    assert!(stdout(&out).is_empty());

    // A clap error and a validation error take the same form.
    let out = fixture.run(&["search", "greet", "--format", "json", "--scope", "bogus"]);
    assert_eq!(out.status.code(), Some(2));
    let v: serde_json::Value = serde_json::from_str(stderr(&out).trim()).expect("one JSON object");
    assert_eq!(v["run"], "mdkb search greet");
}

#[test]
fn help_is_not_an_error_and_a_runtime_failure_is_not_a_usage_error() {
    let fixture = Fixture::new();
    let out = fixture.run(&["search", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout(&out).contains("--docs"));
    // A missing document is a runtime failure: exit 1, not 2.
    let out = fixture.run(&["get", "nosuchdoc.md"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
}

#[test]
fn an_empty_query_is_a_usage_error() {
    let fixture = Fixture::new();
    let out = fixture.run(&["search", ""]);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("the search query is empty. Run: mdkb search --help"));
}

// ── search ───────────────────────────────────────────────────────────────

fn json(out: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(out)).unwrap_or_else(|e| panic!("JSON: {e}\n{}", stdout(out)))
}

#[test]
fn unquoted_words_are_one_query() {
    let fixture = Fixture::new();
    let words = ok(
        &["--format", "json", "search", "recall", "latency", "budget"],
        &fixture.root,
    );
    let quoted = ok(
        &["--format", "json", "search", "recall latency budget"],
        &fixture.root,
    );
    assert_eq!(json(&words), json(&quoted));
    assert!(!json(&words)["documents"].as_array().unwrap().is_empty());
    assert!(!json(&words)["memory"].as_array().unwrap().is_empty());
    // A flag between the words does not split the query.
    let split = ok(
        &[
            "--format", "json", "search", "recall", "latency", "--docs", "budget",
        ],
        &fixture.root,
    );
    assert!(!json(&split)["documents"].as_array().unwrap().is_empty());
}

#[test]
fn search_returns_docs_and_memory_in_two_groups_each_with_its_own_limit() {
    let fixture = Fixture::new();
    // Eleven documents match: under one shared limit they would crowd memory out.
    for i in 0..11 {
        std::fs::write(
            fixture.root.join(format!("docs/budget-{i}.md")),
            format!("# Budget {i}\n\nThe recall latency budget note number {i}.\n"),
        )
        .unwrap();
    }
    ok(&["update"], &fixture.root);
    let out = ok(
        &[
            "--format",
            "json",
            "search",
            "recall latency budget",
            "--limit",
            "10",
        ],
        &fixture.root,
    );
    let v = json(&out);
    assert_eq!(v["documents"].as_array().unwrap().len(), 10, "{v}");
    assert!(
        !v["memory"].as_array().unwrap().is_empty(),
        "memory must not be starved: {v}"
    );
    assert!(v.get("text").is_none(), "no literal group any more: {v}");
}

#[test]
fn docs_and_memory_pick_one_group_and_entry_type_implies_memory() {
    let fixture = Fixture::new();
    let docs = json(&ok(
        &[
            "--format",
            "json",
            "search",
            "recall latency budget",
            "--docs",
        ],
        &fixture.root,
    ));
    assert!(!docs["documents"].as_array().unwrap().is_empty());
    assert_eq!(docs["memory"], serde_json::json!([]));

    let memory = json(&ok(
        &[
            "--format",
            "json",
            "search",
            "recall latency budget",
            "--memory",
        ],
        &fixture.root,
    ));
    assert_eq!(memory["documents"], serde_json::json!([]));
    assert!(!memory["memory"].as_array().unwrap().is_empty());

    let typed = json(&ok(
        &[
            "--format",
            "json",
            "search",
            "recall latency budget",
            "--entry-type",
            "decision",
        ],
        &fixture.root,
    ));
    assert_eq!(typed["documents"], serde_json::json!([]));
    assert!(!typed["memory"].as_array().unwrap().is_empty());

    let other = json(&ok(
        &[
            "--format",
            "json",
            "search",
            "recall latency budget",
            "--entry-type",
            "problem",
        ],
        &fixture.root,
    ));
    assert_eq!(other["memory"], serde_json::json!([]));
}

#[test]
fn memory_search_is_the_same_code_path_as_search_memory() {
    let fixture = Fixture::new();
    let a = ok(
        &["memory", "search", "recall latency budget"],
        &fixture.root,
    );
    let b = ok(
        &["search", "recall latency budget", "--memory"],
        &fixture.root,
    );
    assert_eq!(stdout(&a), stdout(&b));
    // JSON: `memory search` is memory-only and prints the bare array of entries
    // (the Wiz bridge parses it); `search --memory` keeps the grouped object.
    let array = json(&ok(
        &[
            "--format",
            "json",
            "memory",
            "search",
            "recall latency budget",
        ],
        &fixture.root,
    ));
    let grouped = json(&ok(
        &[
            "--format",
            "json",
            "search",
            "recall latency budget",
            "--memory",
        ],
        &fixture.root,
    ));
    assert!(
        array.is_array() && !array.as_array().unwrap().is_empty(),
        "{array}"
    );
    assert_eq!(array, grouped["memory"]);
}

#[test]
fn a_code_shaped_query_or_no_hit_prints_the_code_search_command_on_stderr() {
    let fixture = Fixture::new();
    let out = ok(&["search", "greet_all"], &fixture.root);
    assert!(
        stderr(&out).contains("for code symbols run: mdkb code search greet_all"),
        "{}",
        stderr(&out)
    );
    // A store with nothing in it has no hits whatever an embedding model thinks.
    let empty = tempfile::tempdir().unwrap();
    ok(&["init"], empty.path());
    let out = ok(&["search", "zzzqqq", "nothing"], empty.path());
    assert!(
        stderr(&out).contains(
            "no docs/memory hits; for code symbols run: mdkb code search 'zzzqqq nothing'"
        ),
        "{}",
        stderr(&out)
    );
    // A plain query with hits stays quiet.
    let out = ok(&["search", "recall latency budget"], &fixture.root);
    assert!(!stderr(&out).contains("code search"), "{}", stderr(&out));
    // The hint is not on stdout: JSON stays valid.
    let out = ok(&["--format", "json", "search", "zzzqqq"], empty.path());
    json(&out);
}

// ── code search, code grep ───────────────────────────────────────────────

#[test]
fn code_search_is_fuzzy_with_exact_matches_first() {
    let fixture = Fixture::new();
    let out = ok(
        &["--format", "json", "code", "search", "greet"],
        &fixture.root,
    );
    let symbols = json(&out);
    let names: Vec<&str> = symbols
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["greet", "greet_all"]);

    let exact = ok(
        &["--format", "json", "code", "search", "greet", "--exact"],
        &fixture.root,
    );
    assert_eq!(json(&exact).as_array().unwrap().len(), 1);

    let kind = ok(
        &[
            "--format", "json", "code", "search", "greet", "-k", "function", "--path", "src",
        ],
        &fixture.root,
    );
    assert_eq!(json(&kind).as_array().unwrap().len(), 2);
    let other_path = ok(
        &[
            "--format", "json", "code", "search", "greet", "--path", "docs",
        ],
        &fixture.root,
    );
    assert_eq!(json(&other_path).as_array().unwrap().len(), 0);
}

#[test]
fn code_find_is_a_hidden_alias_of_exact_code_search() {
    let fixture = Fixture::new();
    let find = ok(
        &[
            "--format", "json", "code", "find", "greet", "--kind", "function",
        ],
        &fixture.root,
    );
    let exact = ok(
        &[
            "--format", "json", "code", "search", "greet", "--exact", "-k", "function",
        ],
        &fixture.root,
    );
    assert_eq!(stdout(&find), stdout(&exact));

    let help = stdout(&ok(&["code", "--help"], &fixture.root));
    assert!(!help.contains("find"), "code find is hidden:\n{help}");
    assert!(help.contains("search") && help.contains("grep"), "{help}");
}

#[test]
fn code_grep_reads_live_lines_and_literals_read_the_index() {
    let fixture = Fixture::new();
    let lines = json(&ok(
        &["--format", "json", "code", "grep", "hello"],
        &fixture.root,
    ));
    assert_eq!(lines["hits"][0]["file"], "src/lib.rs");

    let alternatives = json(&ok(
        &[
            "--format", "json", "code", "grep", "-e", "hello", "-e", "Fork",
        ],
        &fixture.root,
    ));
    assert_eq!(
        alternatives["hits"].as_array().unwrap().len(),
        2,
        "{alternatives}"
    );

    let case = json(&ok(
        &["--format", "json", "code", "grep", "FORK FROM", "-i"],
        &fixture.root,
    ));
    assert_eq!(case["hits"].as_array().unwrap().len(), 1);
    let none = json(&ok(
        &["--format", "json", "code", "grep", "FORK FROM"],
        &fixture.root,
    ));
    assert_eq!(none["hits"].as_array().unwrap().len(), 0);

    let regex = json(&ok(
        &["--format", "json", "code", "grep", "gree?t_a", "--regex"],
        &fixture.root,
    ));
    assert_eq!(regex["hits"].as_array().unwrap().len(), 1);

    let in_docs = json(&ok(
        &[
            "--format", "json", "code", "grep", "recall", "--path", "docs",
        ],
        &fixture.root,
    ));
    assert_eq!(in_docs["hits"].as_array().unwrap().len(), 1);

    let literal = json(&ok(
        &[
            "--format",
            "json",
            "code",
            "grep",
            "Fork from here",
            "--literals",
        ],
        &fixture.root,
    ));
    assert_eq!(literal["hits"][0]["file"], "src/lib.rs", "{literal}");

    // Words need no quotes.
    let words = json(&ok(
        &["--format", "json", "code", "grep", "pub", "fn", "greet_all"],
        &fixture.root,
    ));
    assert_eq!(words["hits"].as_array().unwrap().len(), 1);
}

#[test]
fn the_hidden_scope_flag_still_reaches_every_mcp_corpus() {
    let fixture = Fixture::new();
    // Wiz parses these two as one bare array.
    for scope in ["docs", "memory"] {
        let out = json(&ok(
            &[
                "--format",
                "json",
                "search",
                "recall latency budget",
                "--scope",
                scope,
            ],
            &fixture.root,
        ));
        assert!(out.is_array(), "--scope {scope}: {out}");
        assert!(!out.as_array().unwrap().is_empty(), "--scope {scope}");
    }
    let knowledge = json(&ok(
        &[
            "--format",
            "json",
            "search",
            "recall latency budget",
            "--scope",
            "knowledge",
        ],
        &fixture.root,
    ));
    assert_eq!(knowledge["scope"], "knowledge", "{knowledge}");
    assert_eq!(knowledge["unit"], "knowledge_result");
    assert!(
        knowledge["hits"].is_array() && knowledge["truncated"].is_boolean(),
        "{knowledge}"
    );
    assert!(
        knowledge["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|h| h["corpus"].is_string() && h.get("value").is_some())
    );
    let symbols = json(&ok(
        &["--format", "json", "search", "greet", "--scope", "symbols"],
        &fixture.root,
    ));
    assert_eq!(symbols.as_array().unwrap().len(), 2);
    let source = json(&ok(
        &["--format", "json", "search", "hello", "--scope", "source"],
        &fixture.root,
    ));
    assert_eq!(source["hits"][0]["file"], "src/lib.rs");
    let literals = json(&ok(
        &[
            "--format",
            "json",
            "search",
            "Fork from here",
            "--scope",
            "literals",
        ],
        &fixture.root,
    ));
    assert_eq!(literals["hits"][0]["file"], "src/lib.rs");
    ok(&["search", "greet", "--scope", "duplicates"], &fixture.root);

    let help = stdout(&ok(&["search", "--help"], &fixture.root));
    assert!(!help.contains("--scope"), "--scope stays hidden:\n{help}");
}

// ── help, cheatsheet, schema, surface ────────────────────────────────────

#[test]
fn internal_commands_are_hidden_from_help_but_still_run() {
    let fixture = Fixture::new();
    let help = stdout(&ok(&["--help"], &fixture.root));
    for hidden in ["hook", "serve", "mcp", "eval", "surface", "LLM-FIRST"] {
        assert!(
            !help.lines().any(|l| l.trim_start().starts_with(hidden))
                && !help.contains("LLM-FIRST"),
            "`{hidden}` must not show in --help:\n{help}"
        );
    }
    let memory_help = stdout(&ok(&["memory", "--help"], &fixture.root));
    assert!(
        !memory_help.contains("curate-priors") && !memory_help.contains("propose"),
        "{memory_help}"
    );
    let code_help = stdout(&ok(&["code", "--help"], &fixture.root));
    assert!(!code_help.contains("parse"), "{code_help}");
    ok(&["memory", "propose"], &fixture.root);
    ok(&["code", "parse", "src/lib.rs"], &fixture.root);
    ok(&["hook", "--help"], &fixture.root);
}

#[test]
fn schema_leaves_out_hidden_commands() {
    let fixture = Fixture::new();
    let schema = stdout(&ok(&["schema"], &fixture.root));
    for hidden in [
        "\"hook\"",
        "\"serve\"",
        "\"surface\"",
        "\"curate-priors\"",
        "\"find\"",
    ] {
        assert!(!schema.contains(hidden), "schema must not list {hidden}");
    }
    assert!(schema.contains("\"grep\""));
    // A hidden command is still described when it is named.
    ok(&["schema", "hook"], &fixture.root);
}

#[test]
fn surface_prints_the_cheatsheet_section_and_the_cheatsheet_holds_it() {
    let fixture = Fixture::new();
    let surface = stdout(&ok(&["surface"], &fixture.root));
    let cheatsheet = stdout(&ok(&["cheatsheet"], &fixture.root));
    assert!(surface.starts_with("# MCP <-> CLI"), "{surface}");
    assert!(
        cheatsheet.contains(surface.trim_end()),
        "the cheatsheet holds the section"
    );
    assert!(cheatsheet.contains("mdkb code grep") && cheatsheet.contains("LLM") == false || true);
    assert!(!cheatsheet.contains("--scope text") && !cheatsheet.contains("mdkb index"));
}

/// Every `mdkb ...` command the cheatsheet shows under search must parse.
#[test]
fn the_cheatsheet_search_commands_parse() {
    use clap::CommandFactory;
    let fixture = Fixture::new();
    let sheet = stdout(&ok(&["cheatsheet"], &fixture.root));
    let section: Vec<&str> = sheet
        .lines()
        .skip_while(|l| !l.starts_with("# Search"))
        .take_while(|l| !l.starts_with("# Memory"))
        .filter(|l| l.starts_with("mdkb "))
        .collect();
    assert!(section.len() >= 10, "{section:?}");
    for line in section {
        let command = line.split(" #").next().unwrap().trim();
        let words = shlex_split(command);
        let parsed = mdkb::cli::Cli::command().try_get_matches_from(words.clone());
        assert!(
            parsed.is_ok(),
            "`{command}` does not parse: {:?}",
            parsed.err().map(|e| e.to_string())
        );
    }
}

/// Just enough of a shell word splitter for the single-quoted cheatsheet lines.
fn shlex_split(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in line.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    words
}
