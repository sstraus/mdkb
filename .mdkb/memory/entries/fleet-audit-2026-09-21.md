---
id: fleet-audit-2026-09-21
title: "Fleet audit: 80 stores, v20-v30, 34 silent frontmatter losses"
entry_type: topic
source_type: user_statement
status: active
tags: [fleet, audit, mdkb, measured, schema]
created_at: 1789989802
updated_at: 1789989802
---

Measured 2026-09-21 across every .mdkb under ~/Gits excluding .tmp test dirs. 80 stores. PRAGMA integrity_check returns ok on every one - there is no corrupt database in the fleet. Schema versions span v20 to v30; the oldest stores are CC_Playground scratch projects on v20 and they migrate on first open by the current binary, no store refuses. The only quarantined copies are two in personal/mdkb itself (index.sqlite.corrupt-1789222525 from 2026-09-12 and -1789911983 from 2026-09-20, 89 MB together) and mdkb's own sweep deletes them 15 days after quarantine, so they need no hand. THE REAL FINDING: 34 documents across three stores have metadata='null', meaning their whole YAML frontmatter failed to parse and mdkb reported nothing - personal/mdkb 13, personal/tuicommander 19, personal/automa 2. Detection query: SELECT relative_path FROM documents WHERE metadata='null'. That is story 137 confirmed at fleet scale, not a one-repo accident. ~/Gits/LS/.mdkb holds a config and 31 memory entries with no index.sqlite - a memory-only ancestor store, probably intentional given the layered-ancestor-stores plan, but worth confirming. Several __wt worktree stores exist for agent2, gate-os and ego; each is its own store, which is expected.
