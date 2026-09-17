---
id: impact-traverses-unplaced-tier-7-edges
title: impact traverses tier-7 unplaced edges
entry_type: problem
source_type: auto_extracted
status: active
tags: [code-intel, impact, resolution-tier, review-2026-09-16]
created_at: 1789565732
updated_at: 1789565732
---

get_impact_by_tier (src/code/storage/sqlite.rs:896) pushes every newly seen caller onto the next BFS frontier regardless of tier. A caller placed at TIER_UNPLACED (7) is a bare name match, yet the walk continues through it and its whole subtree is reported as impacted. unplaced_arrivals (dispatch.rs:2782) counts only direct arrivals, not the subtree. Story 097-3044, plan Step 15.
