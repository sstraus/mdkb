---
id: one-memory-search-on-every-surface
title: One memory search on every surface
entry_type: decision
source_type: auto_extracted
status: active
tags: [mdkb, retrieval, memory, architecture]
created_at: 1789575377
updated_at: 1789575377
---

Story 084. Three engines answered one question: CLI search --scope memory was plain token-AND FTS, MCP search(scope=memory) token-AND plus vector, the hook OR-expansion plus vector. A paraphrase the hook recalled perfectly returned nothing on the CLI. Now: store::search::build_recall_query is the single expression (moved out of cli::hook_logic - a store function must not depend on the CLI layer), store::memory::search_entries_recall is the single engine, and search_entries_hybrid (the token-AND wrapper) is deleted. handle_memory_search embeds best-effort via llm::get_cached_service and reads [search.memory] from ctx.config_path. OR-expansion is only safe because story 083 made admission absolute: the expression generates candidates, it does not decide relevance - widening it without the floor is exactly how an unrelated prompt used to inject its best BM25 hit. Remaining divergence, marked DEFERRED in src/main.rs: --entry-type still selects an FTS-only path (search_entries_by_type), because unifying it needs an entry_type filter threaded through search_entries_hybrid_fts.
