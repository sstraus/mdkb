---
id: routine-proof-input-validation
title: "Routine: input-validation proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1790974125
---

Proven approach for "input-validation" recurred across 89 stories on 16 distinct days — a reusable routine.

What worked:
- cosine（） rejects ragged input and zero-magnitude vectors; mean（） rejects an empty population
- decode（） length check （empty or not a multiple of 4 => None）, covered by <redacted>; gc（） empty-live-set branch
- end_line < start_line rejected; out-of-range rows rejected; kinds.len（） < SHINGLE hashed whole rather than skipped, so a tiny body cannot come back as fingerprint 0

Source stories: 042-092f, 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 051-7102, 052-132f, 050-de09, 059-97d7, 057-3f0b, 056-da60, 053-28d1, 054-aa7b, 041-0014, 062-a1a2, 072-b5c3, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 104-721f, 094-e662, 095-4f53, 100-fd56, 088-e01a, 083-7aa6, 084-1409, 087-9199, 092-777f, 105-9661, 089-7466, 090-45ae, 091-a2a9, 099-6061, 096-8794, 109-0fa8, 108-e302, 106-b12a, 107-3a1e, 117-a815, 118-20a3, 114-039c, 124-264e, 125-7419, 126-5dab, 128-870a, 129-dc4b, 130-9fc5, 131-7294, 132-757d, 133-6396, 134-7d3a, 135-a018, 136-2433, 137-44b0, 142-e863, 143-1d54, 138-80db, 145-437a, 140-822c, 141-2032, 153-7b40, 175-8ca9, 161-1495, 163-aeae, 171-8ee1, 172-e3c6, 176-bc15, 159-31ea, 166-f183, 167-c322, 164-5553, 168-a42f, 169-6225, 178-66b9, 177-ac4b, 179-e4ac, 180-b051, 183-1f3a, 184-0817, 189-2f66, 188-a540, 174-630a, 193-33f0, 194-f82f
