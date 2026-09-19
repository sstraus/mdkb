---
id: memory-audit-selects-never-decides
title: "memory audit selects, it never decides"
entry_type: decision
source_type: user_statement
status: active
tags: [mdkb, memory, audit, schema-v28]
created_at: 1789829383
updated_at: 1789829383
---

mdkb memory audit (story 108-e302) lists entries worth re-reading from four mechanical signals and writes no verdict. An AI sweep was rejected up front: a model has no ground truth to check a stored entry against and would stamp confident 'still valid' on entries nobody verified — story 092 under another name. Signals: a cited path absent from the tree BUT known to git (a path git never saw is prose shaped like a path and is not reported); a commit under a cited path newer than updated_at, only past stale_after_days; near-duplicate pairs by the write path's own rule plus unresolved contradicts edges; expired and aged lifecycle records. The one write is last_audited_at (schema v28) — NOT last_confirmed_at, which is the decay reference in confidence_at, so writing it would refresh confidence on entries a sweep merely looked at. No revision, no updated_at change, so no markdown projection rewrite.
