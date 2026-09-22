---
id: falsify-before-shipping-a-concurrency-test
title: A concurrency test that passes under its own falsifier is not a test
entry_type: problem
source_type: auto_extracted
status: active
tags: [testing, falsifier, concurrency, tokio]
created_at: 1790069421
updated_at: 1790069421
---

Paid for on 2026-09-22, story 150-21b1.

To prove the cross-repo fan-out no longer occupies the tokio runtime, a test spawned a yield_now ticker on a current-thread runtime, ran a search over 12 real stores, and asserted the tick count exceeded a threshold. It passed. Then it passed again with the fix reverted.

Measured: 11787 ticks with the fix, 7448 without. The ticker banks most of its count during embed_query_off_lock, which already awaits BEFORE the fan-out, so the signal is 1.6x and any threshold between the two numbers is tuned to one machine's timing rather than to the behaviour.

The test was removed and the acceptance criterion REJECTED with the measurement attached, not silently skipped. The property is structural instead: search_roots_blocking has exactly one call site and it is inside spawn_blocking, and the eager batch open is deleted rather than bypassed, so neither path returns without reintroducing deleted code.

Rule: run the falsifier BEFORE writing the assertion threshold. A green test proves nothing until the reverted code turns it red. When no deterministic falsifier exists, say so with the measurement — a rejected criterion carrying evidence is worth more than a checked one carrying a test that cannot fail.
