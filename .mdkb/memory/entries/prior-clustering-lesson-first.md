---
id: prior-clustering-lesson-first
title: "Prior clustering keys on the lesson, not the trigger"
entry_type: decision
source_type: auto_extracted
status: active
tags: [priors, clustering, embedding, promotion]
created_at: 1789655378
updated_at: 1789655378
---

Story 091-a2a9 / plan Step 9, 2026-09-17. DESIGN DECISION: lesson similarity is the PRIMARY clustering key; the canonical trigger key is the fallback. The order was the other way round.

Why: a prior IS its lesson. The trigger is only how the lesson gets surfaced. Two sessions that learned the same rule under different triggers are the same recurrence, and recurrence is what PROMOTION_MIN_SESSIONS=2 counts. Measured on the live store 2026-09-17: 11 candidates whose lesson is the same budget-limit rule sit in 9 separate clusters, so not one of them reaches 2 distinct sessions and the rule is never promoted. Meanwhile a weak '*| grep*' pattern promoted on two sessions because its trigger key happened to repeat. Trigger-first counts spellings; lesson-first counts lessons.

Accepted cost: when two genuinely different triggers carry one lesson, the merged cluster keeps the FIRST cluster's trigger_matcher, so the second trigger loses its own injection point. The candidates keep their own matchers on their rows, so the evidence is not lost and the lesson can be re-expressed. This is the right trade because an unpromoted lesson injects nowhere at all, while a merged one injects on at least one real trigger.

Threshold unchanged at PRIOR_MERGE_SIMILARITY=0.85 cosine over the lesson embedding. A candidate with no embedding still falls back to the trigger key, so mining without the ONNX model degrades to the old behaviour rather than failing.

Also fixed while here: find_cluster_by_embedding excluded 'refuted' and 'expired' but not 'archived', so an archived cluster could still absorb new evidence — newly reachable because migration v26 archives untyped-matcher clusters.
