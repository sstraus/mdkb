---
id: windows-parity-series-overlap-with-main
title: "Windows parity series: what main already covers"
entry_type: decision
source_type: inference
status: active
tags: [windows, pr-review, upstream, triage]
created_at: 1789564967
updated_at: 1789564967
---

smuchow1962's six-PR Windows series (issue #12) against main at 0e2aa98: #15 and #14 closed obsolete. #13 is NOT obsolete — only src/cli/setup.rs, src/daemon/ipc_server.rs, src/daemon/mod.rs conflict; the new src/domain/paths/ module (522 lines) has no upstream counterpart. His PR 4 (8 MiB stack) is already main's src/main.rs:151, and the comment there refutes his command_to_json diagnosis: clap's builder is the cost, not the recursive walk. His PR 5 (hooks off Unix) is already main's ffb2101, which extracted hook dispatch into a portable src/daemon/hook_runtime.rs and left ipc_server fully cfg(unix) — a module boundary instead of his nine per-item cfgs. PRs 3 and 6 have no overlap. Issues #6 and #7 are fixed in source on main but left OPEN: Test Windows cannot compile (unresolved import crate::daemon::ipc_server), so neither can be confirmed on the platform until #13 lands.
