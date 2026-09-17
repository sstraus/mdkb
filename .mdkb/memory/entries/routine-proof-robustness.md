---
id: routine-proof-robustness
title: "Routine: robustness proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1789634152
---

Proven approach for "robustness" recurred across 88 stories on 10 distinct days — a reusable routine.

What worked:
- Walk still stops on any non-comment, non-attribute sibling; two negative tests pin that （quadruple_slash..., <redacted>）. Existing MAX_AST_DEPTH guard untouched.
- macro_name_node returns Option; a macro_invocation without a macro field is skipped, not unwrapped. Depth guard unchanged.
- current_fn shadowing already prefers the innermost function_item, so the seed cannot leak into function bodies; pinned by <redacted>.

Source stories: 005-8f23, 006-9d94, 007-cdb8, 014-f728, 015-088c, 016-de44, 017-96d8, 013-358d, 032-0960, 026-7b25, 025-3141, 008-12f2, 011-e97e, 018-b102, 019-eb8b, 024-b6df, 020-c1ce, 021-beab, 023-95e2, 022-85f5, 033-c5b0, 034-d0b5, 010-44ef, 035-97e1, 036-75b1, 038-9509, 040-7354, 039-f464, 037-8060, 009-ffd5, 027-3f70, 028-8d30, 029-4608, 030-6c0d, 031-4947, 012-a344, 042-3a71, 044-4663, 043-bf8d, 045-fa1d, 046-949f, 047-15e4, 048-926a, 049-645b, 042-092f, 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 051-7102, 052-132f, 050-de09, 058-1631, 059-97d7, 060-d363, 057-3f0b, 056-da60, 061-8821, 053-28d1, 054-aa7b, 055-631e, 041-0014, 062-a1a2, 063-3d17, 064-def1, 067-5ab6, 072-b5c3, 066-e3b0, 069-237a, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 086-679a, 104-721f, 094-e662, 095-4f53, 103-c0b8, 100-fd56, 088-e01a, 083-7aa6, 084-1409, 087-9199, 092-777f
