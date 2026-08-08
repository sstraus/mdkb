---
id: ancestor-layering-removed-on-purpose
title: Ancestor layering was removed on purpose (CPU + umbrella stores)
entry_type: decision
source_type: user_statement
status: active
tags: [mdkb, layering, warmup, ancestor-stores, umbrella, decision]
created_at: 1785593827
updated_at: 1785593827
---

Read-only ancestor layering shipped 2026-06-21 (7f6ba70 anchoring + fe677a0 layered reads), refined 2026-06-26 (62f7292, story 020-5738 global cross-store rank). It was REMOVED on 2026-07-05 in f291430, whose commit message covers only priors cluster-merge and never mentions layering - hence the removal looks silent in git log alone. The rationale is in the stories, not the commit: story 037-a6c0 (P1, same day) 'Project root resolution hijacks parent-directory .mdkb stores (100% CPU spikes)' - resolve_project_root preferred the nearest ANCESTOR store with no git boundary, so a repo without its own .mdkb anchored to a stray parent (~/Gits/.mdkb = 16066 cross-repo files, ~/.mdkb = 24099 incl ~/go/pkg/mod) and the daemon reindexed the whole parent tree, minutes of 100% CPU, live-confirmed from ~/Gits/btcount. Fixed by bounding the walk to the git root (find_store_within, git.rs:155). Story 054-0189 then set out to retire the umbrella stores ~/Gits/.mdkb, ~/Gits/LS/.mdkb and ~/Gits/CC_Playground/.mdkb; its two [MANUAL] criteria were never executed, which is why CC_Playground/.mdkb still exists with a 118MB code.sqlite. CONSEQUENCE: do NOT restore generic ancestor-walk layering - it re-imports a paid-for bug. A cross-cutting 'global' warmup layer must instead be an EXPLICIT opt-in single store (configured path, never discovered by walking up), memory-only, no doc/code indexing (the 037 damage was the indexer, not the reads), warmup-only with its own small cap and token budget, and must not touch resolve_project_root's git boundary. Also do not designate CC_Playground/.mdkb as that global store: 054 marks it for retirement.
