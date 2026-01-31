# Code Review: P3 Stories (Memory Condense + A/B Experiments)
**Date:** 2026-01-31
**Reviewers:** Multi-Agent (security, performance, architecture, simplicity, rust, data-safety)
**Target:** Commits 6848695, b661cbd (stories 007-cg8h, 017-mq8r)

## Summary
- **P1 Critical Issues:** 5
- **P2 Important Issues:** 11
- **P3 Nice-to-Have:** 8

---

## P1 - Critical (Block Merge / Must Fix)

- [ ] **[DATA-001]** Memory condense lacks transaction boundary
  - Location: `src/cli/handlers.rs:1255-1285`
  - Issue: Creates merged entry, then marks originals as superseded in separate operations. Crash between = orphaned/duplicate data
  - Fix: Wrap entire condense operation in `begin_transaction()/commit_transaction()`
  - Agent: data-safety-reviewer

- [ ] **[RUST-001]** Silent error swallowing in query_map results
  - Location: `src/store/stats.rs:240-241, 311-312, 341-342, 615-616, 1073-1074`
  - Issue: `.filter_map(|r| r.ok())` silently discards SQLite errors
  - Fix: Log errors before filtering: `.filter_map(|r| r.map_err(|e| tracing::warn!("Row error: {}", e)).ok())`
  - Agent: rust-reviewer

- [ ] **[SEC-002]** No size limit on JSON experiment configs
  - Location: `src/cli/handlers.rs:2325-2328`, `src/store/stats.rs:983-1003`
  - Issue: Multi-megabyte JSON configs could cause memory/database exhaustion
  - Fix: Add `const MAX_CONFIG_SIZE: usize = 10_000;` and validate before parsing
  - Agent: security-reviewer

- [ ] **[PERF-001]** N+1 query pattern in get_experiment_status()
  - Location: `src/store/stats.rs:1320-1321`
  - Issue: Calls get_variant_stats() twice, each with 4-6 queries = 8-12 queries per status check
  - Fix: Single JOIN query: `SELECT variant, COUNT(*), AVG(score), ... FROM experiment_results GROUP BY variant`
  - Agent: performance-reviewer

- [ ] **[PERF-002]** O(n²) tag intersection in find_common_tags()
  - Location: `src/cli/handlers.rs:1130-1133`
  - Issue: HashSet intersection in loop for each entry
  - Fix: Precompute tag frequencies, filter before intersection
  - Agent: performance-reviewer

---

## P2 - Important (Should Fix)

- [ ] **[SEC-003]** Experiment name lacks length/format validation
  - Location: `src/cli/handlers.rs:2317`, `src/store/stats.rs:983`
  - Issue: Unlimited length names, no format restriction
  - Fix: `const MAX_NAME_LENGTH: usize = 100;` + alphanumeric validation
  - Agent: security-reviewer

- [ ] **[SEC-005]** Unbounded tag processing in memory entries
  - Location: `src/cli/handlers.rs:913-927`
  - Issue: No limits on tag count or individual tag length
  - Fix: `const MAX_TAGS: usize = 10; const MAX_TAG_LENGTH: usize = 50;`
  - Agent: security-reviewer

- [ ] **[SEC-006]** Memory entry content has no size limit
  - Location: `src/cli/handlers.rs:913-927`, `src/main.rs:164-169`
  - Issue: `read_to_string()` with no size limit on stdin
  - Fix: Use `stdin().take(MAX_CONTENT_SIZE as u64)`
  - Agent: security-reviewer

- [ ] **[RUST-002]** Status string matching with silent fallback
  - Location: `src/store/stats.rs:1017-1021`
  - Issue: Unknown status defaults to `Running` instead of error
  - Fix: Implement `TryFrom<&str> for ExperimentStatus` with proper error
  - Agent: rust-reviewer

- [ ] **[ARCH-001]** Domain logic in storage layer
  - Location: `src/store/stats.rs:893-1404`
  - Issue: Statistical calculations, variant routing, business logic in storage
  - Fix: Extract to `src/domain/experiments.rs`, storage should only handle persistence
  - Agent: architecture-reviewer

- [ ] **[ARCH-002]** Status parsing duplicated 3 times
  - Location: `src/store/stats.rs:1017-1022, 1066, 1378-1383`
  - Issue: Same string-to-enum conversion logic repeated
  - Fix: Single `ExperimentStatus::from_str()` method
  - Agent: architecture-reviewer

- [ ] **[DATA-002]** Experiments schema has no migration versioning
  - Location: `src/store/stats.rs:943-980`
  - Issue: No `schema_version` tracking for experiments tables
  - Fix: Add to main schema versioning system or create separate version table
  - Agent: data-safety-reviewer

- [ ] **[DATA-004]** end_experiment() doesn't validate current status
  - Location: `src/store/stats.rs:1342-1363`
  - Issue: Race condition - can end/cancel concurrently
  - Fix: Add `WHERE status = 'running'` and check rows_affected
  - Agent: data-safety-reviewer

- [ ] **[SIMP-001]** `_interactive` parameter unused
  - Location: `src/cli/handlers.rs:1222`
  - Issue: Parameter accepted but never used (YAGNI)
  - Fix: Remove parameter until feature is implemented
  - Agent: simplicity-reviewer

