//! `root` accepts a name, a list, or a star — through ONE parser.
//!
//! Story 126-5dab. `root` was a free `Option<String>` on 13 tool parameter
//! structs, parsed ad hoc in `resolve_handle`: an absolute path or nothing,
//! with `"*"` special-cased and rejected everywhere but `search`. A caller who
//! knew a repo by name had to spell its absolute path, and a caller who wanted
//! two repos out of seven had no way to say so.
//!
//! Two rules here are load-bearing and neither guesses:
//!
//! * **A comma is the list separator, always.** A path that contains one is
//!   ambiguous, so it is REFUSED by name rather than split into two selectors
//!   that resolve to nothing. An input that cannot mean one thing must not be
//!   answered as if it did.
//! * **An absolute path always beats a repo name.** The choice is syntactic —
//!   `Path::is_absolute` decides, before anything touches the disk — so a path
//!   to a repo that is not on the map still behaves exactly as it did.

use std::path::{Path, PathBuf};

use mdkb::mcp::tools::{RootSelector, RootTerm};

#[path = "common/cli.rs"]
mod cli;

/// A directory that exists, canonicalized — the spelling the repo map stores.
fn dir(parent: &Path, name: &str) -> PathBuf {
    let root = parent.join(name);
    std::fs::create_dir_all(&root).expect("create dir");
    root.canonicalize().expect("canonicalize")
}

fn parse(raw: &str) -> Result<RootSelector, String> {
    RootSelector::parse(Some(raw))
}

/// Criterion 4: a bare absolute path behaves exactly as today, and that must
/// hold for a root the map has never heard of — the path IS the answer, so
/// resolution may not depend on membership.
#[test]
fn an_absolute_path_resolves_to_itself_even_when_it_is_not_on_the_map() {
    let tmp = tempfile::tempdir().expect("tmp");
    let stranger = dir(tmp.path(), "stranger");

    let selector = parse(&stranger.display().to_string()).expect("an absolute path parses");
    let resolved = selector
        .resolve(&[], &[])
        .expect("an absolute path needs no map");

    assert_eq!(
        resolved,
        vec![stranger],
        "the path is the answer; the map is not consulted"
    );
}

/// Criterion 2: a repo NAME from the map, which is the whole point of the story.
#[test]
fn a_bare_name_resolves_to_the_root_that_carries_it() {
    let tmp = tempfile::tempdir().expect("tmp");
    let alpha = dir(tmp.path(), "alpha");
    let beta = dir(tmp.path(), "beta");
    let known = vec![alpha.clone(), beta];

    let resolved = parse("alpha")
        .expect("a name parses")
        .resolve(&known, &[])
        .expect("a known name resolves");

    assert_eq!(resolved, vec![alpha]);
}

/// Criterion 2: a comma-separated mix of the two, in the order written.
#[test]
fn a_comma_list_mixes_names_and_paths_in_the_order_written() {
    let tmp = tempfile::tempdir().expect("tmp");
    let alpha = dir(tmp.path(), "alpha");
    let stranger = dir(tmp.path(), "stranger");
    let known = vec![alpha.clone()];

    let raw = format!("alpha, {}", stranger.display());
    let resolved = parse(&raw)
        .expect("a mixed list parses")
        .resolve(&known, &[])
        .expect("a mixed list resolves");

    assert_eq!(
        resolved,
        vec![alpha, stranger],
        "order is the order the caller wrote, and whitespace around a comma is not part of a name"
    );
}

/// Criterion 2: `*` is every KNOWN root, not every open one.
#[test]
fn a_star_resolves_to_every_known_root() {
    let tmp = tempfile::tempdir().expect("tmp");
    let alpha = dir(tmp.path(), "alpha");
    let beta = dir(tmp.path(), "beta");
    let known = vec![alpha.clone(), beta.clone()];
    let open = vec![alpha.clone()];

    let resolved = parse("*")
        .expect("a star parses")
        .resolve(&known, &open)
        .expect("a star resolves");

    assert_eq!(
        resolved,
        vec![alpha, beta],
        "the star covers what is known, not what happens to be open"
    );
}

/// Criterion 3: ambiguity is reported with its candidates, never as nothing
/// found. Two checkouts of one project under different parents share a name.
#[test]
fn an_ambiguous_name_lists_the_candidates_instead_of_returning_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = dir(&dir(tmp.path(), "work"), "mdkb");
    let fork = dir(&dir(tmp.path(), "fork"), "mdkb");
    let known = vec![work.clone(), fork.clone()];

    let err = parse("mdkb")
        .expect("a name parses")
        .resolve(&known, &[])
        .expect_err("an ambiguous name is an error, not an empty list");

    assert!(
        err.contains(&work.display().to_string()) && err.contains(&fork.display().to_string()),
        "both candidate roots must be named so the caller can pick one: {err}"
    );
    assert!(
        err.contains("mdkb cheatsheet"),
        "criterion 7: the error points at the grammar: {err}"
    );
}

