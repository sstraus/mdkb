---
id: salvage-select-star-column-order
title: "Quarantine salvage copies by position, not by name"
entry_type: problem
source_type: inference
status: active
tags: [salvage, sqlite, heal, data-loss, schema, false-all-clear]
created_at: 1789927687
updated_at: 1789927687
---

salvage_table (src/store/heal.rs) copies each quarantined table with INSERT OR IGNORE INTO main.T SELECT * FROM corrupt.T. Positional. Its doc comment stated the assumption - 'Same schema on both sides means SELECT * column order matches' - and the assumption is false: two stores at the SAME schema version have different PHYSICAL column order when one reached that version by migration (ALTER TABLE ADD COLUMN appends) and the other by CREATE TABLE from schema.rs. Measured 2026-09-20 on the mdkb repo: quarantined copy had last_refuted_at at index 22 and last_audited_at at 23, fresh store had them at 15 and 16, so 113 of 119 memory_entries rows landed shifted - source_type into last_refuted_at, expires_at into last_audited_at, due_at into source_type, created_session into expires_at, created_agent into due_at, projected_at into created_session, projected_hash into created_agent. Symptoms: 'mdkb memory sync' died with 'Invalid column type Text at index: 15, name: last_refuted_at', and source_type (the confidence multiplier input) was blank on 113 entries. memory_entries was the only table hit because it is the only one that gained columns by ALTER TABLE. The salvage logged 'salvaged 113 memory entries' - a success message for a scrambling, the same false-all-clear class as stories 106-b12a and 113-d6df. Repaired by attaching a copy of the quarantined file and UPDATE ... FROM by column NAME; last_refuted_at and last_audited_at were null for all 113 rows in the copy, so nothing real was lost. Story 128-870a fixes the copy to be name-based. Lesson: SELECT * across two databases is never safe, even at the same schema version - pragma_table_info on both sides and copy the named intersection.
