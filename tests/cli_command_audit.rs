//! Regressions from the 2026-11-21 command audit (docs/command-audit-2026-11-21.md).
//!
//! Each test drives the real binary on a small repository with documents,
//! frontmatter identities, a two-file clone, and memory entries, and names the
//! story whose defect it pins. Invoke with `cargo test --test cli_command_audit`.

use std::path::{Path, PathBuf};
use std::process::Output;

#[path = "common/cli.rs"]
mod cli;
use cli::run;

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn ok(out: Output, label: &str) -> String {
    assert!(
        out.status.success(),
        "{label}: exit={:?}\n{}",
        out.status.code(),
        text(&out)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn fails(out: Output, label: &str) -> String {
    assert!(!out.status.success(), "{label} must fail:\n{}", text(&out));
    text(&out)
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        write(
            &root,
            "docs/people/alice.md",
            "---\nid: person:alice\naliases: [\"@alice\"]\ntitle: Alice\n---\n# Alice\nOwns billing.\n",
        );
        write(
            &root,
            "docs/adr/0001-queues.md",
            "---\ntitle: Use a queue\nowner: person:alice\n---\n# ADR 1\nRetry card charges through a durable queue.\n",
        );
        write(
            &root,
            "docs/adr/0002-outbox.md",
            "---\ntitle: Use an outbox\nowner: \"@alice\"\n---\n# ADR 2\nThe outbox replaces the retry queue.\n",
        );
        write(
            &root,
            "src/billing.py",
            "def charge(card, amount):\n    total = 0\n    for i in range(3):\n        total += amount * i\n        if total > 100:\n            total -= 1\n    return send(card, total)\n\n\ndef send(card, total):\n    return {\"card\": card, \"total\": total}\n",
        );
        write(
            &root,
            "src/refund.py",
            "from billing import send\n\ndef refund(card, amount):\n    total = 0\n    for i in range(3):\n        total += amount * i\n        if total > 100:\n            total -= 1\n    return send(card, -total)\n",
        );
        ok(run(&["init"], &root), "init");
        ok(run(&["update"], &root), "update");
        Repo { _dir: dir, root }
    }

    fn run(&self, args: &[&str]) -> Output {
        run(args, &self.root)
    }
}

/// Story 069-cf5c: a reversed supersede hid both documents from search.
/// Story 074-55bc: evolution commands accept paths without `.md`.
#[test]
fn evolve_refuses_cycles_and_resolves_paths_like_get() {
    let repo = Repo::new();
    let created = ok(
        repo.run(&[
            "evolve",
            "link",
            "adr/0002-outbox",
            "supersedes",
            "adr/0001-queues",
            "-r",
            "outbox",
        ]),
        "evolve without .md",
    );
    assert!(created.contains("supersedes"), "{created}");

    let err = fails(
        repo.run(&[
            "evolve",
            "link",
            "adr/0001-queues",
            "supersedes",
            "adr/0002-outbox",
        ]),
        "reversed supersede",
    );
    assert!(err.contains("evolution cycle"), "{err}");

    let current = ok(
        repo.run(&["evolve", "current", "adr/0001-queues"]),
        "current",
    );
    assert!(current.contains("adr/0002-outbox.md"), "{current}");

    let by = ok(
        repo.run(&["evolve", "superseded-by", "adr/0001-queues"]),
        "superseded-by",
    );
    assert!(
        by.contains("adr/0002-outbox.md") && by.contains("Use an outbox"),
        "{by}"
    );

    let history = ok(
        repo.run(&["evolve", "history", "adr/0001-queues"]),
        "history",
    );
    let edge = history
        .split_whitespace()
        .find_map(|w| w.strip_prefix('#').and_then(|n| n.parse::<i64>().ok()))
        .unwrap_or_else(|| panic!("history prints the edge id: {history}"));

    ok(
        repo.run(&["evolve", "unlink", &edge.to_string()]),
        "evolve remove",
    );
    let found = ok(
        repo.run(&["search", "durable queue", "--scope", "docs"]),
        "search",
    );
    assert!(
        found.contains("adr/0001-queues.md"),
        "the removed supersede no longer hides it: {found}"
    );
}

/// Story 070-0da0: rename failed on a foreign key once the collection held documents.
/// Story 072-e8e9: `-c` with an unknown collection and `collection remove` of a
/// missing name exited 0.
#[test]
fn collection_rename_carries_documents_and_unknown_names_fail() {
    let repo = Repo::new();
    ok(
        repo.run(&["collection", "rename", "docs", "notes"]),
        "rename with documents",
    );

    let found = ok(
        repo.run(&["search", "outbox", "-c", "notes", "--scope", "docs"]),
        "search new name",
    );
    assert!(found.contains("0002-outbox"), "{found}");

    let err = fails(
        repo.run(&["search", "outbox", "-c", "docs"]),
        "search old name",
    );
    assert!(
        err.contains("collection not found: docs") && err.contains("notes"),
        "{err}"
    );

    fails(
        repo.run(&["collection", "remove", "nope"]),
        "remove missing collection",
    );
}

/// Story 071-eb7b: symbol listings printed tree-sitter rows (0-based) in
/// `path:line` form. `charge` is on line 1 and `send` on line 10.
#[test]
fn symbol_listings_print_one_based_lines() {
    let repo = Repo::new();
    let find = ok(repo.run(&["code", "find", "charge"]), "code find");
    assert!(find.contains("src/billing.py:1 "), "{find}");

    let callers = ok(repo.run(&["code", "callers", "send"]), "code callers");
    assert!(callers.contains("src/billing.py:10)"), "{callers}");
    assert!(callers.contains("src/refund.py:3)"), "{callers}");
}

/// Story 073-504b: `dup --file` filtered the comparison set and missed the clone.
#[test]
fn dup_file_anchors_on_the_file() {
    let repo = Repo::new();
    let out = ok(
        repo.run(&["code", "dup", "--file", "src/refund.py"]),
        "dup --file",
    );
    assert!(
        out.contains("src/billing.py") && out.contains("src/refund.py"),
        "{out}"
    );

    let none = ok(
        repo.run(&["code", "dup", "--file", "lib/"]),
        "dup --file with no symbols",
    );
    assert!(none.contains("No indexed symbol under `lib/`"), "{none}");
}

/// Story 075-b14e: backlinks matched only the literal target string.
#[test]
fn graph_backlinks_resolve_identities() {
    let repo = Repo::new();
    for entity in ["people/alice", "person:alice", "@alice"] {
        let out = ok(repo.run(&["graph", "backlinks", entity]), entity);
        assert!(
            out.contains("adr/0001-queues.md") && out.contains("adr/0002-outbox.md"),
            "backlinks {entity}: {out}"
        );
    }
}

/// Story 076-7514: `memory list` showed superseded entries with no marker.
/// Story 077-1b6c: `--dry-run` skipped the duplicate check.
/// Story 072-e8e9: a missing memory id must fail with its own message.
#[test]
fn memory_list_hides_superseded_and_dry_run_checks_duplicates() {
    let repo = Repo::new();
    ok(
        repo.run(&[
            "memory",
            "add",
            "old-way",
            "-t",
            "Retries use a queue",
            "-c",
            "Queue retries.",
        ]),
        "add old",
    );
    ok(
        repo.run(&[
            "memory",
            "add",
            "new-way",
            "-t",
            "Retries use an outbox",
            "-c",
            "Outbox retries.",
        ]),
        "add new",
    );
    ok(
        repo.run(&["memory", "link", "new-way", "supersedes", "old-way"]),
        "supersede",
    );

    let active = ok(repo.run(&["memory", "list"]), "memory list");
    assert!(
        active.contains("new-way") && !active.contains("old-way"),
        "{active}"
    );

    let all = ok(
        repo.run(&["memory", "list", "--status", "all"]),
        "memory list --status all",
    );
    assert!(
        all.contains("old-way") && all.contains("superseded"),
        "{all}"
    );

    let show = ok(repo.run(&["memory", "show", "old-way"]), "memory show");
    assert!(
        show.contains("Status: superseded") && show.contains("new-way"),
        "{show}"
    );

    let dup = fails(
        repo.run(&[
            "memory",
            "add",
            "twin",
            "-t",
            "Retries use an outbox",
            "-c",
            "x",
            "--dry-run",
        ]),
        "dry-run duplicate",
    );
    assert!(dup.contains("Near-duplicate entry exists"), "{dup}");

    let missing = fails(
        repo.run(&["memory", "link", "nope", "relates_to", "new-way"]),
        "link missing id",
    );
    assert!(
        missing.contains("memory entry not found: nope"),
        "{missing}"
    );
    assert!(!missing.contains("invalid search query"), "{missing}");
    fails(repo.run(&["memory", "rm", "nope"]), "rm missing id");
}

/// Story 078-fba4: the CLI graph could not read the memory graph or another repo.
#[test]
fn graph_reads_the_memory_graph_and_one_named_root() {
    let repo = Repo::new();
    ok(
        repo.run(&[
            "memory",
            "add",
            "outbox-choice",
            "-t",
            "Outbox replaces queue",
            "-c",
            "We moved retries to an outbox.",
        ]),
        "add",
    );
    ok(
        repo.run(&[
            "memory",
            "link",
            "outbox-choice",
            "derived_from",
            "adr/0002-outbox.md",
            "--doc",
        ]),
        "link doc",
    );

    let links = ok(
        repo.run(&["graph", "links", "outbox-choice", "--scope", "memory"]),
        "memory links",
    );
    assert!(
        links.contains("--derived_from--> adr/0002-outbox.md"),
        "{links}"
    );
    let back = ok(
        repo.run(&[
            "graph",
            "backlinks",
            "adr/0002-outbox.md",
            "--scope",
            "memory",
        ]),
        "memory backlinks",
    );
    assert!(back.contains("outbox-choice"), "{back}");

    let other = tempfile::tempdir().unwrap();
    let root = repo.root.display().to_string();
    let remote = ok(
        run(
            &["graph", "--root", &root, "links", "adr/0002-outbox"],
            other.path(),
        ),
        "graph --root",
    );
    assert!(remote.contains("--owner--> @alice"), "{remote}");
}

/// Story 080-4dae: `update` printed this run's counts as totals, and
/// `graph relations` read as if allowlist keys were ignored.
#[test]
fn status_outputs_label_run_counts_and_extracted_keys() {
    let repo = Repo::new();
    let again = ok(repo.run(&["update"]), "second update");
    assert!(
        again.contains("Files reindexed:") && again.contains("Symbols written:"),
        "{again}"
    );
    assert!(again.contains("`mdkb status` prints totals"), "{again}");

    let relations = ok(repo.run(&["graph", "relations"]), "graph relations");
    assert!(relations.contains("| Detected |"), "{relations}");
    assert!(
        relations.contains("Extracted as edges (graph.frontmatter_relations): owner"),
        "{relations}"
    );
}

/// Story 081-1642. Catches `coupling --docs` dropping documents like plain
/// `coupling` does, reporting a changelog as a document, and a drift count
/// that ignores source commits made after the document last changed.
#[test]
fn coupling_docs_pairs_a_document_with_its_source_and_marks_drift() {
    let repo = Repo::new();
    let git = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo.root)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    for i in 0..5 {
        write(
            &repo.root,
            "docs/adr/0001-queues.md",
            &format!("# ADR 1\nrev {i}\n"),
        );
        write(&repo.root, "docs/CHANGES.md", &format!("# Changes\n{i}\n"));
        write(
            &repo.root,
            "src/billing.py",
            &format!("def charge():\n    return {i}\n"),
        );
        git(&["add", "-A"]);
        git(&["commit", "-qm", &format!("together {i}")]);
    }
    for i in 0..2 {
        write(
            &repo.root,
            "src/billing.py",
            &format!("def charge():\n    return {}\n", 10 + i),
        );
        git(&["add", "-A"]);
        git(&["commit", "-qm", &format!("source only {i}")]);
    }
    ok(repo.run(&["update"]), "update");
    ok(repo.run(&["update", "--code"]), "code index");

    let out = ok(
        repo.run(&["graph", "coupling", "--format", "json"]),
        "coupling --docs",
    );
    let report: serde_json::Value = serde_json::from_str(&out).unwrap();
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report}");
    assert_eq!(findings[0]["doc"], "docs/adr/0001-queues.md", "{report}");
    assert_eq!(findings[0]["source"], "src/billing.py", "{report}");
    assert_eq!(findings[0]["cochanges"], 5, "{report}");
    assert_eq!(findings[0]["source_commits_after_doc"], 2, "{report}");
}
