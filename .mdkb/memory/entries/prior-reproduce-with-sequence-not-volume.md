---
id: prior-reproduce-with-sequence-not-volume
title: "Reproduce a concurrency bug by finding the sequence, not by increasing the volume"
entry_type: prior
source_type: user_statement
status: active
tags: [reproduction, testing, correction]
created_at: 1786118175
updated_at: 1786118175
---

Boss stopped a 10x2000-process soak that was pinning his Mac at 100% CPU to hunt SQLite corruption. His objection was correct and structural: in production nobody bombards mdkb — it is a few operations in parallel. If low concurrency corrupts, the variable is a specific SEQUENCE (version skew between long-lived and one-shot writers, a write inside the quarantine/rename window, a migration racing another writer), and brute force buries that sequence in noise instead of surfacing it. Reach for two processes and three operations in a deliberate order before reaching for scale. Also: a soak that only checks integrity at the end yields no verdict when interrupted — check every round.
