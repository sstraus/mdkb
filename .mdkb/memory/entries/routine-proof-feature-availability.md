---
id: routine-proof-feature-availability
title: "Routine: feature-availability proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1790006710
---

Proven approach for "feature-availability" recurred across 134 stories on 13 distinct days — a reusable routine.

What worked:
- Doc comments now reach the index for attributed items; before the fix they were dropped. Verified by RED-then-GREEN on <redacted>.
- Macro call edges now reach the graph; before the change a function whose body was only macro calls produced zero edges.
- Calls in static/const initializers now reach the call graph; before they were silently dropped for lack of a caller.

Source stories: 005-8f23, 006-9d94, 007-cdb8, 014-f728, 015-088c, 016-de44, 017-96d8, 013-358d, 032-0960, 026-7b25, 025-3141, 008-12f2, 011-e97e, 018-b102, 019-eb8b, 024-b6df, 020-c1ce, 021-beab, 023-95e2, 022-85f5, 033-c5b0, 034-d0b5, 035-97e1, 036-75b1, 038-9509, 040-7354, 039-f464, 037-8060, 009-ffd5, 027-3f70, 028-8d30, 029-4608, 030-6c0d, 031-4947, 012-a344, 042-3a71, 044-4663, 043-bf8d, 045-fa1d, 046-949f, 047-15e4, 048-926a, 049-645b, 042-092f, 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 051-7102, 052-132f, 050-de09, 058-1631, 059-97d7, 060-d363, 057-3f0b, 056-da60, 061-8821, 053-28d1, 054-aa7b, 055-631e, 041-0014, 062-a1a2, 063-3d17, 064-def1, 067-5ab6, 072-b5c3, 066-e3b0, 069-237a, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 086-679a, 104-721f, 094-e662, 095-4f53, 103-c0b8, 098-8553, 100-fd56, 088-e01a, 083-7aa6, 084-1409, 087-9199, 092-777f, 105-9661, 089-7466, 093-bb13, 097-3044, 101-58b1, 090-45ae, 091-a2a9, 102-6652, 099-6061, 096-8794, 110-44dd, 109-0fa8, 108-e302, 106-b12a, 107-3a1e, 111-bca6, 123-3966, 117-a815, 118-20a3, 120-3dec, 112-fef6, 113-d6df, 114-039c, 115-6c49, 116-632f, 119-3f70, 121-c8fb, 122-a989, 124-264e, 125-7419, 126-5dab, 127-fcaf, 128-870a, 129-dc4b, 130-9fc5, 131-7294, 132-757d, 133-6396, 134-7d3a, 135-a018, 136-2433, 137-44b0, 139-cd2f, 142-e863, 143-1d54, 138-80db
