---
id: routine-proof-thread-safety
title: "Routine: thread-safety proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1786201101
expires_at: 1788793101
---

Proven approach for "thread-safety" recurred across 18 stories on 3 distinct days — a reusable routine.

What worked:
- salvage reads corrupt via immutable=1 （no lock/hot-journal）; detach best-effort; single fresh connection
- project_scope_token and entry_in_scope are pure functions over borrowed data, no shared state. The one DB read （list_collections） happens under the existing ctx mutex guard, which is dropped before any await.
- <redacted> is a pure function that consumes its Vec by value; no shared state, no interior mutability, nothing held across an await.

Source stories: 083-c95d, 006-07d0, 007-9099, 008-a52a, 005-c348, 022-d950, 014-fdf0, 023-fc7c, 015-2dc2, 012-19e7, 011-9a41, 016-a6dd, 020-9824, 021-0636, 017-a378, 009-686d, 019-3248, 018-56b2
