---
id: graph-relations-report-migrates-the-store
title: A reporting command migrates the store it reports on
entry_type: problem
source_type: user_statement
status: active
tags: [graph, migration, read-only, release, measured]
created_at: 1789990915
updated_at: 1789990915
---

Reported by the mdkb-fleet-audit agent, measured 2026-09-21 on a fresh copy of a v20 store: schema version before running 'mdkb graph relations' was 20, after it was 30. A command that only reports ran the whole migration chain as a side effect, including the data-mutating v21 to v28 steps - delete unreadable-id memory entries, date undated priors, archive prior clusters, retire untyped matchers, move prior candidates. There is no read-only inspection path in the new binary, so anyone surveying the fleet with a report command silently migrates all 102 stores. The agent's own audit avoided this by using sqlite3 -readonly, falling back to a db+wal+shm snapshot copy where WAL makes -readonly return CANTOPEN(14). Worth a story: a reporting subcommand should open read-only and refuse, or say it is about to migrate.
