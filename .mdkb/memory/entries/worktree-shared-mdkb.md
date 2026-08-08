---
id: worktree-shared-mdkb
title: "Git worktrees share main repo's .mdkb/"
entry_type: decision
source_type: auto_extracted
status: active
tags: [git, worktree, architecture, mdkb]
created_at: 1778853073
updated_at: 1778853073
---

**Problem:** Secondary git worktrees got isolated `.mdkb/index.sqlite` databases — memories written in the main worktree were invisible from release/feature worktrees.

**Decision:** `resolve_main_worktree()` in `src/git.rs` reads `.git` file → follows `gitdir:` pointer → resolves to main worktree root. All worktrees of the same repo share one `.mdkb/` directory.

**How it works:**
1. `.git` is a file (not dir) → it's a worktree
2. Parse `gitdir: <path>` from the file (capped at 512 bytes)
3. Validate path structure: `<main>/.git/worktrees/<name>`
4. Sanity check: `main_root/.git` must be a directory
5. Return main root (or fall back to original root with tracing)

**Integration points:**
- `canonicalize_root()` in `daemon/registry.rs` — registry maps all worktrees to same `RepoHandle`
- `resolve_root()` in `cli/hook_client.rs` — hooks from worktrees find main's memories
- `main.rs` — CLI commands from worktrees use main's `.mdkb/`

**Consequence:** Code index is also shared (lives in same `.mdkb/`). Run `--incremental` reindex when switching worktrees.
