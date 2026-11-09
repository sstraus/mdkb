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

/// A controlled peer for the repository-owned `daemon.repos` protocol. The
/// test process is the live PID; no detached daemon or developer state is used.
fn delayed_repo_list(
    home: &Path,
    roots: &[&Path],
    delay: std::time::Duration,
) -> std::thread::JoinHandle<()> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    let listener = UnixListener::bind(home.join(".mdkb/daemon-hook.sock")).unwrap();
    std::fs::write(
        home.join(".mdkb/daemon.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    let roots: Vec<PathBuf> = roots.iter().map(|p| p.to_path_buf()).collect();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let mut body = vec![0; u32::from_le_bytes(header) as usize];
        stream.read_exact(&mut body).unwrap();
        let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["method"], "daemon.repos");
        std::thread::sleep(delay);
        let response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": request["id"], "result": {"roots": roots},
        }))
        .unwrap();
        // A timed-out client has closed its side; that is expected here.
        let _ = stream.write_all(&u32::try_from(response.len()).unwrap().to_le_bytes());
        let _ = stream.write_all(&response);
    })
}

/// Catches: a live daemon missing the 2 s list deadline aborting scoped search
/// even though the persisted map identifies a readable store.
#[test]
fn a_slow_daemon_list_does_not_abort_scoped_search() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let home = home_knowing(tmp.path(), &[&alpha]);
    let before = std::fs::read(home.join(".mdkb/repos.json")).unwrap();
    let server = delayed_repo_list(&home, &[&alpha], std::time::Duration::from_millis(2250));

    let out = search(
        &home,
        &alpha,
        &["zonk_harvest", "--scope", "memory", "--root", "alpha"],
    );
    server.join().unwrap();

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("zonk_harvest"), "{}", stdout(&out));
    let err = stderr(&out);
    let notes: Vec<_> = err
        .lines()
        .filter(|line| line.contains("repos.json"))
        .collect();
    assert_eq!(notes.len(), 1, "{err}");
    assert!(notes[0].contains("no answer within 2s"), "{err}");
    assert!(notes[0].contains("using persisted"), "{err}");
    assert_eq!(
        std::fs::read(home.join(".mdkb/repos.json")).unwrap(),
        before
    );
}

/// Catches: always using the persisted map even when the live daemon answers
/// a different list, causing a selector to miss an in-memory registration.
#[test]
fn a_timely_daemon_list_wins_over_the_persisted_map() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let beta = store(tmp.path(), "beta", "quux_ledger");
    let home = home_knowing(tmp.path(), &[&beta]);
    let server = delayed_repo_list(&home, &[&alpha], std::time::Duration::ZERO);

    let out = search(
        &home,
        &beta,
        &["zonk_harvest", "--scope", "memory", "--root", "alpha"],
    );
    server.join().unwrap();

    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("zonk_harvest"), "{}", stdout(&out));
    assert!(
        !stderr(&out).contains("using persisted"),
        "{}",
        stderr(&out)
    );
}

/// Catches: treating a corrupt fallback map as an empty successful search.
#[test]
fn a_slow_daemon_list_does_not_hide_a_corrupt_persisted_map() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let home = home_knowing(tmp.path(), &[&alpha]);
    std::fs::write(home.join(".mdkb/repos.json"), "{broken").unwrap();
    let server = delayed_repo_list(&home, &[&alpha], std::time::Duration::from_millis(2250));

    let out = search(
        &home,
        &alpha,
        &["zonk_harvest", "--scope", "memory", "--root", "alpha"],
    );
    server.join().unwrap();

    assert!(!out.status.success());
    assert!(stderr(&out).contains("repos.json"), "{}", stderr(&out));
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
    assert_eq!(
        std::fs::read_to_string(home.join(".mdkb/repos.json")).unwrap(),
        "{broken"
    );
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

/// Catches: dot being looked up as a repository name instead of the local path.
#[test]
fn dot_root_searches_the_containing_registered_repo() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let home = home_knowing(tmp.path(), &[&alpha]);
    let subdir = alpha.join("src");
    std::fs::create_dir(&subdir).unwrap();

    let out = search(
        &home,
        &subdir,
        &["zonk_harvest", "--scope", "memory", "--root", "."],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("zonk_harvest"), "{}", stdout(&out));
}

/// Catches: relative or absolute descendants opening the subdirectory instead of
/// the nearest known store; explicit dot syntax distinguishes paths from names.
#[test]
fn root_paths_search_the_nearest_store_from_the_current_directory() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let nested = store(&alpha, "nested", "quux_ledger");
    let leaf = nested.join("src");
    std::fs::create_dir(&leaf).unwrap();
    // Only the outer store is registered: the nearest initialized store wins.
    let home = home_knowing(tmp.path(), &[&alpha]);
    for root in ["./nested/src", "./nested/src/..", leaf.to_str().unwrap()] {
        let out = search(
            &home,
            &alpha,
            &["quux_ledger", "--scope", "memory", "--root", root],
        );
        assert!(out.status.success(), "{root}: {}", stderr(&out));
        assert!(
            stdout(&out).contains("quux_ledger"),
            "{root}: {}",
            stdout(&out)
        );
    }
}

