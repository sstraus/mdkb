---
id: autoheal-memory-loss-tuicommander
title: Autoheal quarantine lost 673 memory entries (tuicommander)
entry_type: problem
source_type: user_statement
status: active
tags: [data-loss]
created_at: 1783755295
updated_at: 1784731535
---

3.7.2 heal.rs quarantines corrupt index.sqlite assuming it is derived data — but memory_entries live only there. tuicommander lost 673 entries + 2381 docs on 2026-07-10; search returned No results with no hint. Recovery: ATTACH corrupt db with immutable=1 (plain ro open fails SQLITE_CANTOPEN), INSERT missing rows by explicit column list (contentless FTS repopulated by memory_ai trigger), then mdkb update for docs. Prevention: story salvage-in-heal; corrupt file kept at .mdkb/index.sqlite.corrupt-1783665818 (2.8GB) until story ships.
