---
id: db-contention-test-hit-ambient-daemon
title: Multiprocess contention test wrote via the ambient daemon
entry_type: problem
source_type: auto_extracted
status: active
tags: [testing, sqlite, daemon, isolation, root-caused]
created_at: 1789554404
updated_at: 1789554404
---

tests/db_contention_multiprocess.rs spawned 'mdkb memory add/show/rm' children WITHOUT MDKB_NO_DAEMON=1, so each child handed its write to whatever mdkb daemon was running on the machine instead of opening the store itself. The file exists to reproduce cross-process contention on one index.sqlite; routing through one shared daemon meant it exercised none of it. Surfaced 2026-09-16 when SCHEMA_VERSION went 22 to 23: every worker failed instantly with 'store schema is v23, but this mdkb binary understands v22' from the installed (older) daemon. This is also the cause of the earlier intermittent failure at line 142 that was recorded as an unexplained flake - it was the ambient daemon, not a race. Fix: mdkb_child() helper sets MDKB_NO_DAEMON=1 and current_dir on all four spawn sites. After the fix the test runs 23.7s (real contention) instead of failing in 0.15s or passing quickly through the daemon. Rule: any test spawning the mdkb CLI must set MDKB_NO_DAEMON=1 unless it is specifically testing daemon routing.
