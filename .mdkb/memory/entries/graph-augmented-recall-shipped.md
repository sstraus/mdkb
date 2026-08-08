---
id: graph-augmented-recall-shipped
title: Graph-augmented automatic recall shipped in 3.5.0 (E1+D1)
entry_type: decision
source_type: user_statement
status: active
tags: [hooks, recall, code-index, graph, pretooluse, userpromptsubmit, release]
created_at: 1782825189
updated_at: 1782825189
---

Plan plans/graph-augmented-recall.md (stories 023-ff9a, 024-e02b). E1: hook_pre_tool_use_impl (now async) injects real code-index file:line for definition Grep/Bash searches via code_index_hits + extract_definition_symbol; falls back to suggestion when unindexed; gated by cfg.code_hits_in_pretooluse. D1: hook_user_prompt_submit_impl injects up to 3 one-hop FRONTMATTER doc-graph neighbors when a prompt names a doc, via doc_graph_neighbors + graph::resolve_to_path (single-resolve, docs-only — entity tags like themes/owner excluded) + path_like_tokens (hook_logic.rs, .md/slash/wikilink, basename fallback, non-md code paths excluded); gated by cfg.doc_graph_in_recall. Plan pseudocode bug: neighbors(KIND_FRONTMATTER) filters relation NAME not source_kind — filter source_kind in Rust. Deploy insight: ~/.local/bin/mdkb is a SYMLINK to target/release/mdkb, so rebuild updates what hooks call, but the running daemon caches old code in memory — must 'mdkb daemon restart' for hooks to serve new behavior (daemon is the primary hook path, not the MDKB_NO_DAEMON fallback). Released v3.5.0.
