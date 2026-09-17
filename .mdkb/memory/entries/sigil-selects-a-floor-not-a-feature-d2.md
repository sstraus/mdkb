---
id: sigil-selects-a-floor-not-a-feature-d2
title: "D2: the sigil selects a recall floor, not the feature"
entry_type: decision
source_type: user_statement
status: active
tags: [recall, hooks, sigil, threshold, shadow-mode, review-2026-09-16]
created_at: 1789661151
updated_at: 1789661151
---

Plan Step 14 / story 096-8794. Decision: build the third option first — recall runs on every prompt, the sigil lowers the admission floor instead of switching retrieval on, and nothing is injected when no candidate clears the floor. Two floors: search.memory.min_recall_cosine 0.40 for a sigil prompt, hooks.recall_auto_min_cosine 0.50 for one without. Rationale: an injection nobody asked for is charged on every turn after it, a miss on a sigil prompt costs one search.

0.50 is derived, not picked. The precision rule that fixes 0.40 (lowest floor admitting no labelled negative) stops discriminating above it — every floor from 0.40 up scores precision 1.000 on the 36-query/40-negative fixture. The second rule is the recall plateau: 0.40->0.45 costs 0.139 recall@5, 0.45->0.50 costs 0.027, 0.50->0.55 costs 0.111. eval::fixture::tests::print_the_precision_recall_curve_over_tau asserts the property for both constants.

NOT flipped: user_prompt_submit_require_sigil stays true. Measured on this repo 2026-09-17: 1716 UserPromptSubmit calls over 72 days, 8 injections, 0.47%. Flipping turns the other 1708 into retrieval attempts and the fixture cannot rank floors above 0.40, so the release gate is a week of hooks.user_prompt_submit_shadow data judged on four counters together — injection rate, precision (read off the entry ids the row names), repetition rate, P95 latency — not fixture precision alone. Shadow mode detaches the session dedup map and skips the prior leg: both are writes that would change what a later real injection does and corrupt the counters it exists to produce.
