---
id: corruption-localized-memory-write-path
title: Memory-only localization was refuted
entry_type: problem
source_type: user_statement
status: superseded
tags: [corruption, sqlite, data-loss, refuted, superseded]
created_at: 1786099131
updated_at: 1786282128
superseded_by: mdkb-3-7-12-writer-recovery
---

Refuted by the 2026-08-09 TUICommander incident: quick_check found duplicate pages in memory_fts_data and idx_call_log_session, wrong entry counts in both call_log indexes, and a malformed memory_fts index. The damage was therefore not confined to the memory write transaction. Superseded by mdkb-3-7-12-writer-recovery, which serializes all main-store writers and evicts corrupt long-lived contexts.
