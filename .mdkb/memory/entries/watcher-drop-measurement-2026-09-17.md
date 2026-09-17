---
id: watcher-drop-measurement-2026-09-17
title: "Watcher event drops: measured, and the WAL red herring"
entry_type: problem
source_type: auto_extracted
status: active
tags: [watcher, fsevents, code-index, wal, sqlite, measurement]
created_at: 1789654099
updated_at: 1789654099
---

Story 101-58b1. Measured 2026-09-17 by instrumenting FileWatcher::new with an eprintln on try_send failure: a 500-file burst into a watched src/ produced 793 dropped events (channel holds 100) and exactly one flush observing missed=true with 820 paths already batched. The existing rescan-on-drop recovery DID fire and DID cover all 500 files, so the burst path was already healthy — the plan's proposed fix (8a35e2e, 2026-07-07) predates the review that reported the bug.

Two real gaps remained. (1) take_missed_events was only read in the batch-flush arm, which runs only when a batch/doc/memory sync is already pending; a burst whose delivered events all route nowhere (cargo build filling the channel with target/**) left the loop blocked with the flag set. (2) full_code_rescan went through run_code_mutation, which returns None on a closed facade slot, so after corruption closed the index the rescan silently no-opped with the flag already consumed — permanently stale.

The WAL red herring: code.sqlite is journal_mode=wal. Under a daemon holding the connection open, writes land in code.sqlite-wal and the main file's mtime freezes at the last checkpoint. The review's headline evidence ('code.sqlite last updated two days before') is therefore NOT proof of a stale index and must not be used that way again. Use mdkb stats, or max(mtime) over code.sqlite{,-wal,-shm}.
