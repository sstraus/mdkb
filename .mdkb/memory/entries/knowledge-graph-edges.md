---
id: knowledge-graph-edges
title: "Knowledge graph: edges from frontmatter + wikilinks"
entry_type: decision
source_type: user_statement
status: active
tags: [graph, edges, mcp, schema-v11, code-graph-pattern]
created_at: 1781008183
updated_at: 1781008183
---

store 005-ceaa. New 'edges' table (schema v11, FK source_doc_id only, target_ref free text). KEY DECISIONS: (1) target_ref stored verbatim; resolved to documents.id at QUERY TIME via ref_forms matching (x <-> x.md <-> ./x) — no persisted target_doc_id, no backfill; dangling edges survive and resolve when target indexed later. (2) idempotent re-index: process_graph_edges (cli/handlers.rs) calls graph::delete_edges_for_source before re-inserting. (3) neighbors/path are UNDIRECTED BFS (outgoing union incoming); links/backlinks directional. (4) MCP: ONE consolidated 'graph' tool with direction=links|backlinks|neighbors|path (mirrors code_graph's calls/callers/impact) — min always-on surface; path adds optional 'to' field, fixed hop limit 6 on MCP, CLI exposes --max-hops. (5) graph allowlist (config [graph].frontmatter_relations, default owner/stakeholders/themes/related) must NOT overlap evolution's reserved keys. store/graph.rs is the module; evolution.rs was the blueprint.