/// Catches: silently opening an uninitialized outside path rather than refusing it.
#[test]
fn an_outside_path_exits_non_zero_and_names_the_path() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "zonk_harvest");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(outside.join(".mdkb")).unwrap();
    let home = home_knowing(tmp.path(), &[&alpha]);
    let raw = outside.to_str().unwrap();
    let out = search(&home, &alpha, &["zonk_harvest", "--root", raw]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains(raw) && err.contains("not inside a known repo"),
        "{err}"
    );
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
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

/// Catches: the CLI and MCP disagreeing on transcripts. `*` must not print a
/// `claude_sessions` hit; `*:all` must.
#[test]
fn a_star_leaves_claude_sessions_out_and_star_all_includes_them() {
    let tmp = TempDir::new().unwrap();
    let alpha = store(tmp.path(), "alpha", "unrelated_thing");
    {
        let ctx = Context::open(&alpha).unwrap();
        let now = chrono::Utc::now().timestamp();
        mdkb::store::collections::add_collection(
            &ctx.conn,
            &mdkb::domain::Collection {
                name: "claude_sessions".to_string(),
                path: "./no-such-dir".to_string(),
                pattern: "**/*".to_string(),
                source: mdkb::domain::COLLECTION_SOURCE_SESSIONS.to_string(),
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();
        let content = "Transcript where we discussed ledger_sync at length.";
        let doc = mdkb::domain::Document {
            id: 0,
            collection: "claude_sessions".to_string(),
            relative_path: "sid-chunk-001".to_string(),
            hash: mdkb::store::documents::compute_hash(content),
            title: Some("Session about ledger_sync".to_string()),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
            status: Some("current".to_string()),
        };
        mdkb::store::documents::index_document(&ctx.conn, &doc, content).unwrap();
    }
    let home = home_knowing(tmp.path(), &[&alpha]);

    let default = search(
        &home,
        &alpha,
        &["ledger_sync", "--scope", "docs", "--root", "*"],
    );
    assert!(default.status.success(), "{default:?}");
    assert!(
        !stdout(&default).contains("sid-chunk-001"),
        "{}",
        stdout(&default)
    );

    let all = search(
        &home,
        &alpha,
        &["ledger_sync", "--scope", "docs", "--root", "*:all"],
    );
    assert!(all.status.success(), "{all:?}");
    assert!(stdout(&all).contains("sid-chunk-001"), "{}", stdout(&all));
}

/// Catches: the CLI skipping the scope rule the MCP side applies. `--root '*'`
/// from a work directory must not print the home repo's hit and must say it left
/// one out; `'*:all'` must print both.
#[test]
fn a_star_leaves_out_the_other_scope_and_says_so_while_star_all_mixes() {
    let tmp = TempDir::new().unwrap();
    let scoped = mdkb::domain::canonicalize_plain(tmp.path())
        .unwrap()
        .join("scoped");
    let home_repo = store(&scoped.join("home"), "h", "zonk_harvest");
    let work_repo = store(&scoped.join("work"), "w", "zonk_harvest");
    let home = home_knowing(tmp.path(), &[&home_repo, &work_repo]);
    std::fs::write(
        home.join(".mdkb/daemon.toml"),
        format!(
            "[[scopes]]\nprefix = \"{}\"\nscope = \"home\"\n\n[[scopes]]\nprefix = \"{}\"\nscope = \"work\"\n",
            scoped.join("home").display(),
            scoped.join("work").display()
        ),
    )
    .unwrap();

    let own = search(
        &home,
        &work_repo,
        &["zonk_harvest", "--scope", "memory", "--root", "*"],
    );
    assert!(own.status.success(), "{own:?}");
    let text = stdout(&own);
    assert!(text.contains("zonk_harvest"), "control: {text}");
    assert!(!text.contains(&home_repo.display().to_string()), "{text}");
    assert!(
        stderr(&own).contains("Excluded by scope: 1"),
        "{}",
        stderr(&own)
    );

    let mixed = search(
        &home,
        &work_repo,
        &["zonk_harvest", "--scope", "memory", "--root", "*:all"],
    );
    assert!(mixed.status.success(), "{mixed:?}");
    assert!(stdout(&mixed).contains(&home_repo.display().to_string()));
    assert!(!stderr(&mixed).contains("Excluded by scope"));
}
