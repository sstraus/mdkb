---
title: Stack overflow when indexing large JS/TS files
category: runtime-errors
tags: [tree-sitter, stack-overflow, parser, recursion, thread-stack]
symptom: "fatal runtime error: stack overflow, aborting" during mdkb update
root_cause: Default 8MB thread stack + unbounded recursion in parser extraction functions
date: 2026-03-20
---

# Stack overflow when indexing large JS/TS files

## Symptom

Running `mdkb update` on a Tauri project crashes with:

```
fatal runtime error: stack overflow, aborting
```

## Investigation

1. The crash happens in the PARSE stage of the code indexing pipeline.
2. `stage_parse` runs on a `thread::spawn` thread with default 8MB stack.
3. Large minified JS/TS files produce deeply nested ASTs.
4. 8 recursive functions (`extract_calls_recursive`, `extract_method_calls_recursive`, etc.) in the TypeScript and Go parsers lacked depth guards.
5. Only `extract_symbols_from_node` had the `check_recursion_depth` guard.

## Root Cause

Two compounding issues:

1. **Default thread stack**: `thread::spawn` uses 8MB on macOS, insufficient for deeply nested ASTs.
2. **Missing depth guards**: 8 out of 9 recursive traversal functions in TS/Go parsers had no recursion limit.

## Solution

**Layer 1 - Larger stack** (belt):
```rust
thread::Builder::new()
    .name("mdkb-parse".into())
    .stack_size(16 * 1024 * 1024) // 16MB
    .spawn(move || stage_parse(&content_rx, &parsed_tx))?;
```

**Layer 2 - Depth guards** (suspenders):
Added `depth: usize` parameter and `check_recursion_depth(depth, *node)` to all 8 recursive functions in both TypeScript and Go parsers.

## Prevention

- [x] All recursive AST traversal functions now have depth guards
- [x] Parse thread uses 16MB stack
- [ ] Consider converting deepest recursions to iterative (explicit stack) for robustness
- [ ] Add integration test with a deeply nested file

## Files Changed

- `src/code/indexing/pipeline.rs` - 16MB stack for parse thread
- `src/code/parsing/typescript/parser.rs` - depth guards on 4 functions
- `src/code/parsing/go/parser.rs` - depth guards on 4 functions
