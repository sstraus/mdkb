---
id: routine-proof-thread-safety
title: "Routine: thread-safety proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1789973240
---

Proven approach for "thread-safety" recurred across 53 stories on 10 distinct days — a reusable routine.

What worked:
- The scope split only changes which PathBuf the DISCOVER thread walks; the channel topology, stage handles and join order are unchanged. <redacted> runs on the caller's thread before any stage is spawned.
- Extraction happens on the PARSE threads, which own their parser; the collected imports cross to COLLECT and INDEX through the existing bounded channel. Writes stay on the single INDEX thread inside its transaction — no new shared state.
- find_calls_in_node and the two new helpers are associated functions over borrowed nodes with no shared or static state; each PARSE thread owns its parser. Nothing added is Sync-relevant.

Source stories: 013-358d, 011-e97e, 018-b102, 019-eb8b, 024-b6df, 020-c1ce, 021-beab, 023-95e2, 022-85f5, 033-c5b0, 034-d0b5, 035-97e1, 036-75b1, 038-9509, 039-f464, 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 058-1631, 060-d363, 053-28d1, 054-aa7b, 055-631e, 041-0014, 063-3d17, 064-def1, 067-5ab6, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 086-679a, 100-fd56, 088-e01a, 084-1409, 087-9199, 092-777f, 093-bb13, 101-58b1, 102-6652, 096-8794, 109-0fa8, 108-e302, 106-b12a, 107-3a1e, 115-6c49, 124-264e, 125-7419
