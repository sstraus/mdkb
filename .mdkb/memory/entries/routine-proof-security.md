---
id: routine-proof-security
title: "Routine: security proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1786201101
expires_at: 1788793101
---

Proven approach for "security" recurred across 9 stories on 3 distinct days — a reusable routine.

What worked:
- cargo test expand_recall green
- heal.rs module doc rewritten: memory_entries/memory_edges live ONLY in the index; projection is best-effort
- The token is derived only from a cwd already validated as <redacted> （hook_session_cwd, story 005） and is only accepted when it matches a registered collection name, so a client-supplied cwd cannot name an arbitrary scope. The token is never interpolated into SQL - matching happens in Rust over already-loaded tags.

Source stories: 082-6508, 083-c95d, 006-07d0, 007-9099, 008-a52a, 005-c348, 013-4b7f, 017-a378, 018-56b2
