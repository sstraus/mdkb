---
id: prior-belief-loop-settles-at-stop
title: Prior belief loop settles at the Stop hook
entry_type: decision
source_type: user_statement
status: active
tags: [priors, belief, stop-hook, schema-v24]
created_at: 1789562229
updated_at: 1789562229
---

cluster_injection_score divides by a Beta belief over confirmed_count/refuted_count and nothing moved either, so a promoted prior started at 0.33 against a 0.3 threshold and decayed under it in ~20 days. Fix (story 080-62d2): prior_injections table keyed (cluster_id, session) records each injection; prior_clusters.error_signature holds the failure the lesson prevents, taken from the detector's CandidateSignal. settle_session runs at Stop BEFORE the mining gate — injection and mining are separate switches — and refutes when that signature recurs at or after injected_at, confirms otherwise. ToolError now carries the record timestamp so an error predating the injection cannot refute. mdkb memory confirm/refute on the projection routes to the cluster via apply_belief_from_memory. Schema v24.
