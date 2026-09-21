---
id: fleet-warmup-limit-is-two-fields
title: warmup_limit exists twice; only one root really overrides
entry_type: problem
source_type: user_statement
status: active
tags: [config, fleet, measured, correction]
created_at: 1789990145
updated_at: 1789990145
---

Measured 2026-09-21 while checking a fleet-audit claim that 23 repos froze hooks.warmup_limit at 50 against a shipped default of 10. The claim conflated two different fields. src/config.rs declares warmup_limit twice: MemoryConfig.warmup_limit defaults to DEFAULT_WARMUP_LIMIT = 50, and HooksConfig.warmup_limit defaults to 10. Verified by reading a fresh mdkb init config, which prints memory warmup_limit 50 and hooks warmup_limit 10. Across the Gits tree excluding the temp dir only 5 config files carry an uncommented warmup_limit at all, 2 of them inside the 2026-09-21 migration backup tree. Four of the five have memory 50 and hooks 10, both equal to the shipped defaults - no override at all. Exactly ONE root has hooks warmup_limit = 50, which is the genuine 5x-injection case. One repo, not 23. Lesson: a config key name is not unique across sections, so grepping by key name alone cannot tell you which default it should be compared against.
