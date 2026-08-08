---
id: mdkb-quarantine-banner-is-disk-artifact
title: "mdkb 'INDEX QUARANTINED' banner is a stale disk artifact, not a live diagnosis"
entry_type: problem
source_type: user_statement
status: active
tags: [mdkb, quarantine, dx, false-alarm]
created_at: 1785274264
updated_at: 1785274264
---

Symptom: 'mdkb stats' keeps printing '⚠ INDEX QUARANTINED (was corrupt)' on repos whose DB is perfectly healthy (PRAGMA integrity_check = ok).

Root cause: quarantine_reports() in src/store/heal.rs:274 does read_dir(.mdkb/) and flags ANY filename containing '.corrupt-' (excluding -wal/-shm/.report.json). It is a pure disk-artifact scan: historical, no expiry, no auto-cleanup, non-recursive (so quarantine/ and backups/ subdirs do NOT trigger it). The banner persists forever until the operator manually deletes the file.

Three DX defects:
1. No programmatic way out — no 'doctor'/'heal' subcommand clears quarantine artifacts.
2. The remediation hint 'remove .mdkb/<file> to clear' is TRUNCATED by the 72-column frame in stats_render_report.rs, so the only useful action is invisible.
3. The banner does not distinguish 'corrupt now' from 'was corrupt 3 weeks ago'.

Observed 2026-07-28 on tuicommander (gating file index.sqlite.corrupt-1785150725, 185 MB, dated 07-27) and itview (index.sqlite.corrupt-1785151964, 7.2 MB, dated 07-27). Both DBs healthy.

Data verification method: compare memory_entries IDs between live and quarantined DB via 'comm -13'. tuicommander live contained all 3378 IDs of the corrupt copy plus 15 more -> zero loss. itview corrupt copy was genuinely malformed; 'sqlite3 .recover' yielded 13 MB of SQL with ZERO 'INSERT INTO memory_entries' (only FTS shadow rows) -> nothing recoverable.

Gotcha: running 'sqlite3 <corrupt-file> .recover' RECREATES the -shm/-wal sidecars next to the corrupt file. Delete them afterwards.

Resolution: deleted the stale .corrupt-* artifacts (~216 MiB reclaimed); banner cleared in both repos, doc/memory counts unchanged (132/718 and 40/108).

Prevention: corruption recurred repeatedly (tuicommander 07-11, 07-19, 07-21, 07-27; itview 07-17, 07-18, 07-23, 07-27, plus 2 code.sqlite quarantines 07-22 and 07-26). Last event 07-27, one day before commit 3478a2e 'fix(db): stop quarantine from seeding the next corruption' (3.7.6/3.7.7). No new quarantine since — correlation over a single day, NOT proven fixed. Real fix needed: make the banner self-clearing or add a clear-quarantine command, and stop truncating the remediation line.
