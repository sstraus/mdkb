---
id: refutation-writes-corrections-not-a-decrement
title: "A refutation records corrections, it does not cancel a confirmation"
entry_type: decision
source_type: user_statement
status: active
tags: [memory, confidence, staleness, story-087]
created_at: 1789591883
updated_at: 1789591883
---

Story 087. Before: outcome_to_delta mapped refuted to -1 and confirm_entry decremented confirmations (floor 0), never writing the corrections column. Every consumer of corrections was therefore dead code: memory_graph::stale_dependency_ids never reported a STALE-DEP and the warmup filter 'AND corrections <= confirmations' excluded nothing. Now: a negative delta raises corrections and stamps a new last_refuted_at column (schema v25), and leaves confirmations and last_confirmed_at alone - last_confirmed_at is the decay reference, so moving it would let a refutation refresh the entry's decay clock. Belief became (1+confirmations)/(2+confirmations+3*corrections): one refutation outweighs three confirmations. 'Disputed' is a DERIVED predicate MemoryEntry::is_disputed() = corrections > 0 && last_refuted_at > last_confirmed_at, deliberately NOT an EntryStatus variant, because status IS projected to the git-tracked markdown while the counters are machine-local and never projected - a status flag would rewrite a tracked file to record a local refutation. A disputed entry is dropped in dispatch::injectable and in rank_warmup_entries, so it is never injected unasked however high it scores (20c/1r still scores 0.84, above every threshold in the system), but an explicit memory search still returns it. A same-second confirm/refute tie goes to the confirmation, so reconfirming always clears the dispute.
