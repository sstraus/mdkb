---
id: daemon-config-never-reloaded
title: Daemon never reloaded .mdkb/config.toml (fixed)
entry_type: problem
source_type: user_statement
status: active
tags: [config, daemon, hooks, shadow, measured, fixed]
created_at: 1790698938
updated_at: 1790717576
---

Measured 2026-09-29 (mdkb 3.11.1): RepoHandle::open loaded config.toml once, so edits were ignored until LRU eviction or restart (3 skipped prompts over 12s with shadow enabled after open). Fixed in 618765c (story 180-b051): registry compares (len, mtime) on each access and reopens over the shared store; invalid TOML keeps the last good config. Prevention: a store-schema bump migrates on the first read by a new binary; rebuild target/release before running a dev build against a live repo, or the running daemon refuses the store until it retires (ExeIdentity, 30s).
