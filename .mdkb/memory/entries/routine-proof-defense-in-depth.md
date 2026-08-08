---
id: routine-proof-defense-in-depth
title: "Routine: defense-in-depth proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1786201101
expires_at: 1788793101
---

Proven approach for "defense-in-depth" recurred across 20 stories on 3 distinct days — a reusable routine.

What worked:
- Two independent checks must both pass: canonicalized containment under root （hook_session_cwd） AND membership in the registered collection list （project_scope_token）. project_scope_token re-checks containment via strip_prefix even though the caller already validated it.
- Two independent barriers keep a foreign handoff out: the scope filter on candidate selection, and the fail-closed rule that an empty candidate set injects nothing rather than falling back to the newest overall.
- Deliberately a bias and not a filter, so a wrong scope token can only reorder - it can never suppress an entry the session would have received before. The 073 handoff-stripping invariant is enforced upstream in <redacted> and is independent of this comparator.

Source stories: 006-07d0, 007-9099, 008-a52a, 005-c348, 010-b146, 022-d950, 013-4b7f, 014-fdf0, 023-fc7c, 015-2dc2, 012-19e7, 011-9a41, 016-a6dd, 020-9824, 021-0636, 017-a378, 009-686d, 019-3248, 024-0c7e, 018-56b2
