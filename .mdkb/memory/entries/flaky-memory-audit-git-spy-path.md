---
id: flaky-memory-audit-git-spy-path
title: "A git spy on global PATH counted sibling tests' calls"
entry_type: problem
source_type: inference
status: active
tags: [testing, flaky, rust, path, git]
created_at: 1789945255
updated_at: 1789945255
---

tests/memory_audit.rs::a_dead_path_cited_from_two_entries_spawns_git_once failed 5/5 on a clean tree at 5cec9bc (left: 3, right: 1) and passed single-threaded. The test prepends a spy 'git' to PATH that logged every invocation, but PATH is process-global and sibling tests in the same binary spawn git without taking env_lock, so their calls inflated the count. Fix: the spy logs only calls carrying '-C <this repo>', which is how git::path_ever_existed invokes it. Lesson for measuring 'is this mine?': stash ALL changes (git stash push -u, verify git status is empty) — stashing one file while seven other edits remain proves nothing.
