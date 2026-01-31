---
title: UNIQUE constraint on duplicate symbols breaks code reindex
category: runtime-errors
tags: [sqlite, unique-constraint, code-index, javascript, parser]
symptom: "UNIQUE constraint failed: code_symbols.name, code_symbols.file_id, code_symbols.line_start" on code reindex
root_cause: JS/TS parser emits duplicate (name, file_id, line_start) tuples for same-line redeclarations
date: 2026-04-08
---

# UNIQUE constraint on duplicate symbols breaks code reindex

## Symptom

Code reindexing fails silently at MCP server startup or explicitly via CLI:

```
Error: Indexing failed: UNIQUE constraint failed: code_symbols.name, code_symbols.file_id, code_symbols.line_start
```

The startup error is logged via `tracing::error!` but the server continues with a stale index.

## Root Cause

The TypeScript/JavaScript parser's `process_variable_declaration` iterates all `variable_declarator` children. When two declarators share the same name on the same line, two symbols with identical `(name, file_id, line_start)` are emitted.

JS patterns that trigger this:
- Same-line redeclarations (common in minified code): `const r = fetch(); const r = process();`
- `var` with duplicate names: `var a = 1, a = 2;` (legal JS)
- Bundler output with reused variable names on single lines

## Solution

Changed `INSERT INTO code_symbols` to `INSERT OR REPLACE INTO code_symbols` in `insert_symbol()`. This is defensive across all parsers, not just JS:
- `OR REPLACE` does DELETE + INSERT, so FTS triggers fire correctly
- Last symbol wins (keeps more complete info if any difference)
- No parser-side changes needed across 14 language parsers

## Files Changed

- `src/code/storage/sqlite.rs` - `INSERT` → `INSERT OR REPLACE` in `insert_symbol()`
- `src/code/storage/schema.rs` - Updated test to verify replace behavior instead of reject
