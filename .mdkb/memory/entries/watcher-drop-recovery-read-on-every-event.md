---
id: watcher-drop-recovery-read-on-every-event
title: "Step 19: a drop alone arms the flush; the WAL mtime proved nothing"
entry_type: decision
source_type: user_statement
status: active
tags: [watcher, fsevents, code-index, wal, sqlite, review-2026-09-16]
created_at: 1789661183
updated_at: 1789661183
---

Plan Step 19 (marked 'Still open — do not choose before one reproduced drop sequence exists') / story 101-58b1, landed a6c918f. The sequence was reproduced first, then the choice was made, so the condition the plan set is met.

Measured 2026-09-17: a 500-file burst into a watched src/ drops 793 events (the channel holds 100) and the existing rescan-on-drop recovery (8a35e2e, 2026-07-07) covered all 500. The burst path was already healthy — the fix the plan proposed predates the review that reported the bug.

Two real gaps, both fixed. (1) The drop flag was read only in the flush arm, which runs only when a batch, a doc update or a memory sync is already pending. A burst whose delivered events all route nowhere — a cargo build filling the channel with target/** — left the loop blocked with the flag set and nothing scheduled to read it. Decision: read it on every delivered event, and let a drop alone arm the flush. (2) The rescan went through run_code_mutation, which returns None on a facade slot a corrupt database had closed: it did nothing, said nothing, and the flag that scheduled it was already spent. It reopens the index first.

Rejected the review's headline evidence: code.sqlite is journal_mode=wal, so under a daemon holding the connection open its mtime freezes at the last checkpoint. 'code.sqlite last updated two days before' was never proof of a stale index, and story criterion 4 was rejected on that ground.
