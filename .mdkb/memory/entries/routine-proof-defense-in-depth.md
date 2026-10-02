---
id: routine-proof-defense-in-depth
title: "Routine: defense-in-depth proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1790974125
---

Proven approach for "defense-in-depth" recurred across 100 stories on 16 distinct days — a reusable routine.

What worked:
- decode（） rejects a malformed blob rather than trusting it （store.rs:203）; set_embedding returns the rowcount so a missing body is a caller error, not a silent no-op
- body_text returns Option and both bounds must resolve （start?..end?） — a missing line cannot silently become offset 0. FIELD_SEP separates shingle terms so [ab,c] and [a,bc] cannot collide
- debug_assert!（max_tier < TIER_UNPLACED） in <redacted> makes the fail-open rule unbypassable by a future caller; from_id <> sym_id drops self-edges before union-find could union a node with itself; suppression_for treats （None, None） owners as NOT shared, since the opposite would suppress every free-function finding

Source stories: 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 051-7102, 050-de09, 056-da60, 053-28d1, 054-aa7b, 055-631e, 041-0014, 062-a1a2, 063-3d17, 064-def1, 069-237a, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 104-721f, 094-e662, 088-e01a, 083-7aa6, 084-1409, 087-9199, 092-777f, 105-9661, 089-7466, 093-bb13, 097-3044, 101-58b1, 090-45ae, 091-a2a9, 102-6652, 099-6061, 096-8794, 109-0fa8, 108-e302, 106-b12a, 107-3a1e, 117-a815, 118-20a3, 120-3dec, 112-fef6, 113-d6df, 114-039c, 115-6c49, 116-632f, 119-3f70, 121-c8fb, 122-a989, 124-264e, 125-7419, 126-5dab, 127-fcaf, 128-870a, 129-dc4b, 130-9fc5, 131-7294, 132-757d, 133-6396, 134-7d3a, 135-a018, 136-2433, 137-44b0, 139-cd2f, 142-e863, 143-1d54, 138-80db, 145-437a, 140-822c, 141-2032, 146-4862, 147-6c71, 148-7dcb, 149-b5f5, 150-21b1, 151-430e, 152-24a6, 153-7b40, 154-fa5a, 155-3ab4, 156-1c3e, 157-a554, 158-2a30, 166-f183, 167-c322, 164-5553, 165-7fa6, 168-a42f, 169-6225, 178-66b9, 177-ac4b, 170-1d2f, 184-0817, 174-630a
