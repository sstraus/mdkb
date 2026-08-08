---
id: caching-parser-tree-sitter
title: CachingParser eliminates 6x tree-sitter reparse
entry_type: decision
source_type: user_statement
status: active
tags: [tree-sitter, caching, performance, unsafe-send]
created_at: 1774061995
updated_at: 1774061995
---

Each `find_*` method called `parser.parse(code, None)` independently — 6-7 parses per file. `CachingParser` wraps `tree_sitter::Parser` with FNV-1a hash-keyed single-entry cache. `Tree::clone()` is `ts_tree_copy` (deep copy, cheaper than reparse). Only `unsafe impl Send` needed (not Sync). Raw pointer cache key replaced with content hash to avoid stale cache. See `docs/solutions/performance-issues/tree-sitter-redundant-parsing.md`.
