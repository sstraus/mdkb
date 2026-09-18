---
id: windows-git-pathspec-backslash
title: "Windows: git pathspecs need forward slashes"
entry_type: problem
source_type: inference
status: active
tags: [windows, git, memory-sync, pathspec, ci]
created_at: 1789728907
updated_at: 1789728907
---

Test Windows on main failed a_parent_gitignore_shadowing_the_store_is_reported and committed_bulk_deletions_archive_above_the_cap (runs fa56430, 6947292, 60edad6, 2026-09-17). Cause: gitignore_shadow and committed_deletions in src/core/memory_sync.rs pass a Path::strip_prefix result to git; on Windows it carries backslashes and git reads backslash in a pathspec as a glob escape, so check-ignore says not-ignored and ls-tree/log return nothing (every deletion becomes suspect, archived 0). The extended-length prefix was already stripped by 60edad6 and was not the cause. Fix: route both through crate::domain::rel_key (src/domain/mod.rs:31), the existing owner of the separator rule. Audit: no other production git call in src/git.rs or src/core/coupling.rs passes a filesystem-derived path. Plan: plans/windows-git-sync-pathspec.md. Verification is CI only.
