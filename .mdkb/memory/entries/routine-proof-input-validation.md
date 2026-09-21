---
id: routine-proof-input-validation
title: "Routine: input-validation proven approach"
entry_type: prior
source_type: auto_extracted
status: active
tags: [self-learning, success-routine]
created_at: 1783298426
updated_at: 1789989822
---

Proven approach for "input-validation" recurred across 83 stories on 12 distinct days — a reusable routine.

What worked:
- Byte ranges come from tree-sitter nodes and index the same source string the tree was parsed from, so slicing is always on a char boundary. Malformed C yields ERROR nodes, which hit the _ arm and are logged, not sliced.
- Every user-supplied path is still canonicalized and rejected if it does not start with the canonical root. <redacted> joins stored relative paths onto root, and a stored absolute path （used for files outside root） replaces the join, so no path is misresolved.
- Byte ranges come from tree-sitter nodes over the same source string, so slicing is always on a char boundary. Depth is bounded by <redacted> before any recursion.

Source stories: 017-96d8, 013-358d, 032-0960, 026-7b25, 025-3141, 008-12f2, 011-e97e, 018-b102, 019-eb8b, 024-b6df, 020-c1ce, 021-beab, 023-95e2, 022-85f5, 034-d0b5, 035-97e1, 036-75b1, 040-7354, 037-8060, 009-ffd5, 027-3f70, 028-8d30, 029-4608, 030-6c0d, 031-4947, 042-092f, 044-6d2f, 043-4559, 045-b76a, 046-bcde, 047-6b18, 048-fcec, 049-11af, 051-7102, 052-132f, 050-de09, 059-97d7, 057-3f0b, 056-da60, 053-28d1, 054-aa7b, 041-0014, 062-a1a2, 072-b5c3, 082-b609, 079-af23, 078-f373, 081-ea44, 080-62d2, 104-721f, 094-e662, 095-4f53, 100-fd56, 088-e01a, 083-7aa6, 084-1409, 087-9199, 092-777f, 105-9661, 089-7466, 090-45ae, 091-a2a9, 099-6061, 096-8794, 109-0fa8, 108-e302, 106-b12a, 107-3a1e, 117-a815, 118-20a3, 114-039c, 124-264e, 125-7419, 126-5dab, 128-870a, 129-dc4b, 130-9fc5, 131-7294, 132-757d, 133-6396, 134-7d3a, 135-a018, 136-2433
