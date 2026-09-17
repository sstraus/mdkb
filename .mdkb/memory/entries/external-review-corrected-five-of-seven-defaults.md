---
id: external-review-corrected-five-of-seven-defaults
title: External review corrected 5 of 7 review defaults
entry_type: decision
source_type: auto_extracted
status: active
tags: [review-2026-09-16, second-opinion, retrieval, priors, code-intel]
created_at: 1789567168
updated_at: 1789567168
---

Second opinion (gpt-5.6-sol) on the seven open questions of plans/review-followups-2026-09-16.md. Verified corrections adopted: vec0 returns L2 distance not cosine so a cosine floor is d <= sqrt(2(1-tau)); confidence must leave the admission decision; 'any BM25 hit' is not an absolute test because recall OR-expands; MemoryEntry has no corrections field so Step 5 is mostly plumbing; symmetric belief is too weak at 20c/1r = 0.913 so weight refutations by 3 and add a disputed state; trigger selectors are conjunctive not alternative; settlement needs four outcomes because silence is not evidence even with a signature; callers_of_any already excludes TIER_EXTERNAL so the tier-3 question was a false premise; 'push only if tier < 7' under-traverses, the rule is enqueue once on first expandable arrival; dup ignore must be a subset test over a reviewed membership snapshot, not 'any ignored member'. Source multipliers are OfficialDocs 1.0 and UserStatement 0.85, the reverse of what the earlier review message stated.
