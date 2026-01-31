# Code Review: Stale Embeddings Cleanup + Search Behaviour Tests

**Date:** 2026-03-05
**Reviewers:** Multi-Agent (security, performance, architecture, simplicity, silent-failure, test-quality, rust)
**Target:** HEAD~2..HEAD (commits a649665, fbc8df3)
**Confidence Threshold:** 70

## Summary

- **P1 Critical Issues:** 1
- **P2 Important Issues:** 10
- **P3 Nice-to-Have:** 7
- **Filtered Out (below threshold):** 0

## P1 - Critical

- [ ] **[SILENT-FAILURE]** Poisoned mutex silently ignored on 4 write paths in SemanticSearch `src/code/semantic.rs:359,415,505,513` (Confidence: 95)
  - Issue: `if let Ok(mut cache) = self.cache.lock()` silently discards poison on write paths (generate_embeddings, generate_embeddings_incremental, remove_embeddings, clear), while read paths (ensure_cache, search) correctly propagate. Store is mutated even when cache invalidation fails, creating stale-cache split-brain.
  - Fix: Propagate error with `map_err(...)?` on methods returning Result; at minimum add `tracing::warn!` on the else branch.
  - Agents: silent-failure-hunter, rust-reviewer

## P2 - Important

- [ ] **[TEST]** `test_search_results_multiple_get_hint` conditional assertion never fails `tests/e2e_search_behaviour.rs:339` (Confidence: 92)
  - Issue: Assertion wrapped in `if result_lines.len() >= 2` — silently passes when search returns <2 results.
  - Fix: Assert count >= 2 first, then assert unconditionally.
  - Agent: test-quality-reviewer

- [ ] **[TEST]** `SemanticSearch::remove_embeddings` has no unit test `src/code/semantic.rs:500` (Confidence: 95)
  - Issue: Only `VectorStore::remove_by_ids` is tested. The cache-invalidation contract of the public API is untested.
  - Fix: Add test that warms cache, removes, verifies count reflects removal.
  - Agent: test-quality-reviewer

- [ ] **[TEST]** `delete_by_file` embedding cleanup path untested `src/code/indexing/mod.rs:231` (Confidence: 88)
  - Issue: Existing `test_delete_by_file` creates facade without semantic store — the embedding cleanup branch is never reached.
  - Fix: Add `#[ignore]` integration test with real SemanticSearch attached.
  - Agent: test-quality-reviewer

- [ ] **[SILENT-FAILURE]** Query failure in `get_symbol_ids_for_path` silently returns empty set `src/code/indexing/mod.rs:255` (Confidence: 92)
  - Issue: Tantivy query error → empty HashSet → caller skips embedding cleanup → orphaned embeddings.
  - Fix: Add `tracing::error!` with impact message. Consider returning `Result<HashSet<u32>>`.
  - Agent: silent-failure-hunter

- [ ] **[SECURITY]** `truncate_text` panics on multi-byte UTF-8 `src/mcp/server.rs:1652` (Confidence: 88)
  - Issue: Raw byte-offset slice without char boundary check. Any non-ASCII memory title at the cut boundary crashes the MCP server.
  - Fix: Walk back to `is_char_boundary()` before slicing (same pattern already in `format_symbol_text`).
  - Agent: security-reviewer

- [ ] **[PERF]** O(n*m) filter in `store_load_filtered` closure `src/code/indexing/mod.rs:621` (Confidence: 95)
  - Issue: `symbols.iter().any()` inside per-entry closure = O(symbols) per embedding entry.
  - Fix: Build `HashSet<u32>` from changed symbol IDs first → O(1) lookup.
  - Agent: performance-reviewer

- [ ] **[DRY]** Test harness duplicated verbatim across PTY test files `tests/e2e_search_behaviour.rs:32` (Confidence: 97)
  - Issue: `SearchTestHarness` is a near-identical clone of `McpTestHarness` in `e2e_mcp_pty.rs`.
  - Fix: Extract shared harness to `tests/common/mod.rs` with `with_env` constructor.
  - Agent: architecture-reviewer

- [ ] **[RUST]** `as u32` cast from u64 silently truncates `src/code/indexing/mod.rs:263` (Confidence: 85)
  - Issue: `id as u32` relies on write-side invariant invisible at read site.
  - Fix: Use `u32::try_from(id).ok()` to make invariant explicit.
  - Agent: rust-reviewer

