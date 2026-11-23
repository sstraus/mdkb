//! `mdkb status`, `update` flags, `rm` aliases and the removed names of the
//! status/update/top-level consolidation (story 093-1e8d).

use std::path::{Path, PathBuf};
use std::process::Output;

#[path = "common/cli.rs"]
mod cli;
use cli::run;

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
        let root: &Path = dir.path();
        assert_ok(&run(&["init"], root), "init");
        let docs = root.join("docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(
            docs.join("guide.md"),
            "# Getting Started\n\nThis is the setup guide for the project.\n",
        )
        .unwrap();
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n",
        )
        .unwrap();
        let root = root.to_path_buf();
        Repo { _dir: dir, root }
    }
}

/// The TUICommander dashboard reads these top-level keys of `stats`, which
/// `status` replaced; `daemon` and `telemetry` are the two sections folded in
/// from `daemon status` and `metrics status`.
#[test]
fn smoke_status_json_is_one_object_with_every_stats_key() {
    let repo = Repo::new();
    let out = run(&["--format", "json", "status"], &repo.root);
    assert_ok(&out, "status --format json");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("status must print exactly one JSON object: {e}"));
    let object = json.as_object().expect("a single object");
    for key in [
        "embedding",
        "code_embedding",
        "header",
        "index",
        "collections",
        "memory",
        "code",
        "sessions",
        "hooks",
        "quarantine",
        "recall",
        "doctor",
        "daemon",
        "telemetry",
    ] {
        assert!(object.contains_key(key), "status JSON lost `{key}`: {json}");
    }
    assert_eq!(json["daemon"]["running"], false, "MDKB_NO_DAEMON test home");
    assert!(json["telemetry"]["enabled"].is_boolean());
}

/// Catches an unhealthy line that names the problem and not the fix.
#[test]
fn smoke_status_text_ends_unhealthy_lines_with_the_fixing_command() {
    let repo = Repo::new();
    let out = run(&["status", "--no-color"], &repo.root);
    assert_ok(&out, "status");
    let text = stdout(&out);
    assert!(
        text.contains("not running; run: mdkb daemon restart"),
        "{text}"
    );
}

/// Every removed name of the status/update/top-level consolidation fails with
/// the one-line `Run:` form, and the command it prints works as printed.
#[test]
fn smoke_consolidated_removed_names_print_a_command_that_runs() {
    let repo = Repo::new();
    std::fs::write(repo.root.join("docs/v2.md"), "# V2\nUpdated guide.\n").unwrap();
    run(&["update"], &repo.root);
    let journals = repo.root.join("journals");
    std::fs::create_dir_all(&journals).unwrap();
    std::fs::write(journals.join("e.md"), "# E\n\nSome content.\n").unwrap();
    let journals = journals.to_str().unwrap();
    let sessions = repo.root.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let sessions = sessions.to_str().unwrap();
    // The `doctor` replacement needs hooks registered to exit 0.
    assert_ok(&run(&["setup", "hooks", "claude"], &repo.root), "hooks");

    let cases: Vec<(Vec<&str>, String)> = vec![
        (vec!["code", "info"], "mdkb status".into()),
        (vec!["daemon", "status"], "mdkb status".into()),
        (vec!["metrics", "status"], "mdkb status".into()),
        (vec!["setup", "check"], "mdkb doctor --full".into()),
        (vec!["code", "init"], "mdkb update --code".into()),
        (
            vec!["session", "index", "--sessions-path", sessions],
            format!(
                "mdkb update --sessions --sessions-path {}",
                mdkb::cli::usage_errors::shell_quote(sessions)
            ),
        ),
        (vec!["dup"], "mdkb code dup".into()),
        (vec!["coupling"], "mdkb code coupling".into()),
        (vec!["coupling", "--docs"], "mdkb graph coupling".into()),
        (
            vec!["journal", "import", journals, "-n"],
            format!("mdkb memory import --journal {journals} --dry-run"),
        ),
        (
            vec!["journal", "import-all", "--dir", journals],
            format!("mdkb memory import --journal {journals}"),
        ),
        (
            vec![
                "evolve",
                "supersedes",
                "v2.md",
                "guide.md",
                "-r",
                "newer version",
            ],
            "mdkb evolve link v2.md supersedes guide.md -r 'newer version'".into(),
        ),
    ];
    for (args, expected) in cases {
        let out = run(&args, &repo.root);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(2),
            "`mdkb {}` must fail",
            args.join(" ")
        );
        assert!(
            stderr.contains("error: ")
                && stderr.contains("was removed. Run: ")
                && stderr.contains(&expected),
            "`mdkb {}` must print `Run: {expected}`: {stderr}",
            args.join(" ")
        );
        assert_eq!(stderr.trim_end().lines().count(), 1, "one line: {stderr}");
        let mut words = shell_words(&expected);
        assert_eq!(words.remove(0), "mdkb");
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        let ran = run(&words, &repo.root);
        assert_ok(&ran, &format!("suggested `{expected}`"));
    }
}

