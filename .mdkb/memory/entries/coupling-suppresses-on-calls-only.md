---
id: coupling-suppresses-on-calls-only
title: coupling suppresses on Calls edges only
entry_type: problem
source_type: auto_extracted
status: active
tags: [code-intel, coupling, docs, review-2026-09-16]
created_at: 1789565733
updated_at: 1789565733
---

src/core/coupling.rs:9 documents Calls/Uses/Expands/Implements as suppressing edges. connected_file_pairs (:209) builds from resolved_edges, which hardcodes WHERE r.kind = 'Calls' (src/code/storage/sqlite.rs:1195) at tiers 1-2 only. A pair connected only by Implements is reported as hidden coupling. Fix is the doc, not the query: no Uses-connected false positive measured yet. Story 098-8553, plan Step 16.
