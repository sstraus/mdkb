---
id: fleet-hooks-warmup-limit-frozen-at-50
title: hooks.warmup_limit frozen at 50 in 32 live configs
entry_type: problem
source_type: user_statement
status: active
tags: [config, fleet, measured, hooks]
created_at: 1789990892
updated_at: 1789990892
---

Reported by the mdkb-fleet-audit agent and confirmed by my own python walk on 2026-09-21, after my first attempt with grep -r undercounted it to 1. Of 83 .mdkb/config.toml files under ~/Gits outside the temp tree: 76 carry [memory] warmup_limit = 50, which IS the shipped default and no override; 38 carry [hooks] warmup_limit = 10, also the default; and 33 carry [hooks] warmup_limit = 50, a genuine 5x override, 32 of them outside the 2026-09-21 migration backup tree. src/config.rs declares warmup_limit twice - MemoryConfig defaults to DEFAULT_WARMUP_LIMIT = 50 and HooksConfig to 10 - so the section must be attributed before any comparison; that part of my caution was right, the count was not. Traced to commit 6b6d28d (3.7.0, 2026-07-06) which lowered the hooks default from 50 to 10. Every affected config predates it and was written by the generator that materialised all defaults into the file, which is the exact failure commented_default_toml exists to prevent: a key PRESENT in the file takes the file's value, so those roots are still injecting 5x the intended SessionStart warmup on every session.
