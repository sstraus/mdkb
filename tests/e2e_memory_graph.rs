//! Integration coverage for the schema-v14 memory graph at the MCP tool boundary.
//!
//! Drives a real `mdkb serve` process over JSON-RPC (no internal shortcuts):
//! `memory_write(relates=[...])` typed-edge creation, `graph(scope="memory")`
//! link/backlink/relation-filter queries, unknown-relation rejection, and
//! `on_conflict="contradicts"` conflict recording. Closes review finding TEST-2.

mod common;

use common::McpTestHarness;
use serde_json::{Value, json};

/// Seed a plain topic memory, asserting the write succeeded.
fn seed_topic(h: &mut McpTestHarness, id: &str, title: &str, content: &str) {
    let r = h.call_tool(
        "memory_write",
        json!({ "id": id, "title": title, "content": content, "entry_type": "topic" }),
    );
    assert!(r.get("error").is_none(), "seed {id} failed: {r}");
}

fn graph_text(h: &mut McpTestHarness, args: Value) -> String {
    McpTestHarness::get_text_content(&h.call_tool("graph", args))
}

#[test]
fn memory_write_relates_creates_typed_edges_queryable_via_graph_scope_memory() {
    let mut h = McpTestHarness::new();
    h.initialize();

    // Two edge targets whose content is distinct from the source, so they can
    // only surface through the typed edges — never accidental text matching.
    seed_topic(
        &mut h,
        "graph-dst",
        "Connection pool sizing",
        "The connection pool caps at twenty simultaneous connections.",
    );
    seed_topic(
        &mut h,
        "graph-note",
        "Retry backoff policy",
        "Exponential backoff with jitter guards the retry loop.",
    );

    // The source declares two differently-typed edges at write time.
    let written = h.call_tool(
        "memory_write",
        json!({
            "id": "graph-src",
            "title": "OAuth token store design",
            "content": "OAuth tokens live in the session store with rotation.",
            "entry_type": "decision",
            "relates": [
                {"relation": "supports", "target": "graph-dst"},
                {"relation": "relates_to", "target": "graph-note"},
            ],
        }),
    );
    assert!(
        written.get("error").is_none(),
        "relates write must succeed: {written}"
    );

    // Outgoing edges: both targets appear, each annotated with its relation.
    let links = graph_text(
        &mut h,
        json!({"entity": "graph-src", "scope": "memory", "direction": "links"}),
    );
    assert!(
        links.contains("graph-dst") && links.contains("supports"),
        "supports edge to graph-dst must be listed: {links}"
    );
    assert!(
        links.contains("graph-note") && links.contains("relates_to"),
        "relates_to edge to graph-note must be listed: {links}"
    );

    // The relation filter narrows to the single matching edge.
    let only_supports = graph_text(
        &mut h,
        json!({"entity": "graph-src", "scope": "memory", "direction": "links", "relation": "supports"}),
    );
    assert!(
        only_supports.contains("graph-dst"),
        "relation=supports must keep the supports edge: {only_supports}"
    );
    assert!(
        !only_supports.contains("graph-note"),
        "relation=supports must drop the relates_to edge: {only_supports}"
    );

    // Backlinks resolve from the target back to the source.
    let backlinks = graph_text(
        &mut h,
        json!({"entity": "graph-dst", "scope": "memory", "direction": "backlinks"}),
    );
    assert!(
        backlinks.contains("graph-src"),
        "backlink from graph-dst to graph-src must resolve: {backlinks}"
    );
}

#[test]
fn memory_write_rejects_an_unknown_relation() {
    let mut h = McpTestHarness::new();
    h.initialize();

    // An unparseable relation must reject the whole write — no half-written entry,
    // no silently-dropped edge.
    let r = h.call_tool(
        "memory_write",
        json!({
            "id": "bad-relation",
            "title": "bad relation",
            "content": "x",
            "relates": [{"relation": "entangles", "target": "graph-dst"}],
        }),
    );
    assert!(
        r.get("error").is_some(),
        "an unknown relation must reject the write: {r}"
    );
}

#[test]
#[ignore = "requires ONNX model download; run with: cargo test --test e2e_memory_graph -- --ignored"]
fn on_conflict_contradicts_writes_entry_and_records_a_contradicts_edge() {
    let mut h = McpTestHarness::new();
    h.initialize();

    // Near-duplicate detection is embedding-based, so both writes need the model.
    let content =
        "The primary database runs Postgres 15 with logical replication to a read replica.";
    let original = h.call_tool(
        "memory_write",
        json!({
            "id": "dup-original",
            "title": "Primary DB topology",
            "content": content,
            "entry_type": "decision",
        }),
    );
    assert!(
        original.get("error").is_none(),
        "seed of the original must succeed: {original}"
    );

    // The same claim, opted into contradiction: written (not rejected) and linked.
    let contradiction = h.call_tool(
        "memory_write",
        json!({
            "id": "dup-contradiction",
            "title": "Primary DB topology (revised)",
            "content": content,
            "entry_type": "decision",
            "on_conflict": "contradicts",
        }),
    );
    assert!(
        contradiction.get("error").is_none(),
        "on_conflict=contradicts must NOT reject the near-duplicate: {contradiction}"
    );
    let msg = McpTestHarness::get_text_content(&contradiction);
    assert!(
        msg.contains("contradicts"),
        "the write output must report the contradicts edge: {msg}"
    );

    // The edge is queryable: dup-contradiction --contradicts--> dup-original.
    let links = graph_text(
        &mut h,
        json!({"entity": "dup-contradiction", "scope": "memory", "direction": "links", "relation": "contradicts"}),
    );
    assert!(
        links.contains("dup-original"),
        "the contradicts edge to the original must be queryable: {links}"
    );
}
