---
id: parser-stack-overflow-deep-ast
title: Stack overflow on large JS/TS files in parser
entry_type: problem
source_type: user_statement
status: active
tags: [tree-sitter, stack-overflow, parser, recursion]
created_at: 1774061995
updated_at: 1774061995
---

Default 8MB thread stack + unbounded recursion in 8 `extract_*_recursive` functions caused stack overflow on minified JS. Fix: 16MB stack via `thread::Builder` + `check_recursion_depth` guard on all recursive traversals (TS and Go parsers). See `docs/solutions/runtime-errors/stack-overflow-in-parser.md`.
