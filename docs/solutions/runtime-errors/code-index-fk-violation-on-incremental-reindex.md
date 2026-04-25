---
title: FOREIGN KEY constraint failure on incremental code reindex
category: runtime-errors
tags: [sqlite, foreign-key, code-index, incremental-reindex, pipeline]
symptom: "Code reindexing failed: FOREIGN KEY constraint failed" on mdkb update
root_cause: Pipeline assigns sequential IDs (1,2,3...) but SQLite rowids diverge after incremental reindex
date: 2026-03-20
---

# FOREIGN KEY constraint failure on incremental code reindex

## Symptom

Running `mdkb update` on a previously indexed repository fails with:

```
WARN mdkb: Code reindexing failed: Error { kind: Other("Indexing failed: FOREIGN KEY constraint failed") }
```

The code index silently produces no results. `scope="code"` searches return empty.

## Investigation

1. The error only happens on **incremental** reindex (not fresh). A fresh `code index` works fine.
2. The `code_symbols` table has `file_id INTEGER NOT NULL REFERENCES code_files(id) ON DELETE CASCADE`.
3. The pipeline's COLLECT stage assigns `file_id` with a counter starting at 1.
4. After deleting stale files and re-indexing, SQLite auto-increments past the old max rowid.
5. The pipeline writes `file_id=1` for symbols, but the actual `code_files.id` is now e.g. 500.

## Root Cause

The indexing pipeline has a COLLECT stage that assigns sequential file/symbol IDs (1, 2, 3...) before the INDEX stage writes to SQLite. On a fresh index, SQLite rowids match the counter. On incremental reindex (delete stale + re-insert), SQLite auto-incremented rowids don't start at 1, causing a mismatch between pipeline-assigned IDs and real database IDs.

The `write_batch` function used pipeline IDs directly as foreign keys, but after incremental operations these IDs pointed to non-existent rows.

## Solution

Build ID remapping in `write_batch`: after each `insert_file` / `insert_symbol`, capture the real SQLite rowid and build a `HashMap<pipeline_id, real_id>`. Use this map when inserting symbols and relationships.

```rust
// pipeline.rs - write_batch()
let mut file_id_map: HashMap<u32, i64> = HashMap::with_capacity(batch.file_registrations.len());

for reg in &batch.file_registrations {
    let real_id = db.insert_file(&reg.rel_path, ...)?;
    file_id_map.insert(reg.file_id.value(), real_id);
}

// When inserting symbols, use remapped file_id
let real_file_id = file_id_map
    .get(&symbol.file_id.value())
    .copied()
    .unwrap_or(i64::from(symbol.file_id.value()));
```

Same pattern for `symbol_id_map` to remap `from_symbol_id` in relationships.

## Prevention

- [x] Regression test `test_incremental_index_after_delete_no_fk_violation` (verified red-then-green)
- [ ] Consider using `RETURNING id` in batch inserts to make ID assignment explicit
- [ ] Add `--force` flag to `code index` for recovery (implemented)

## Related

- `docs/solutions/runtime-errors/stack-overflow-in-parser.md` - discovered during same investigation
- `docs/solutions/security-issues/absolute-paths-in-code-index.md` - also discovered during same investigation

## Files Changed

- `src/code/indexing/pipeline.rs` - ID remapping in `write_batch()`
