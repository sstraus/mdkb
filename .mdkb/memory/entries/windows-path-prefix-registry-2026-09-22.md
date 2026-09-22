---
id: windows-path-prefix-registry-2026-09-22
title: "The \\\\?\\ prefix, fourth time: registry::canonicalize_root"
entry_type: problem
source_type: user_statement
status: active
tags: [windows, canonicalize, ci, paths]
created_at: 1790096204
updated_at: 1790096204
---

On 2026-09-22 the Windows CI job was red for six days (last green 067a3c8, 2026-09-16). Six rounds of fixes: (1) tests/memory_audit.rs used PermissionsExt::from_mode with no cfg gate, so the memory_audit target did not compile on Windows at all; (2) ten daemon::registry and daemon::repo_map tests built expectations with std::fs::canonicalize while the code under test uses canonical_key, which strips the prefix; (3) registry::canonicalize_root itself used the raw call while repo_map::canonical_key strips it, so the MCP refusal told the caller 'Specify root: \\?\C:\Users\...' — a path that compares equal to nothing. Fixed in production: canonicalize_root now goes through domain::canonicalize_plain; (4) the Windows runner hands out a TempDir under the 8.3 short name C:\Users\RUNNER~1\..., while the registry keys the long runneradmin form; (5) an assertion read format!({err:?}), whose debug escaping doubles every backslash, so it compared a path against its own escape; (6) tests/cross_repo_search.rs fixtures fed prefixed roots into handle_init and resolve_single_root, which answered 'No repos registered' and read as a product defect. Remaining after all six: session_start_on_uninitialized_project_returns_silence still emits the cheatsheet line on Windows. Unproven hypothesis, NOT measured: %TEMP% on Windows sits under the user profile, so the walk up from a temp project reaches ~/.mdkb (the daemon home) and finds a store where a unix /tmp project finds nothing. Needs eprintln on a real Windows run before anybody writes it down as the cause.
