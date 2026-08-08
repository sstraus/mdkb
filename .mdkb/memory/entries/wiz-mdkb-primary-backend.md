---
id: wiz-mdkb-primary-backend
title: mdkb is primary memory backend; journal is fallback
entry_type: decision
source_type: user_statement
status: active
tags: [architecture]
created_at: 1783287401
updated_at: 1783287401
---

Boss directive 2026-07-05: when mdkb is loaded/configured, ALL memory writes (handoff, precompact, learnings) go to mdkb; the FS journal is used ONLY when mdkb is absent or MDKB_DISABLE=1. Dual-write rejected (two-path race, stale latest.json). Drives plans/mdkb-wiz-synergy-audit.md (mdkb) and wiz-agents/plans/mdkb-first-integration.md (wiz).