- [ ] **[RUST]** `.unwrap()` after `ensure_semantic()` — redundant borrow workaround `src/code/indexing/mod.rs:538,598,657` (Confidence: 88)
  - Issue: `ensure_semantic().is_none()` check + `self.semantic.as_ref().unwrap()` — use `let else` instead.
  - Fix: `let Some(semantic) = self.ensure_semantic() else { return ...; };`
  - Agent: rust-reviewer

- [ ] **[SILENT-FAILURE]** Failed embedding removal logged as `warn` not `error` `src/code/indexing/mod.rs:233` (Confidence: 88)
  - Issue: Index already committed, no rollback possible — orphaned embeddings are permanent until full reindex.
  - Fix: Change to `tracing::error!` with symbol count.
  - Agent: silent-failure-hunter

## P3 - Nice-to-Have

- [ ] **[YAGNI]** `select_base_instructions()` has only one variant `src/mcp/server.rs:1593` (Confidence: 97)
  - Note: Both match arms return BASE_INSTRUCTIONS. Delete when no second variant exists.
  - Agents: simplicity-reviewer

- [ ] **[PERF]** `env::var` called per invocation, could use OnceLock `src/mcp/server.rs:1593` (Confidence: 80)
  - Note: Low impact since only called at startup. OnceLock is cleaner.
  - Agents: rust-reviewer, performance-reviewer

- [ ] **[ARCH]** `delete_by_file` bypasses `ensure_semantic()` without comment `src/code/indexing/mod.rs:232` (Confidence: 80)
  - Note: Correct behavior (avoids loading 300-800MB model for delete), just needs a comment.
  - Agent: architecture-reviewer

- [ ] **[RUST]** `into_iter().filter().collect()` could be `Vec::retain` `src/code/semantic.rs:157` (Confidence: 75)
  - Note: In-place filtering avoids extra allocation.
  - Agent: rust-reviewer

- [ ] **[TEST]** `remove_by_ids` "remove all entries" case untested `src/code/semantic.rs:731` (Confidence: 82)
  - Agent: test-quality-reviewer

- [ ] **[TEST]** `test_search_no_results_message` accepts too many string variants `tests/e2e_search_behaviour.rs:352` (Confidence: 85)
  - Note: Three disjuncts mask which path is actually exercised. Assert exact string.
  - Agent: test-quality-reviewer

- [ ] **[TEST]** 3 instruction tests spawn 3 processes for 1 logical assertion `tests/e2e_search_behaviour.rs:196` (Confidence: 75)
  - Note: Consolidate into one test with multiple asserts on same `initialize()` response.
  - Agent: simplicity-reviewer

## Cross-Cutting Analysis

### Root Causes Identified

| Root Cause | Findings Affected | Suggested Fix |
|------------|-------------------|---------------|
| Inconsistent mutex poison handling | P1 #1 (4 sites) | Standardize on `map_err(...)?` for all Result-returning methods, `tracing::warn!` for void methods |
| Silent empty-set fallback pattern | P2 #4, P2 #10 | Add error logging or propagate errors on failure paths that affect data integrity |
| Test harness duplication | P2 #7, P3 #7 | Extract `tests/common/mod.rs` |
| `as u32` truncation | P2 #8 | Global `try_from` migration (also affects `doc_to_symbol` at 6 other sites) |

### Single-Fix Opportunities

1. **Mutex poison fix** — Fixes P1 finding + consistency across 4 methods (~20 lines)
2. **Test harness extraction** — Fixes P2 DRY + enables consolidating instruction tests (~0 new lines, move existing)
3. **`u32::try_from` migration** — Fixes P2 cast safety across ~8 sites (~16 lines changed)
4. **`truncate_text` char boundary** — Fixes P2 crash bug (~4 lines)

### Context Files (Read Before Fixing)

| File | Reason | Referenced By |
|------|--------|---------------|
| `src/code/indexing/pipeline.rs:646` | Write-side invariant for symbol IDs | rust-reviewer |
| `src/code/types.rs` | SymbolId backed by u32 | rust-reviewer |
| `src/code/storage.rs` | CodeIndex writer lifecycle | performance, architecture |
| `tests/e2e_mcp_pty.rs` | Original test harness to unify with | architecture |

## Recommended Actions

1. **Immediate:** Fix P1 mutex poison (4 sites in semantic.rs)
2. **Immediate:** Fix `truncate_text` UTF-8 panic (server.rs)
3. **This session:** Fix conditional test assertion, add missing test for remove_embeddings
4. **Follow-up:** Extract shared test harness, O(n*m) filter fix, `try_from` migration
