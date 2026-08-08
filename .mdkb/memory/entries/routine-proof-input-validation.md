---
id: routine-proof-input-validation
title: "Routine: input-validation proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1786201101
expires_at: 1788793101
---

Proven approach for "input-validation" recurred across 15 stories on 4 distinct days — a reusable routine.

What worked:
- strip_prefix guards non-matching refs; empty basename skipped in collection_prefixes
- relation is an optional filter compared against edge.relation; limit is usize; no injection （columns hardcoded）
- <redacted>: an older quarantine with no sidecar is still surfaced （never hidden）

Source stories: 077-5c76, 081-ae1c, 083-c95d, 006-07d0, 007-9099, 005-c348, 010-b146, 022-d950, 013-4b7f, 014-fdf0, 023-fc7c, 015-2dc2, 016-a6dd, 017-a378, 024-0c7e