- [ ] **[SIMP-002]** `_df` computed but unused in significance calculation
  - Location: `src/store/stats.rs:1256-1258`
  - Issue: Degrees of freedom calculated then ignored
  - Fix: Remove dead code or implement t-distribution properly
  - Agent: simplicity-reviewer

- [ ] **[SIMP-003]** LLM prompt built but never used
  - Location: `src/cli/handlers.rs:1142-1166`
  - Issue: 25 lines building prompt for LLM that's never called
  - Fix: Delete prompt code or actually call LLM
  - Agent: simplicity-reviewer

---

## P3 - Nice-to-Have

- [ ] **[SEC-004]** Tag filter in condense has no length limit
  - Location: `src/cli/handlers.rs:1042-1065`
  - Note: Minor, tag comparison works but extremely long filters could slow down
  - Agent: security-reviewer

- [ ] **[SEC-007]** No validation on min_samples range
  - Location: `src/cli/handlers.rs:2322`
  - Note: 0 or negative values make no sense, very large causes issues
  - Agent: security-reviewer

- [ ] **[RUST-003]** Unnecessary cloning in find_related_entries
  - Location: `src/cli/handlers.rs:1101-1105, 1127-1132`
  - Note: Could use borrowed references instead of cloning strings
  - Agent: rust-reviewer

- [ ] **[DATA-003]** No unique constraint on experiment_results
  - Location: `src/store/stats.rs:962-972`
  - Note: Same query can be recorded multiple times - document if intentional
  - Agent: data-safety-reviewer

- [ ] **[DATA-006]** CASCADE DELETE on experiment_results undocumented
  - Location: `src/store/stats.rs:971`
  - Note: Intentional but should be documented
  - Agent: data-safety-reviewer

- [ ] **[SIMP-004]** CondenseGroup.proposed_title/content always Some
  - Location: `src/cli/handlers.rs:1035-1037`
  - Note: Fields are Option but always populated before use
  - Agent: simplicity-reviewer

- [ ] **[SIMP-005]** ExperimentResult.created_at passed but overwritten
  - Location: `src/store/stats.rs:939, 1102-1103`
  - Note: Field accepted then ignored
  - Agent: simplicity-reviewer

- [ ] **[ARCH-003]** Error type semantic mismatch
  - Location: `src/cli/handlers.rs:2386`
  - Note: Using `ErrorKind::Config` for "not found" - should be separate type
  - Agent: architecture-reviewer

---

## Cross-Cutting Analysis

### Patterns Across Agents

1. **Silent error handling** - Multiple agents flagged `.filter_map(|r| r.ok())` and `unwrap_or` patterns that hide errors
2. **Missing input validation** - Security and data-safety both noted unbounded inputs
3. **No transactions for multi-step operations** - Data-safety flagged, Rust agent noted potential panic paths
4. **Domain logic in wrong layer** - Architecture and simplicity both flagged stats.rs containing business logic

### Systemic Issues

1. **Schema versioning inconsistent** - Main schema has versioning, experiments don't
2. **Error propagation vs swallowing** - Inconsistent approach across codebase
3. **Feature-gated code mixed with core** - LLM prompt code exists but unused

### Combined Risk Assessment

The memory condense operation (P1-DATA-001) combined with silent error swallowing (P1-RUST-001) creates a scenario where data corruption could happen and not be properly logged. This is the highest-priority fix.

---

## Recommended Actions

1. **Immediate (P1):**
   - Fix transaction boundary in memory condense
   - Add error logging before `.filter_map(|r| r.ok())`
   - Add JSON config size limits
   - Add composite SQL query for experiment stats

2. **Before next release (P2):**
   - Remove unused `_interactive` parameter
   - Implement `TryFrom` for ExperimentStatus
   - Add input validation for experiment names, tags, content
   - Add schema versioning for experiments

3. **Follow-up tickets (P3):**
   - Refactor domain logic out of stats.rs
   - Clean up unused LLM prompt code
   - Document CASCADE DELETE behavior
   - Add experiment data export function

---

## Agent Highlights

- **Security:** Input validation gaps - JSON configs, experiment names, tags need size/format limits
- **Performance:** N+1 query in get_experiment_status, O(n²) in tag intersection
- **Architecture:** Domain logic in storage layer, status parsing duplicated 3x
- **Simplicity:** Unused parameters (_interactive, _df), dead prompt-building code
- **Rust Idioms:** Silent error swallowing in 5 locations, string-based enum matching
- **Data Safety:** Missing transaction in condense, no schema versioning for experiments

---

## Files Changed

| File | Lines Changed | Key Issues |
|------|---------------|------------|
| src/store/stats.rs | +706 | P1-RUST-001, P1-PERF-001, P2-DATA-002, P2-ARCH-001 |
| src/cli/handlers.rs | +395 | P1-DATA-001, P1-PERF-002, P2-SEC-005, P2-SIMP-001 |
| src/main.rs | +242 | P2-SEC-006 |
| src/cli/mod.rs | +83 | - |
| tests/e2e_mcp.rs | +367 | - |

---

Generated by Multi-Agent Review System
