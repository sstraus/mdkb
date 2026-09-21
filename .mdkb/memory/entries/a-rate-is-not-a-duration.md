---
id: a-rate-is-not-a-duration
title: A rate is not a duration; quote the stopwatch
entry_type: topic
source_type: user_statement
status: active
tags: [estimation, method, measured]
created_at: 1790001946
updated_at: 1790001946
---

Four estimates were wrong in one session on 2026-09-21 and every one was a measured rate turned into a projected duration. (1) itview embedding projected at ~100 minutes from a sampled 2.4 embeddings/s; actual 17m18s at 14.4/s, wrong by 6x. The 2.4/s was sampled during an mdkb update that was chunking and embedding concurrently on a copy, a different workload from mdkb embed running alone. (2) Two mid-run windows on the same drain gave 6.0/s and 3.8/s, so even re-sampling would have projected wrong - the rate is not steady. (3) '90 minutes of write-locked reindex' was a duration inferred from a long-running process and the noun was wrong too: WAL means no lock, and indexing alone was 9 seconds. (4) An earlier '4738 documents in 1m21s' came from an aborted run that had embedding switched on, so it measured the wrong thing. No counterexamples. THE RULE that survives: a rate is not a duration. Quote the stopwatch or quote nothing. If a number has to be given before the work finishes, say explicitly which part is measured and which is projected, and from what sample - the fleet-audit agent did that correctly once ('4,738 documents is measured and final, >90 minutes is a projection from a measured rate') and it was still off by 6x, which is the point.
