---
id: cli-memory-search-has-no-vector-leg
title: CLI search --scope memory has no vector leg
entry_type: problem
source_type: auto_extracted
status: active
tags: [search, cli, memory, parity, hybrid]
created_at: 1789564891
updated_at: 1789564891
---

Measured 2026-09-16. main.rs Some("memory") calls core::memory::handle_memory_search -> memory::search_entries -> search_entries_fts: token-AND FTS5 only. MCP search scope=memory uses search_entries_hybrid (token-AND BM25 + vector). Hook recall uses search_entries_hybrid_fts with OR-expansion. Three surfaces, three semantics. Live proof: 'provider API rejected the configured model with an HTTP error' returns nothing on the CLI memory scope and the default scope, while the hook path finds prior-mining-dead-distiller-400. The retrieval eval measures only search_entries_hybrid_fts, so it cannot see this. README claims hybrid for memory.
