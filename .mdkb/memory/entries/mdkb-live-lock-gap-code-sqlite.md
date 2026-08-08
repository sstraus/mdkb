---
id: mdkb-live-lock-gap-code-sqlite
title: "3.7.6 live-lock fix is incomplete: three code.sqlite opens bypass it"
entry_type: problem
source_type: user_statement
status: active
tags: [mdkb, corruption, sqlite, concurrency, live-lock]
created_at: 1785274785
updated_at: 1785274785
---

Root cause of the recurring index/code corruption IS diagnosed in commit 3478a2e (3.7.6): autoheal renamed index.sqlite/code.sqlite plus -wal/-shm while other processes (notably the daemon, which keeps per-repo handles alive for days) still had the DB open. SQLite binds a connection to the inode but derives -wal/-shm from the PATH, so once the path was recycled onto a fresh database a surviving connection could land its frames in the replacement's WAL -> doubly-referenced pages. Explains why the 3.7.4 SQLite bump did not help and why code.sqlite corrupted too.

Fix is structural (not a guard/flag): every connection holds a shared *.live.lock for its lifetime; quarantine renames only when it can take that lock exclusively, else returns Heal::CorruptInUse and leaves the file in place. live.lock is a separate sidecar from mutation.lock so a live connection never blocks an index-wide write.

GAP FOUND 2026-07-28: the commit message claims 'Every connection now holds a shared *.live.lock for its lifetime'. Not true — three production code.sqlite opens bypass both the live lock and quarantine_if_corrupt:
  1. src/main.rs:563 — 'mdkb compact' — raw Connection::open + VACUUM (rewrites the whole DB). READ-WRITE, no live lock.
  2. src/cli/stats_report.rs:324 — 'mdkb stats' collect_code() — Connection::open then run_repairs() which issues DELETEs (src/code/storage/repair.rs:74-99). READ-WRITE, no live lock.
  3. src/mcp/dispatch.rs:2820 — hook code-staleness check — open_with_flags SQLITE_OPEN_READ_ONLY. No live lock, but read-only connections do not inject WAL frames, so lower risk.

Properly guarded: CodeIndex::new/open/open_or_create (src/code/storage/sqlite.rs:52,68 via acquire_live_code_lock at :588) and both index.sqlite opens (src/cli/handlers.rs:160,247).

Exposure: requires a quarantine concurrent with 'mdkb compact' or 'mdkb stats'. Narrow window, but quarantine_if_corrupt runs on EVERY CodeIndex::open and the hooks open it constantly. Irony: 'mdkb stats' — the command that reports the quarantine banner — is itself one of the unguarded writers.

Proposed fix: wrap sites 1 and 2 in mutation_lock::acquire_live_shared before Connection::open. Site 3 is a judgment call (read-only). Decide whether read-only connections should also announce themselves.
