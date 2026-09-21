---
id: grep-r-undercounts-under-gits
title: grep -r under ~/Gits returns a non-deterministic subset
entry_type: problem
source_type: user_statement
status: active
tags: [tooling, measurement, grep, fleet, measured]
created_at: 1789990892
updated_at: 1789990892
---

Measured 2026-09-21. Counting the same files three ways under ~/Gits: find -path '*/.mdkb/config.toml' returns 146; grep -rl '' --include=config.toml returns 45; grep -Rl (follow symlinks) returns 0 after a 60s timeout, because ~/Gits/target is a symlink and -R loops on it. An earlier grep -rl with a pattern returned 12, and after a path filter 5. Same tree, same instant, four different answers. Causes: ~/Gits/.tmp holds 35 GB and 205 leftover test stores, and there are symlinked directories at the top level. CONSEQUENCE, paid for once: I used grep -rl to check how many configs override hooks.warmup_limit, got 5, and told a teammate its count of 29 was wrong. A python os.walk that prunes node_modules/target/.git found 83 configs outside .tmp, of which 33 carry [hooks] warmup_limit = 50 - the teammate was right and I was not. RULE: for any fleet-wide count under ~/Gits use find or a pruning os.walk, never grep -r, and never trust a count you did not cross-check with a second instrument. This also invalidates the allowlist-audit recipe shipped in README.md on the same day (grep -rhoE '^[a-z_]+:' --include='*.md' .) when it is run above a project root.
