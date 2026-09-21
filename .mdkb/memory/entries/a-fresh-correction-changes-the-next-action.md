---
id: a-fresh-correction-changes-the-next-action
title: A correction lands only if it arrives before the next action
entry_type: topic
source_type: user_statement
status: active
tags: [collaboration, method, measured]
created_at: 1790000347
updated_at: 1790000347
---

Observed 2026-09-21 across a long session with the mdkb-fleet-audit teammate. The teammate asserted that a long mdkb update was a 90-minute write-locked reindex. It never measured a lock - it inferred one from elapsed time and shipped it as a finding. I relayed it upward without checking, and Boss caught it with one question: 'write lock of what?'. Measured answer: the store is WAL so readers never block, and handle_embed runs after the commit behind a config gate, so the slow phase was a drainable background queue. Indexing alone was 15s on a copy and 9s on the live store. THE PART WORTH KEEPING is what the teammate said afterwards: it verified its next claim - that search stays usable at 97.7% unembedded, tested by actually running a search and getting three good hits - specifically BECAUSE it had just been caught on the previous one, in the same conversation. Not general diligence; a correction fresh enough to change the immediately following action. Across the day the same pattern held in both directions: I withdrew a warmup_limit count after a teammate pushed back, and found my grep -r instrument was returning 45 of 146 files; the teammate named its own sqlite3 -readonly CANTOPEN bug that miscounted 26 of 101 stores. Every one of these was an inference standing where a measurement belonged. The operational rule that follows: when you catch someone (or yourself) substituting an inference for a measurement, say so immediately rather than at the end, because the value decays with distance from the next decision.
