---
id: flaky-watcher-set-level-breaker
title: Watcher set-level breaker test is load-flaky
entry_type: problem
source_type: auto_extracted
status: active
tags: [testing, flaky, watcher, daemon]
created_at: 1789569782
updated_at: 1789569782
---

e2e_daemon_watcher::watcher_deletions_still_go_through_the_set_level_breaker failed once inside a full 'cargo nextest run' on 2026-09-16 (13.2s, FAIL) and passed alone in 1.75s, then passed in a second full run of the same tree (2423/2423). It waits for a filesystem-watcher debounce plus an observed reconciliation pass, so a saturated machine can push it past its deadline. Not caused by the story-088 archive-then-remove change: the test only uses handle_memory_add and sync_memory_files, never rm or prune. Raised with Boss rather than silently retimed - the deadline needs measuring before it is moved.
