---
id: cross-repo-fanout-stays-serial
title: "The cross-repo fan-out is serial on purpose, and rayon is not an option"
entry_type: decision
source_type: user_statement
status: active
tags: [mcp, concurrency, rayon, onnx, fan-out]
created_at: 1790069411
updated_at: 1790069411
---

Decided 2026-09-22 (story 150-21b1), with a second opinion that changed the answer.

The cross-repo fan-out runs the whole per-repo loop inside ONE spawn_blocking and searches repos SERIALLY. That is the shipped design, not a stopgap.

Why not per-repo spawn_blocking with a bounded width: the immediate defect was that the loop ran ON a runtime worker that also serves the 200 ms hook socket, not that it lacked parallelism. Shipping a width picked from 'an SSD handles concurrency' is unmeasured. The archived story 039-a6b7 did exactly that in April: it introduced join_all over futures containing no .await, closed on 'cargo check clean' with BOTH acceptance criteria unchecked, and added a dependency plus a 'Fan out concurrently' comment while adding zero concurrency.

Why not rayon par_iter: src/llm/embeddings.rs cap_rayon_global_pool() fixes rayon's GLOBAL pool to ONE thread, deliberately. fastembed parallelises batches on that pool while every ONNX session opens its own N-thread pool; nesting them measured 1250% CPU with no forward progress for 20 minutes. A par_iter on the global pool would be serial, and reconfiguring it resurrects that failure. A dedicated pool would work but adds a second scheduler for an unmeasured gain.

Before adopting per-repo concurrency: benchmark widths 2/4/8 warm and cold, recording hook p99 and RSS, not just total latency. If adopted, attach each root's ordinal and restore order before merging — join_all preserved input order, which is the implicit tie-break before the score sort, and buffer_unordered would make equal-score results reorder between identical requests.

Also settled here: the eager RepoRegistry::open_read_only batch is DELETED, not bypassed. A store is opened immediately before its search and dropped after, so a workspace of 104 stores costs one live SQLite connection.
