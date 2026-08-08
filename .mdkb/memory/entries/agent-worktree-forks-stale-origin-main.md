---
id: agent-worktree-forks-stale-origin-main
title: Agent worktrees fork from stale origin/main in mdkb
entry_type: problem
source_type: user_statement
status: active
tags: [subagent, parallel, git, tooling]
created_at: 1783422632
updated_at: 1783422632
---

In this repo, Agent tool worktree isolation forks from origin/main, not the current branch tip. origin/main (f291430) is 6+ commits behind local main and does NOT compile (missing DispatchContext.hook_dedup field and schema::last_index_scan_at/LAST_INDEX_SCAN_KEY). Subagents also cannot self-heal because a hard branch-reset is blocked by the destructive-command guard. Prevention: do NOT use worktree subagents here until origin/main is pushed current; execute inline or via non-worktree agents scoped to disjoint files.
