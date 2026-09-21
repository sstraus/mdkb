---
id: read-commands-migrate-and-now-say-so
title: "A read command migrates a stale store, and now announces it"
entry_type: decision
source_type: user_statement
status: active
tags: [cli, migration, release, measured]
created_at: 1789996670
updated_at: 1789996670
---

Decided and implemented 2026-09-21 (story 139-cd2f). Context::open_read_only is strict and refuses a stale store with SchemaStale; the CLI wrapper Context::open_read_only_migrating catches that and migrates. That was deliberate and stays - refusing would break every read on the 82 of 102 stores older than v28 - but it was SILENT: the only trace was a tracing::info! that is invisible at the default level. Measured: a copy of LS/quill-builder went from schema 20 to 30 on mdkb graph relations, a command that only reports, and the migration also runs the data-mutating steps (v21 deletes memory entries with unreadable ids, v23 and v26 archive prior clusters, v27 moves prior candidates). The shipped cheatsheet stated the opposite with the direction reversed - 'When the binary is newer, read-only commands refuse rather than migrate silently' - while refuse_future_schema guards only the store-newer direction. FIX: Context::migrated_from carries the pre-migration version, and main::open_reader (one helper replacing the same call at 17 CLI sites) prints one stderr line naming both versions and what the migration rewrites. The cheatsheet now states both directions. RULE FOR SURVEYS: to inspect a store without mutating it, use sqlite3 -readonly, or a db+wal+shm snapshot copy where WAL makes -readonly return CANTOPEN(14). There is still no read-only CLI path.
