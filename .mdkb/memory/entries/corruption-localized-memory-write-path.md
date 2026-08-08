---
id: corruption-localized-memory-write-path
title: index.sqlite corruption is confined to the memory write path
entry_type: problem
source_type: user_statement
status: active
tags: [corruption, sqlite, data-loss]
created_at: 1786099131
updated_at: 1786099131
---

Mapped sqlite_master.rootpage on 5 quarantined files from 3 projects (itview, agent2, tuicommander). Damage is ALWAYS the same class — 'Tree N page P cell C: 2nd reference to page X', i.e. two b-tree cells claiming one overflow page — and ALWAYS confined to memory_entries (root 16), memory_fts_data (root 23), memory_embeddings (root 66), occasionally content (root 5). Never the document FTS, never the vec0 shadow tables, never code.sqlite. These are exactly the tables one memory write touches in a single transaction (store_memory_embedding, src/store/vectors.rs:559-573), all blob-heavy so all overflow-page allocators.

Ruled out this session: statically linked SQLite 3.51 (no system libsqlite3), mmap (removed Jul 7), auto_vacuum (NONE), incremental_vacuum (no caller).

Two live hypotheses, neither proven: (a) sqlite-vec 0.1.9 (a C extension) scribbling on neighbouring pager pages — index.sqlite has vec0 and corrupts, code.sqlite has none and never has; (b) multi-process concurrency, though SQLite WAL is designed to be safe there, so this requires something breaking its locking.

Amplifier found and fixed: get_entry (src/store/memory.rs:668) bumps access_count on every READ, which fired the unscoped memory_au trigger and rewrote the entry's FTS5 segments — every memory read was the store's heaviest writer, on memory_fts_data. Trigger scoped to AFTER UPDATE OF id,title,content,tags in schema v18.

Post-mortem was previously impossible because the quarantine report recorded only salvage counts. It now records quick_check output, damaged table names, db/WAL sizes and the detecting pid+version.
