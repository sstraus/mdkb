//! `mdkb search --root` (story 218-78c8): the selector grammar MCP `search`
//! accepts, from the CLI, resolved by the same `RootSelector`.
//!
//! Every store is a real current-schema store in a temp dir and the CLI runs
//! with `HOME` pointed at another temp dir holding its own `repos.json`, so
//! nothing here reads or writes the real `~/.mdkb`. The fixtures are memory
//! entries whose title carries an identifier-shaped needle: no ONNX model
//! runs here, so only the lexical leg can admit a hit (see
//! `cross_repo_search.rs`).

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Output;

use mdkb::core::Context;
use mdkb::daemon::repo_map::RepoMap;
use mdkb::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
use tempfile::TempDir;

#[path = "common/cli.rs"]
mod cli;

/// A store at `parent/name` holding one memory entry about `needle`.
fn store(parent: &Path, name: &str, needle: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).unwrap();
    let root = mdkb::domain::canonicalize_plain(&root).unwrap();
    mdkb::cli::handlers::handle_init(&root).expect("init");
    let ctx = Context::open(&root).expect("open store");
    let now = chrono::Utc::now().timestamp();
    add_entry(
        &ctx.conn,
        &MemoryEntry {
            triggers: Vec::new(),
            id: format!("{needle}-entry"),
            title: format!("The {needle} decision"),
            content: format!("Body mentioning {needle} so the lexical leg has something to match."),
            entry_type: EntryType::Decision,
            tags: vec![needle.to_string()],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        },
    )
    .expect("seed memory entry");
    root
}

/// A throwaway `HOME` whose repo map lists `known`.
fn home_knowing(tmp: &Path, known: &[&Path]) -> PathBuf {
    let home = tmp.join("home");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    let map = RepoMap::open(Some(home.join(".mdkb/repos.json")), &[]);
    for root in known {
        map.record(root);
    }
    home
}

fn search(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    cli::command()
        .env("HOME", home)
        .env_remove("MDKB_NAMESPACE")
        .arg("search")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("run mdkb")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Catches: the CLI/MCP parity gap itself — `--root <name>` ignored, so the
/// search runs against the repo of the current directory and the other repo's
/// entry never appears.
#[test]
fn root_by_name_searches_that_repo_from_another_repos_directory() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let beta = store(tmp.path(), "beta", "quux_ledger");
    let home = home_knowing(tmp.path(), &[&alpha, &beta]);

    let without = search(&home, &beta, &["zonk_harvest", "--scope", "memory"]);
    assert!(without.status.success(), "{without:?}");
    assert!(
        !stdout(&without).contains("zonk_harvest"),
        "control: beta alone must not see alpha's entry: {}",
        stdout(&without)
    );

    let with = search(
        &home,
        &beta,
        &["zonk_harvest", "--scope", "memory", "--root", "alpha"],
    );
    assert!(with.status.success(), "{with:?}");
    assert!(stdout(&with).contains("zonk_harvest"), "{}", stdout(&with));
}

/// Catches: an absolute path needing the repo to be on the map (the MCP
/// grammar says a path "need not be a known repo"), and the path term being
/// looked up as a name.
#[test]
fn root_by_absolute_path_needs_no_repo_map_entry() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let beta = store(tmp.path(), "beta", "quux_ledger");
    let home = home_knowing(tmp.path(), &[]);

    let out = search(
        &home,
        &beta,
        &[
            "zonk_harvest",
            "--scope",
            "memory",
            "--root",
            alpha.to_str().unwrap(),
        ],
    );

    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains("zonk_harvest"), "{}", stdout(&out));
}

/// Catches: a CLI that silently picks one of several repos sharing a name — it
/// must exit non-zero and name every candidate so the caller can choose.
#[test]
fn an_ambiguous_name_exits_non_zero_and_lists_the_candidate_paths() {
    let tmp = TempDir::new().unwrap();
    let first = store(&tmp.path().join("one"), "twin", "zonk_harvest");
    let second = store(&tmp.path().join("two"), "twin", "quux_ledger");
    let other = store(tmp.path(), "other", "mork_signal");
    let home = home_knowing(tmp.path(), &[&first, &second, &other]);

    let out = search(&home, &other, &["zonk_harvest", "--root", "twin"]);

    assert!(!out.status.success(), "{out:?}");
    let err = stderr(&out);
    assert!(err.contains(first.to_str().unwrap()), "{err}");
    assert!(err.contains(second.to_str().unwrap()), "{err}");
    assert!(
        stdout(&out).is_empty(),
        "no hits may print: {}",
        stdout(&out)
    );
}

/// Catches: `*` or a list collapsing to the first repo (hits from the second
/// vanish), and the repos' answers running together with no path to tell which
/// repo a hit came from.
#[test]
fn a_star_searches_every_known_repo_and_labels_each_by_path() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let beta = store(tmp.path(), "beta", "zonk_harvest");
    let home = home_knowing(tmp.path(), &[&alpha, &beta]);

    let out = search(
        &home,
        &beta,
        &["zonk_harvest", "--scope", "memory", "--root", "*"],
    );

    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(text.contains(&format!("## {}", alpha.display())), "{text}");
    assert!(text.contains(&format!("## {}", beta.display())), "{text}");
}

/// Catches: a per-repo scope (symbols) run silently against one of several
/// repos, and a JSON stream with headings between documents.
#[test]
fn several_repos_are_refused_where_no_merged_answer_exists() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let beta = store(tmp.path(), "beta", "quux_ledger");
    let home = home_knowing(tmp.path(), &[&alpha, &beta]);

    let scoped = search(&home, &beta, &["x", "--scope", "symbols", "--root", "*"]);
    assert!(!scoped.status.success(), "{scoped:?}");
    assert!(
        stderr(&scoped).contains("cannot span repos"),
        "{}",
        stderr(&scoped)
    );

    let json = cli::command()
        .env("HOME", &home)
        .args(["--format", "json", "search", "x", "--root", "*"])
        .current_dir(&beta)
        .output()
        .expect("run mdkb");
    assert!(!json.status.success(), "{json:?}");
    assert!(
        stderr(&json).contains("print one repo"),
        "{}",
        stderr(&json)
    );
}
