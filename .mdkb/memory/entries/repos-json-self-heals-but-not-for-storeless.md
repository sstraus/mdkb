---
id: repos-json-self-heals-but-not-for-storeless
title: "RepoMap self-heals a gone path, never a storeless dir"
entry_type: problem
source_type: user_statement
status: active
tags: [daemon, repo-map, isolation, measured]
created_at: 1789990128
updated_at: 1789990128
---

Corrects an earlier entry of mine that said the map only forgets a root whose path is gone and that a stale entry survives until removed by hand. Both halves were wrong. Measured 2026-09-21 by the fleet-audit agent and re-checked: the daemon repo map churns on its own because cargo test runs under the Gits temp dir (the Defender TMPDIR workaround) reach the real daemon and register themselves as roots. The daemon then drops them by itself - RootHealth::is_absence() removes both Gone and NoStore on every RepoMap::open, so a vanished tempdir disappears at the next daemon restart with no hand-editing. I observed a tmp root in the live file at 10:33 and gone after a restart, and wrongly credited the agent; the agent never touched the file and its 10:22 backup holds a different tempdir entirely. WHAT THE MAP NEVER FORGETS is a .mdkb directory that exists but holds no index.sqlite: classify() calls that Healthy, so it is neither Gone nor NoStore and stays forever. That is the durable half of the trap. Also measured: daemon stop is not durable while MCP sessions are open - 11 live mdkb mcp clients respawn it, and stop returned did-not-exit-within-10s with a replacement already up. The real defect underneath is still that a test reaches the production daemon at all; the isolated_home helper is not covering some path.
