---
id: mdkb-3-7-12-writer-recovery
title: 3.7.12 writer recovery protocol
entry_type: decision
source_type: user_statement
status: active
tags: [mdkb, sqlite, corruption, recovery, resolved]
created_at: 1786282128
updated_at: 1786282128
---

Released and verified on 2026-08-09. Every index.sqlite writer now uses one cross-process writer-admission lock; long-lived daemon reads and writes evict their Context on typed SQLite corruption; post-mutation verification uses a fresh connection; integrity markers are invalidated by writes and must be at least as new as the DB and WAL. Future-schema refusal runs read-only before recovery. TUICommander recovery preserved 893/893 memory entries, rebuilt both FTS indexes, and passed quick_check, integrity_check, FTS5 integrity checks, daemon-routed add/search/delete, full E2E, Windows CI, and release CI. The first low-level page-corruption trigger was not proven, but the observed multi-writer and recovery deadlocks are closed operationally.
