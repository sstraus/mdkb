---
id: mdkb-fourth-corruption-mechanism-open
title: index.sqlite corruption reproduces under 3.7.7 with all three known mechanisms ruled out
entry_type: problem
source_type: user_statement
status: active
tags: [mdkb, corruption, sqlite, open-issue, unresolved]
created_at: 1785275969
updated_at: 1785275969
---

2026-07-28, tuicommander: index.sqlite passed PRAGMA integrity_check at ~23:21 and was corrupt by ~23:50. Damage: 'Tree 16 ... 2nd reference to page' (memory_entries, rootpage 16), same on rootpage 23 (memory_fts_data) and tree 66; freelist size 1817 but should be 1830; wrong # of entries in idx_memory_access/status/type and sqlite_autoindex_memory_entries_1; malformed inverted index for FTS5 memory_fts.

Signature = the doubly-referenced pages 3.7.6 (commit 3478a2e) set out to close. But ALL THREE known mechanisms were ruled out:
  1. Rename-under-open-handle: the live lock WAS held. Verified by probing both sidecars with a non-blocking exclusive flock from python (fcntl.LOCK_EX|LOCK_NB) — both index.sqlite.live.lock and code.sqlite.live.lock reported held. No new *.corrupt-* file existed, so no rename had occurred.
  2. mmap + truncation: no mmap_size is set anywhere in src/ (deliberately — see comments at src/store/mod.rs:74 and src/cli/handlers.rs:102).
  3. auto_vacuum INCREMENTAL / incremental_vacuum: PRAGMA auto_vacuum returns 0 (NONE) on the real DB and there is no incremental_vacuum caller in src/.

So a FOURTH mechanism is open. Root cause NOT identified — do not assume 3.7.6/3.7.7 closed it.

Context at the time: daemon pid 91189 up 13h43m holding per-repo handles; three concurrent 'mdkb mcp' clients (all symlinks to the same 3.7.7 binary, no version skew); another live Claude session writing tuicommander's hooks. Last recorded write before detection: index.sqlite mtime 23:47:09 with index.sqlite.mutation.lock reading 'pid=16570 operation=open-schema' (Context::open, src/cli/handlers.rs:142) — that pid was already dead, an ephemeral hook process. Stopping the daemon is not enough to keep it down: an MCP client respawns it within ~2s, so recovery must stop-and-act in one tight window.

Confounder to be honest about: during diagnosis I ran the SYSTEM sqlite3 CLI (a different SQLite build than mdkb's linked rusqlite) against the live index.sqlite while the daemon held it open — PRAGMA integrity_check and a SELECT on memory_entries at ~23:26. Read-only, but a connecting process writes -shm on a WAL database and can trigger WAL recovery. Cannot be excluded as a contributor; also cannot be shown to be the cause, since the only recorded write in the window is mdkb's own open-schema at 23:47:09. PREVENTION: probe a COPY, never the live file.

RECOVERY (safe, verified): memory is mirrored 1:1 to .mdkb/memory/entries — 3393 markdown files vs 3393 memory_entries rows, so SQLite holds no unique data. Sequence: back up the corrupt file; 'mdkb daemon stop' in a poll loop until a non-blocking exclusive flock on index.sqlite.live.lock succeeds; mv index.sqlite aside and rm -f the -wal/-shm/.integrity-ok sidecars; MDKB_NO_DAEMON=1 mdkb memory import .mdkb/memory/entries; MDKB_NO_DAEMON=1 mdkb update. Result: integrity_check ok, memory 718 active (unchanged), documents 276, DB 173 MB -> 31 MB. Note 'memory import' of 3393 entries exceeds a 2-minute timeout — run it backgrounded.
