---
id: code-index-relative-paths
title: Code index uses relative paths (not absolute)
entry_type: decision
source_type: user_statement
status: active
tags: [code-index, security, mcp, relative-paths]
created_at: 1774061995
updated_at: 1774061995
---

`code_files.path` and `code_symbols.file_path` now store project-relative paths. All callers (`delete_by_file`, `get_indexed_file_hashes`, mtime comparison) converted to use relative paths. MCP error messages sanitized to avoid leaking filesystem paths. See `docs/solutions/security-issues/absolute-paths-in-code-index.md`.
