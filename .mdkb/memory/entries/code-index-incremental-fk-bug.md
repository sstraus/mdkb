---
id: code-index-incremental-fk-bug
title: Code index FK violation on incremental reindex
entry_type: problem
source_type: user_statement
status: active
tags: [sqlite, foreign-key, code-index, pipeline, incremental-reindex]
created_at: 1774061994
updated_at: 1774061994
---

Pipeline COLLECT stage assigns sequential file/symbol IDs (1,2,3...) but after incremental reindex (delete stale + re-insert), SQLite auto-incremented rowids don't match. Fix: `write_batch` builds `HashMap<pipeline_id, real_rowid>` from `insert_file`/`insert_symbol` return values and remaps all FK references. Regression test: `test_incremental_index_after_delete_no_fk_violation`. See `docs/solutions/runtime-errors/code-index-fk-violation-on-incremental-reindex.md`.