/// Criterion 3: an unknown name says so. An empty result would read as "that
/// repo has nothing in it", which is the false negative this story shares with
/// 125-7419 and 127-fcaf.
#[test]
fn an_unknown_name_says_so_instead_of_returning_nothing() {
    let tmp = tempfile::tempdir().expect("tmp");
    let alpha = dir(tmp.path(), "alpha");

    let err = parse("nosuchrepo")
        .expect("a name parses")
        .resolve(&[alpha], &[])
        .expect_err("an unknown name is an error, not an empty list");

    assert!(
        err.contains("nosuchrepo"),
        "the name that failed must be quoted back: {err}"
    );
    assert!(
        err.contains("alpha"),
        "and the names that would have worked listed: {err}"
    );
}

/// The comma rule. `/tmp/x/my,repo` is a real directory AND a two-item list,
/// and no rule can be right for both. It is refused, loudly, by name — not
/// silently split into two selectors that resolve to nothing.
#[test]
fn a_path_containing_a_comma_is_refused_not_split() {
    let tmp = tempfile::tempdir().expect("tmp");
    let comma = dir(tmp.path(), "my,repo");

    let raw = comma.display().to_string();
    let err = RootSelector::parse(Some(&raw))
        .expect_err("an input that cannot mean one thing is refused");

    assert!(
        err.contains(&raw),
        "the offending selector is quoted back: {err}"
    );
    assert!(
        err.contains("comma"),
        "and the reason named, so it is fixable: {err}"
    );
    assert!(
        err.contains("mdkb cheatsheet"),
        "criterion 7: the error points at the grammar: {err}"
    );
}

/// An absolute path is classified as a path before anything is looked up, so a
/// repo NAME that happens to be spelled like one cannot shadow it. The rule is
/// syntactic on purpose: it does not depend on what exists on disk.
#[test]
fn an_absolute_path_always_beats_a_name_spelled_the_same_way() {
    let tmp = tempfile::tempdir().expect("tmp");
    let decoy = dir(tmp.path(), "decoy");
    // A known root whose *name* is the full text of another root's path.
    let confusing = decoy.display().to_string();

    let selector = parse(&confusing).expect("parses");
    assert!(
        matches!(selector, RootSelector::List(ref terms) if terms.len() == 1
            && matches!(terms[0], RootTerm::Path(_))),
        "an absolute string is a path term, never a name term"
    );

    let resolved = selector.resolve(&[], &[]).expect("resolves with no map");
    assert_eq!(resolved, vec![decoy]);
}

/// Criterion 5: a tool that cannot fan out refuses a multi-root selector and
/// names the tool that can, the way the old `root="*"` message did.
#[test]
fn a_tool_that_cannot_fan_out_names_the_one_that_can() {
    let message = RootSelector::multi_root_rejection(3);

    assert!(
        message.contains("search"),
        "criterion 5: the tool that CAN fan out must be named: {message}"
    );
    assert!(
        message.contains("mdkb cheatsheet"),
        "criterion 7: the error points at the grammar: {message}"
    );
}

/// Criterion 6 and 10: the grammar lives in `mdkb cheatsheet` and nowhere else,
/// in the shape story 119 left that output in — bare command names, and no
/// machine-specific build path on the grammar lines.
#[test]
fn the_cheatsheet_carries_the_root_grammar() {
    let out = cli::command()
        .arg("cheatsheet")
        .output()
        .expect("run mdkb cheatsheet");
    assert!(out.status.success(), "`mdkb cheatsheet` must exit 0");
    let text = String::from_utf8_lossy(&out.stdout);

    for shape in [
        "root=\"/abs/path\"",
        "root=\"name\"",
        "root=\"name,/abs/path\"",
        "root=\"*\"",
    ] {
        assert!(
            text.contains(shape),
            "the cheatsheet must teach {shape}: {text}"
        );
    }

    let grammar: Vec<&str> = text
        .lines()
        .filter(|l| l.trim_start().starts_with("root="))
        .collect();
    assert!(
        grammar.len() >= 4,
        "the grammar block must be lines, not one paragraph: {grammar:?}"
    );
    let exe = cli::bin().display().to_string();
    for line in &grammar {
        assert!(
            !line.contains(&exe),
            "story 119: no machine-specific build path on a grammar line: {line}"
        );
    }
}
