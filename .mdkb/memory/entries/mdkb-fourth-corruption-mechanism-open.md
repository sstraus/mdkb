---
id: mdkb-fourth-corruption-mechanism-open
title: Fourth corruption path closed operationally
entry_type: problem
source_type: user_statement
status: active
tags: [mdkb, corruption, sqlite, resolved, superseded]
created_at: 1785275969
updated_at: 1786282128
---

Superseded by mdkb-3-7-12-writer-recovery. Historical incidents proved doubly referenced pages while older mmap, auto-vacuum, and rename-under-open hypotheses were ruled out. The exact first low-level corruption trigger remains unproven, but 3.7.12 closes the demonstrated uncoordinated writer surfaces, stale integrity marker, cached-connection verification, swallowed SQLITE_CORRUPT, and daemon live-lock recovery loop. Full CI and live TUICommander recovery are green.
