---
id: impact-reported-and-expandable-are-separate-d6
title: "D6: impact separates reported from eligible for expansion"
entry_type: decision
source_type: user_statement
status: active
tags: [code-intel, impact, resolution-tier, graph-traversal, review-2026-09-16]
created_at: 1789661162
updated_at: 1789661162
---

Plan Step 15 / story 097-3044, landed 8d75b8f. Decision: an arrival at TIER_UNPLACED (7) is a bare name match — the caller wrote the name and no rule could say it meant this symbol — so it is REPORTED but never EXPANDED. Two states, not one.

Rejected: 'push only when tier < 7'. It under-traverses, because a symbol first seen at tier 7 gets recorded and is then never re-queued when a later round reaches it at tier 1. The rule adopted instead: record every arrival at the nearest tier that reached it; enqueue only expandable arrivals; enqueue each symbol at most once, on its first expandable arrival, with the depth of the path that qualified it rather than of the one that merely named it. Depth belongs to the path, not to queue activity.

Also rejected: per-hop confidence decay. Tiers are ordinal categories, not probabilities. The tier reported is the last edge, so a 1 -> 7 -> 1 path is never presented as tier 1.

The false premise in the original question: callers_of_any already excludes TIER_EXTERNAL, so that was never the leak.

Output splits into the actionable radius, an ambiguous frontier, and a coverage note saying how many arrivals the walk stopped at — a radius short by those subtrees used to read as exhaustive. code impact --format json is one document with targets, ambiguous and stopped_arrivals. Tests: an_unplaced_arrival_is_reported_but_never_walked_through, a_caller_reached_twice_in_one_hop_keeps_its_nearest_tier, smoke_code_impact_separates_the_ambiguous_frontier_and_says_where_it_stopped.
