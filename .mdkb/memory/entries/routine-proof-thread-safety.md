---
id: routine-proof-thread-safety
title: "Routine: thread-safety proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1790974125
---

Proven approach for "thread-safety" recurred across 58 stories on 13 distinct days — a reusable routine.

What worked:
- REVIEW FINDING FIXED: open（） lacked busy_timeout. Now matches Context::<redacted> （src<path>:169） — busy_timeout=5000, WAL, synchronous=NORMAL, temp_store=memory. Asserted by <redacted>
- body.rs is free functions over borrowed data, no shared state. create_parser is unchanged in behaviour — still one parser per language per parse thread, as stage_parse already required （LanguageParser: Send）
- Free functions over borrowed data plus a single-threaded parser map, mirroring stage_parse. No shared mutable state; the DupDb connection is used from the calling thread only

Source stories: 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 058-1631, 060-d363, 053-28d1, 054-aa7b, 055-631e, 041-0014, 063-3d17, 064-def1, 067-5ab6, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 086-679a, 100-fd56, 088-e01a, 084-1409, 087-9199, 092-777f, 093-bb13, 101-58b1, 102-6652, 096-8794, 109-0fa8, 108-e302, 106-b12a, 107-3a1e, 115-6c49, 124-264e, 125-7419, 136-2433, 139-cd2f, 143-1d54, 140-822c, 141-2032, 147-6c71, 149-b5f5, 150-21b1, 154-fa5a, 156-1c3e, 167-c322, 164-5553, 165-7fa6, 168-a42f, 169-6225, 177-ac4b, 180-b051, 182-6329, 183-1f3a, 187-d7c8
