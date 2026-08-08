---
id: routine-proof-completeness
title: "Routine: completeness proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1786201100
expires_at: 1788793100
---

Proven approach for "completeness" recurred across 29 stories on 4 distinct days — a reusable routine.

What worked:
- All output surfaces （CLI text<path> + MCP） render source path; graph::edge_views unit test + cli_smoke assert path present, numeric id absent
- via populated in neighbors（） from adjacent_pairs rows already fetched; unit test asserts via non-empty on every neighbor
- resolvable_forms（conn,ref） appends <redacted> ref_forms; used by resolve_entity_ref. Kept out of pure ref_forms to avoid a collections query on every BFS hop （perf）

Source stories: 075-4804, 076-da62, 077-5c76, 078-e4c7, 079-115a, 080-f2ac, 081-ae1c, 082-6508, 083-c95d, 006-07d0, 007-9099, 008-a52a, 005-c348, 010-b146, 022-d950, 013-4b7f, 014-fdf0, 023-fc7c, 015-2dc2, 012-19e7, 011-9a41, 016-a6dd, 020-9824, 021-0636, 017-a378, 009-686d, 019-3248, 024-0c7e, 018-56b2
