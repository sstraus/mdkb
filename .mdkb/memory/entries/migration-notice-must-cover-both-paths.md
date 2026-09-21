---
id: migration-notice-must-cover-both-paths
title: A notice on the read path alone missed the worse one
entry_type: problem
source_type: user_statement
status: active
tags: [cli, migration, measured, incomplete-fix]
created_at: 1789998451
updated_at: 1789998451
---

Story 139 shipped 2026-09-21 making a stale-store migration visible, and scoped the notice to read commands because the trigger was a report command (mdkb graph relations) migrating a v20 store to v30. That left the WRITE path silent, and the write path is worse: mdkb update is what an operator deliberately runs on an old store, and it performs the same data-mutating steps (v21 deletes memory entries with unreadable ids, v23 and v26 archive prior clusters, v27 moves prior candidates). Found by the mdkb-fleet-audit agent with a controlled test while executing a collections batch - two rsync copies of one v20 store, mdkb stats printed the line, mdkb update printed nothing, both reached v30 - which meant the batch could not satisfy the per-store evidence condition I had set. Fix: Context::open_impl reads the schema version BEFORE init_schema, because afterwards nothing remains to say it happened, and records it like the read path; main::open_writer mirrors open_reader across 10 call sites and both route through one announce_migration so the two cannot drift into describing the same event differently. LESSON: when a defect is 'X happens invisibly', the fix is scoped by WHERE X HAPPENS, not by where you first saw it. I had the symptom on a read command and wrote the fix for read commands.
