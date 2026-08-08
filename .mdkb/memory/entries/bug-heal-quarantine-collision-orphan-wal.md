---
id: bug-heal-quarantine-collision-orphan-wal
title: Heal quarantine collision and WAL hazard
entry_type: problem
source_type: auto_extracted
status: active
tags: [mdkb, fixed, sqlite, resilience]
created_at: 1784731483
updated_at: 1784731535
---

Fixed in the current mdkb working tree. `store::heal::quarantine` previously used second-resolution names without collision handling, so a second quarantine in the same second could replace an existing forensic copy on Unix. WAL/SHM moves also ignored errors, which could leave an orphaned WAL beside the fresh database path. The implementation now selects a non-conflicting suffix, preserves timestamp parsing for collision suffixes, and rolls back completed main/sidecar renames if any sidecar move fails. A unit test proves existing forensic copies are not overwritten.
