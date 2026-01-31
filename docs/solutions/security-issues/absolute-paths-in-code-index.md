---
title: Absolute filesystem paths leaked via MCP code search
category: security-issues
tags: [mcp, information-leak, filesystem, code-index, paths]
symptom: MCP tool responses expose full filesystem paths like /Users/username/...
root_cause: code_files.path stored absolute paths, exposed in search results
date: 2026-03-20
---

# Absolute filesystem paths leaked via MCP code search

## Symptom

MCP code search responses contain absolute paths:

```json
{"file_path": "/Users/stefano.straus/Gits/personal/tuicommander/src/main.rs"}
```

This leaks the full filesystem layout to any MCP client.

## Investigation

1. `code_files.path` column stored the absolute path from `reg.path.to_string_lossy()`.
2. `code_symbols.file_path` also stored absolute paths.
3. MCP search responses directly returned these paths.
4. The pipeline already computed `rel_path` via `strip_prefix(root)` but used it only as a secondary column.

## Root Cause

The code indexer used absolute paths as the primary identifier in `code_files.path` and propagated them to symbol records. The relative path was computed but stored in a separate column, while all lookups and MCP responses used the absolute path.

## Solution

Changed `write_batch` to pass `reg.rel_path` as the primary `path` to `db.insert_file()`. Updated all callers (`delete_by_file`, `get_indexed_file_hashes`, incremental mtime comparison) to use relative paths. MCP responses now show project-relative paths.

Additionally, sanitized the semantic search error message to avoid leaking model file paths:

```rust
// Before: raw error forwarded to MCP client
.map_err(|e| mcp_error(format!("Semantic code search failed: {e}")))?;

// After: log internally, generic message to client
.map_err(|e| {
    tracing::error!("Semantic code search failed: {e}");
    mcp_error("Semantic code search failed. The embedding model may not be installed.".to_string())
})?;
```

## Prevention

- [x] Code index now stores relative paths by default
- [x] Error messages sanitized at MCP boundary
- [ ] Audit all `mcp_error` calls for path leakage
- [ ] Add test asserting no absolute paths in MCP responses

## Files Changed

- `src/code/indexing/pipeline.rs` - Use `rel_path` in `write_batch`
- `src/code/indexing/mod.rs` - Convert abs→rel in all DB operations
- `src/code/storage/sqlite.rs` - Updated doc comments and parameter names
- `src/mcp/server.rs` - Sanitized error messages
