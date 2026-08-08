---
id: memory-edges-phase23-shipped
title: Memory edges Phase 2+3 shipped (schema v14)
entry_type: decision
source_type: user_statement
status: active
tags: [memory-graph, schema-v14, edges, provenance, stale-dep, recall-expansion]
created_at: 1783240231
updated_at: 1783240231
---

plans/memory-edges-phase23.md complete (8 stories 025-032). Shipped: memory_edges table (schema v14, FK cascade, dangling target_ref, PK(source_id,target_ref,relation)); src/store/memory_graph.rs (typed edges CRUD, supersedes syncs superseded_by+status in one tx via add_edge_in, has_stale_dependency); MCP memory_write relates=[{relation,target,target_kind}] (entry+edges in one tx), on_conflict=contradicts, graph scope=memory; CLI 'mdkb memory link'; created_session/created_agent provenance (memory::set_provenance/get_provenance, shown in MCP get_impl only, NOT CLI get); post-recall 1-hop expansion (expand_recall_neighbors, cap 2 seeds/3 neighbors, via <relation>); [STALE-DEP] read-only flag in warmup+recall. Relations closed set: supports|contradicts|supersedes|derived_from|relates_to.
