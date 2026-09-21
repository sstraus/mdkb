---
id: lowering-priority-is-one-way
title: "A process can nice itself down, never back up"
entry_type: problem
source_type: user_statement
status: active
tags: [scheduling, design, measured, rust]
created_at: 1790001884
updated_at: 1790001884
---

Measured 2026-09-21 while fixing mdkb embed saturating 10 of 14 cores. An unprivileged process may lower its own scheduling priority with setpriority and may NOT raise it back - the raise returns EPERM. Confirmed twice: a python probe (os.nice(5) then setpriority back to 0 raises PermissionError errno 13) and a Rust test asserting setpriority returns -1. THIS KILLED THE FIRST DESIGN. I had written an RAII guard that lowered priority on acquire and restored it on Drop, with tests for the normal path and the panic path. Both failed, and in isolation the failure was not the parallel-test interference I first assumed - it was that the restore silently could not happen. A guard that cannot restore is worse than no guard: it would have demoted a long-lived daemon permanently while reading as scoped. WHAT FOLLOWS: the call is one-way and named lower_process_priority to say so, and WHERE it is called becomes the entire design. Only a process that exists to do one job and then exit may lower itself - the mdkb embed CLI arm. tests/layering.rs asserts the symbol is unreachable from src/daemon, src/mcp, src/core and src/store, so the constraint is enforced rather than written in a comment. Also measured: priority is per PROCESS, so concurrent tests touching it measure their own interleaving; serialise them behind a mutex and say why. And priority, not a thread cap, is what implements 'fast when the CPU is free, slower under contention' - a static cap cannot do the first half.
