//! `mdkb update` must not report success while a collection has vanished.
//!
//! Story 011-9a41: the `map` collection (2307 docs) disappeared from a store.
//! The next `mdkb update` printed "Docs: 3 indexed (3 new, 0 changed) /
//! Unchanged: 0" and exited 0, having indexed the three root-level files of
//! `_root`. Every graph query returned DocumentNotFound and hybrid search
//! returned only the root docs. Nothing in that output distinguishes a healthy
//! index from one that has lost its main collection; it was found by accident
//! several harvest runs later, when a spot-check query failed.
//!
//! The root cause is fixed separately (autoheal now salvages `collections` —
//! see tests/quarantine_salvage.rs). This file covers the reporting failure that
//! HID it, which is the more important of the two: a cause you can see is a bug,
//! a cause you cannot see is a data-loss event with a delay fuse.

use mdkb::cli::handlers::{handle_collection_add, handle_init, handle_update};
use mdkb::core::Context;

fn store() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    handle_init(&root).expect("init");
    (dir, root)
}

fn seed_collection(ctx: &Context, name: &str, docs: usize) {
    let dir = ctx.root().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    for i in 0..docs {
        std::fs::write(
            dir.join(format!("doc{i}.md")),
            format!("# Doc {i}\n\nbody\n"),
        )
        .unwrap();
    }
    handle_collection_add(ctx, name, name, "**/*.md").expect("add collection");
}

/// The delta has to be per-collection. A single total cannot distinguish "2307
/// documents were re-indexed" from "one collection went to zero and another
/// grew" — which is the exact ambiguity that let 2307 documents vanish behind
/// the word "indexed".
#[test]
fn update_reports_a_per_collection_document_count() {
    let (_dir, root) = store();
    let ctx = Context::open(&root).expect("open");
    seed_collection(&ctx, "map", 4);
    seed_collection(&ctx, "notes", 2);

    let result = handle_update(&ctx, &root).expect("update");
    let mut counts: Vec<(String, usize)> = result
        .collections
        .iter()
        .map(|c| (c.name.clone(), c.documents))
        .collect();
    counts.sort();
    assert_eq!(
        counts,
        vec![("map".to_string(), 4), ("notes".to_string(), 2)],
        "each collection must report its own document count"
    );
}

/// The reported failure itself: a collection present in a previous run and now
/// gone must be named, not silently omitted.
#[test]
fn a_collection_that_disappeared_is_named_in_the_result() {
    let (_dir, root) = store();
    let ctx = Context::open(&root).expect("open");
    seed_collection(&ctx, "map", 4);
    seed_collection(&ctx, "notes", 2);
    handle_update(&ctx, &root).expect("first update");

    // The loss, as autoheal used to produce it: the registration is gone while
    // the files it pointed at are still on disk.
    ctx.conn
        .execute("DELETE FROM collections WHERE name = 'map'", [])
        .expect("drop registration");

    let result = handle_update(&ctx, &root).expect("second update");
    assert_eq!(
        result.collections_vanished,
        vec!["map".to_string()],
        "a collection that had documents last run and is now unregistered must \
         be named — this is the line whose absence hid 2307 lost documents"
    );
    assert!(
        !result.errors.is_empty(),
        "the loss must also reach `errors`, so a caller checking only that field \
         still sees it"
    );
}

/// A store with no document collection at all indexes nothing and, before this,
/// said so in the same words it uses for a healthy no-op run.
#[test]
fn update_warns_when_no_collection_is_registered() {
    let (_dir, root) = store();
    let ctx = Context::open(&root).expect("open");
    ctx.conn
        .execute("DELETE FROM collections", [])
        .expect("clear collections");

    let result = handle_update(&ctx, &root).expect("update");
    assert!(
        result.no_collections_registered,
        "an update against a store with no registered collection must say so — \
         otherwise 'Docs: 0 indexed' reads as 'nothing changed'"
    );
}

/// A healthy run must set neither flag, or they become noise and stop being read.
#[test]
fn a_healthy_update_reports_no_loss() {
    let (_dir, root) = store();
    let ctx = Context::open(&root).expect("open");
    seed_collection(&ctx, "map", 3);

    handle_update(&ctx, &root).expect("first update");
    let result = handle_update(&ctx, &root).expect("second update");
    assert!(result.collections_vanished.is_empty());
    assert!(!result.no_collections_registered);
}

/// `mdkb collection list` is the documented recovery check, so its output has to
/// be assertable from a script rather than eyeballed.
#[test]
fn collection_list_json_is_stable_enough_to_assert_on() {
    let (_dir, root) = store();
    let ctx = Context::open(&root).expect("open");
    seed_collection(&ctx, "map", 2);
    handle_update(&ctx, &root).expect("update");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["--format", "json", "collection", "list"])
        .current_dir(&root)
        .output()
        .expect("run collection list");
    assert!(out.status.success(), "collection list must exit 0");

    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("collection list --format json must emit JSON");
    let names: Vec<&str> = parsed
        .as_array()
        .expect("a JSON array of collections")
        .iter()
        .filter_map(|c| c.get("name").and_then(|n| n.as_str()))
        .collect();
    assert!(
        names.contains(&"map"),
        "the recovery check must let a script confirm a collection is registered; got {names:?}"
    );
}
