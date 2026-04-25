---
title: Each file parsed 6x by tree-sitter in code indexing pipeline
category: performance-issues
tags: [tree-sitter, parsing, caching, performance, code-index]
symptom: Code indexing slower than necessary, tree-sitter parse called 6-7 times per file
root_cause: Each find_* method independently called parser.parse(code, None)
date: 2026-03-20
---

# Each file parsed 6x by tree-sitter in code indexing pipeline

## Symptom

Code indexing pipeline calls tree-sitter `parse()` 6-7 times per file:

```rust
parser.parse(&fc.content, ...)       // symbols
parser.find_imports(&fc.content, ...) // imports
parser.find_calls(&fc.content)        // function calls
parser.find_method_calls(&fc.content) // method calls
parser.find_uses(&fc.content)         // type uses
parser.find_defines(&fc.content)      // method defines
```

Each method independently calls `self.parser.parse(code, None)`.

## Root Cause

The `LanguageParser` trait methods were designed as independent operations. Each internally creates a fresh tree-sitter `Tree` from the source text. No tree sharing between methods.

## Solution

Created `CachingParser` wrapper that caches the last parsed tree, keyed by FNV-1a content hash:

```rust
pub struct CachingParser {
    parser: Parser,
    cached: Option<(u64, Tree)>,
}

impl CachingParser {
    pub fn parse_cached(&mut self, code: &str) -> Option<Tree> {
        let hash = fnv1a_hash(code.as_bytes());
        if let Some((cached_hash, ref tree)) = self.cached {
            if cached_hash == hash {
                return Some(tree.clone()); // ts_tree_copy, cheaper than reparse
            }
        }
        let tree = self.parser.parse(code, None)?;
        self.cached = Some((hash, tree.clone()));
        Some(tree)
    }
}
```

All 13 language parsers updated to use `CachingParser` instead of raw `tree_sitter::Parser`.

**Key design decisions:**
- `Tree::clone()` calls `ts_tree_copy` (deep copy of node array). Cheaper than re-parsing but not free.
- FNV-1a hash instead of raw pointer for cache key (avoids stale cache from pointer reuse).
- `unsafe impl Send` required because `Tree` contains raw pointers. Safe because parsers are single-threaded.
- `Sync` NOT implemented (removed) — only `Send` needed for trait bound.

## Prevention

- [x] CachingParser now default for all parsers
- [ ] Consider storing `&Tree` with interior mutability to avoid clone cost entirely
- [ ] Benchmark actual speedup on large repos

## Files Changed

- `src/code/parsing/caching_parser.rs` - New CachingParser wrapper
- `src/code/parsing/mod.rs` - Module registration
- 13 parser files - `Parser` → `CachingParser`, `parse()` → `parse_cached()`
