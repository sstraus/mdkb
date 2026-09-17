---
id: routine-proof-defense-in-depth
title: "Routine: defense-in-depth proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1789653138
---

Proven approach for "defense-in-depth" recurred across 38 stories on 9 distinct days — a reusable routine.

What worked:
- Two independent layers still hold: the base directory is forced to 0700 before any socket work, and the socket itself is 0600. Neither was weakened; the change only removes the window between the second layer being created and being enforced.
- Reuse is keyed by the exact embedding text, not by name or path, so a stale vector cannot be served even if the id mapping were wrong: a changed doc comment produces a different key and falls through to the model.
- The sweep is the second line, not the first: delete_by_file still removes the vectors of the file it deletes, and the sweep catches whatever any path misses. Either alone would have fixed the measured case; both together mean a future code path that forgets to clean up costs one run of disk, not a permanent leak.

Source stories: 033-c5b0, 038-9509, 039-f464, 037-8060, 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 051-7102, 050-de09, 056-da60, 053-28d1, 054-aa7b, 055-631e, 041-0014, 062-a1a2, 063-3d17, 064-def1, 069-237a, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 104-721f, 094-e662, 088-e01a, 083-7aa6, 084-1409, 087-9199, 092-777f, 105-9661, 089-7466, 093-bb13, 097-3044
