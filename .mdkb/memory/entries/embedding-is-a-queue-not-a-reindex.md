---
id: embedding-is-a-queue-not-a-reindex
title: "A slow update is the embedding queue, not a locked reindex"
entry_type: problem
source_type: user_statement
status: active
tags: [indexing, embedding, performance, measured, correction]
created_at: 1789997981
updated_at: 1789997981
---

Corrects a framing I relayed without checking on 2026-09-21. A fleet-audit teammate measured mdkb update on a copy of CC_Playground/itview at 18m21s and still running, and we both described it as a 90-minute write-locked reindex that would degrade searches on a live store. Both halves are false, verified at source. (1) The store is WAL - PRAGMA journal_mode returns wal - so readers never block on a writer and search keeps working for the whole run. (2) Embedding is not part of indexing: src/core/indexing.rs:318 calls handle_embed AFTER the commit, gated on config.search.auto_embed_docs, and its failure is logged rather than fatal. There is a standalone mdkb embed subcommand and spawn_embedding_backfill drains pending work in the background under a single-flight guard. The teammate's own observation proves it - the document count 4738 was stable for over 20 minutes while chunk count kept climbing, meaning documents were committed early and the remaining time was the embedding queue draining at a measured 2.4/s. CORRECT HANDLING of a large store: set search.auto_embed_docs = false for the run, mdkb update to index, then mdkb embed whenever, interruptible. Search stays usable meanwhile because the BM25/FTS leg does not need embeddings; only semantic hits are missing and only for undrained documents. LESSON: elapsed wall time on an update is not evidence of a lock. Check journal_mode and whether the slow phase is after the commit before calling anything an operational event.
