---
id: windows-verbatim-prefix-splits-store-paths
title: "Windows \\\\?\\ prefix split the store's own paths"
entry_type: problem
source_type: user_statement
status: active
tags: [windows, canonicalize, memory-sync, paths, measurement]
created_at: 1789744146
updated_at: 1789744146
---

Measured 2026-09-18 on a real Windows host (10.37.2.56, rustc 1.97.0 MSVC), not inferred. Two memory_git_sync tests failed on Test Windows. Instrumenting gitignore_shadow and committed_deletions with eprintln showed: root=C:\Users\...\.tmpX (plain), entries_dir=\\?\C:\Users\...\.tmpX\.mdkb\memory\entries (verbatim prefix). Path::strip_prefix between them returns None, both functions bail at .ok()? and GIT IS NEVER INVOKED. Cause: Context::open_impl canonicalizes mdkb_dir (deliberate, for lock identity) and std::fs::canonicalize returns the \\?\ form on Windows, while Context.root is stored as the caller passed it. Fix: domain::canonicalize_plain drops the prefix, applied at all three Context open paths (src/core/mod.rs:223,389,477). Verified 17/17 on the Windows box. IMPORTANT NEGATIVE RESULT: the first diagnosis — git reads backslash in a pathspec as a glob escape, so route paths through rel_key — was reasoned from source and is FALSE. Reverting the rel_key change leaves the Windows suite green; git for Windows accepts either separator. That wrong fix reached a plan, a story, a commit and a changelog entry before anyone ran it. Third time this prefix has cost something here: also broke SQLite ATTACH in store::heal (salvaged 0 entries over a full file) and git clone (hostname contains invalid characters).
