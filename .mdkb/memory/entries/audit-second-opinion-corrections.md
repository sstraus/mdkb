---
id: audit-second-opinion-corrections
title: 2nd opinion corrections to synergy audit plans
entry_type: problem
source_type: user_statement
status: active
tags: [second-opinion]
created_at: 1783287737
updated_at: 1783287737
---

GPT-5.5 review (2026-07-05) of plans/mdkb-wiz-synergy-audit.md + wiz mdkb-first-integration.md, verified in code: (1) setup dedupe already exists (setup.rs:547-566) — live duplicate hooks persist only because setup was never re-run; (2) mdkb hook memory-confirm already exists (main.rs:1133, daemon-only); (3) CLI memory add hardcodes SourceType::UserStatement (handlers.rs:1354) — bridge writes over-trusted; (4) handle_embed iterates ALL collections incl claude_sessions while default search excludes them; (5) query_events stores raw query_text (privacy). Plans amended: verified fallback for handoffs, scoped auto-embed, opt-in prune with --older-than, opt-in hash-only telemetry, token-budget warmup.
