---
id: routine-proof-security
title: "Routine: security proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1790974125
---

Proven approach for "security" recurred across 72 stories on 15 distinct days — a reusable routine.

What worked:
- Every statement binds via params!/[]; no format!-built SQL in store.rs. Data is local repo source hashes, no trust boundary crossed
- Read-only over repo source already indexed; no SQL, no process, no network. body_text slices by byte offsets computed from split_inclusive, so it cannot index outside the source
- Every statement binds via params!; the only interpolation is resolved_edges, a crate-internal const SQL fragment with no user input. Read-only over the index and repo source

Source stories: 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 050-de09, 053-28d1, 054-aa7b, 055-631e, 064-def1, 067-5ab6, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 104-721f, 094-e662, 087-9199, 092-777f, 105-9661, 090-45ae, 102-6652, 099-6061, 096-8794, 110-44dd, 109-0fa8, 108-e302, 106-b12a, 107-3a1e, 111-bca6, 117-a815, 114-039c, 119-3f70, 124-264e, 125-7419, 126-5dab, 128-870a, 129-dc4b, 130-9fc5, 131-7294, 132-757d, 133-6396, 134-7d3a, 135-a018, 136-2433, 137-44b0, 139-cd2f, 142-e863, 143-1d54, 138-80db, 145-437a, 140-822c, 141-2032, 146-4862, 147-6c71, 148-7dcb, 150-21b1, 166-f183, 167-c322, 168-a42f, 169-6225, 178-66b9, 160-a148, 170-1d2f, 181-63ba, 182-6329, 184-0817, 188-a540, 174-630a
