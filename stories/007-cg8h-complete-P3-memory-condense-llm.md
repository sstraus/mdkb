---
id: 007-cg8h
title: Implement mdkb memory condense command with LLM merging
status: complete
priority: P3
created: 2026-01-31
updated: 2026-01-31
dependencies: []
acceptance_criteria:
  - mdkb memory condense finds related entries (overlapping tags)
  - Uses local LLM to suggest merges (feature-gated behind --features llm)
  - Creates consolidated entry with supersedes relationships
  - Marks original entries as superseded_by=merged-id
  - Interactive mode asks user to confirm each merge
  - Reduces memory index size while preserving knowledge
test_coverage: LLM integration tests (behind feature flag)
---

## Problem

Over time, related memory entries accumulate (e.g., auth-jwt-basics, auth-jwt-refresh, auth-jwt-fix). No way to consolidate without manual work.

## Solution

Implement AI-assisted consolidation:

1. Find entries with overlapping tags
2. Group by tag/domain
3. Use local LLM to:
   - Read group of related entries
   - Propose merged title and consolidated content
   - Preserve all information from originals
4. Create merged entry
5. Mark originals as superseded_by=merged_id
6. Remove from warmup (replaced by consolidated version)

## Implementation Tasks

- [ ] Implement find_related_entries() function
- [ ] Add LLM prompt for consolidation (behind feature flag)
- [ ] Implement create_consolidated_entry()
- [ ] Update original entries: status->superseded, superseded_by field
- [ ] Add CLI: `mdkb memory condense [--interactive] [--tag TAG]`
- [ ] Add dry-run mode to show proposed merges
- [ ] Test: Related entries found correctly (feature-gated)
- [ ] Test: Consolidation preserves all original content
- [ ] Test: Supersedes relationships created correctly

## Notes

From plan Phase 6 section "Condensation Strategy" (lines 1119-1138).

This is a nice-to-have (P3) because:
- Requires LLM feature (not everyone has it)
- Manual management works, but consolidation helps at scale
- Should be tried at 10+ related entries (config: condense_threshold)

Example from plan:
```
Before:
- auth-jwt-basics (2025-06)
- auth-jwt-refresh (2025-08)
- auth-jwt-expiry-fix (2025-11)

After:
- auth-jwt-complete (2026-01)
  Supersedes: auth-jwt-basics, auth-jwt-refresh, auth-jwt-expiry-fix
```
