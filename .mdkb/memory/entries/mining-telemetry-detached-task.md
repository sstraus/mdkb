---
id: mining-telemetry-detached-task
title: Detached work needs its own telemetry and an awaited route
entry_type: topic
source_type: auto_extracted
status: active
tags: [priors, telemetry, hooks, daemon, tokio]
created_at: 1789556398
updated_at: 1789556398
---

Two failures with one shape, fixed in story 078-f373. (1) hook_stop_impl returns {} before the distiller starts, so the 'stop' hook event logged outcome=skipped on 105 of 105 Stop events - it was measuring the dispatch, not the work. Fix: the detached task records its own prior_mining event with outcome in {gated,distilled,rejected,failed} plus the reason. The outcome is produced by an inner fn returning MiningOutcome and written once by the wrapper, so 'one event per run' survives the seven early returns. (2) tokio::spawn inside a hook is correct in the daemon (it outlives the hook by hours) and silently wrong on the MDKB_NO_DAEMON route, where the process exits the instant the hook returns and the task is dropped before it runs - mining produced nothing on that route, ever. Fix: DispatchContext::background, None for the daemon and Some for the in-process route, awaited after emit so the host still gets the answer at the same moment. Rule: when the same code runs both in a long-lived daemon and in a process that exits at the end of the call, 'spawn and forget' is a per-route decision, not a property of the function.