/// Catches: the legacy aliases `stats`, `embed` and `code index` failing with the
/// removed-name error, which breaks every script still calling them, or showing
/// up in `--help` and so becoming official again.
///
/// DEFERRED (2026-11-23) — remove in February 2027 (Boss), with `retired::legacy_alias_argv`.
#[test]
fn legacy_aliases_still_run_and_stay_out_of_help() {
    let repo = Repo::new();
    let status = run(&["--format", "json", "stats"], &repo.root);
    assert_ok(&status, "stats");
    let json: serde_json::Value = serde_json::from_str(&stdout(&status)).expect("stats JSON");
    assert!(
        json.get("daemon").is_some(),
        "stats prints the status object: {json}"
    );
    assert_ok(
        &run(&["stats", "--no-color"], &repo.root),
        "stats --no-color",
    );
    assert_ok(
        &run(
            &["--format", "json", "code", "index", "--force"],
            &repo.root,
        ),
        "code index --force",
    );
    assert_ok(
        &run(&["code", "index", "src"], &repo.root),
        "code index src",
    );
    assert_ok(&run(&["embed"], &repo.root), "embed");

    let top = stdout(&run(&["--help"], &repo.root));
    let code = stdout(&run(&["code", "--help"], &repo.root));
    for (help, word) in [(&top, "stats"), (&top, "embed"), (&code, "index")] {
        assert!(
            !help
                .lines()
                .any(|l| l.trim_start().starts_with(&format!("{word} "))),
            "`{word}` must stay out of help:\n{help}"
        );
    }
    assert!(
        top.lines().any(|l| l.trim_start().starts_with("status ")),
        "{top}"
    );
}

/// Split a printed command the way a POSIX shell would, for single quotes only.
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let (mut quoted, mut started) = (false, false);
    for c in line.chars() {
        match c {
            '\'' => {
                quoted = !quoted;
                started = true;
            }
            ' ' if !quoted => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(current);
    }
    words
}

#[test]
fn smoke_update_flags_select_one_leg_and_reject_combinations() {
    let repo = Repo::new();
    let out = run(&["update", "--code", "--sessions"], &repo.root);
    assert!(!out.status.success(), "--code and --sessions conflict");
    let out = run(
        &["update", "--files", "docs/guide.md", "--code"],
        &repo.root,
    );
    assert!(!out.status.success(), "--files conflicts with --code");
    assert_ok(
        &run(&["update", "--code", "--force"], &repo.root),
        "rebuild code",
    );
}

#[test]
fn smoke_rm_aliases_and_evolve_unlink_work() {
    let repo = Repo::new();
    for verb in ["rm", "remove", "delete"] {
        let name = format!("tmp-{verb}");
        assert_ok(
            &run(&["collection", "add", &name, "docs"], &repo.root),
            "collection add",
        );
        assert_ok(
            &run(&["collection", verb, &name], &repo.root),
            &format!("collection {verb}"),
        );
    }
    let out = run(&["evolve", "unlink", "9999"], &repo.root);
    assert!(!out.status.success(), "unknown edge id fails");
    assert_ok(
        &run(&["status", "--root", "."], &repo.root),
        "--root after the command name is accepted on any command",
    );
}
