---
id: autoheal-memory-loss-tuicommander
title: Autoheal memory loss is resolved
entry_type: problem
source_type: user_statement
status: active
tags: [data-loss, mdkb, recovery, resolved, superseded]
created_at: 1783755295
updated_at: 1786282128
---

Historical 3.7.2 autoheal treated index.sqlite as derived data and could lose unique memory rows. This was fixed by quarantine salvage and durable memory projection. Superseded by mdkb-3-7-12-writer-recovery. The 2026-08-09 controlled TUICommander recovery preserved 893/893 memory IDs, left all forensic snapshots and quarantines intact, rebuilt documents and FTS, and passed full integrity checks after daemon activity.
