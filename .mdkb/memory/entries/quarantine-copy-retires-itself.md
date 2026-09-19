---
id: quarantine-copy-retires-itself
title: A quarantined index copy retires itself
entry_type: decision
source_type: user_statement
status: active
tags: [mdkb, heal, quarantine, retention]
created_at: 1789829393
updated_at: 1789829393
---

store::heal::sweep_expired_quarantines deletes a *.corrupt-* copy older than QUARANTINE_RETENTION (15 days) on the Context::open path, after the salvage. Story 109-0fa8. Age comes from the .corrupt-<unix_secs> suffix, NOT the mtime: a quarantine renames the file, so its mtime belongs to the healthy generation and can be arbitrarily older. The .report.json sidecar is kept forever — heal.rs already records that a quarantined file on its own has never answered how corruption happened, so the forensics are the 1.4 KB report, not the 56 MB copy. Removal is best-effort because Windows refuses to unlink an open file and a sweep must never fail the open that triggered it. Watch out: any test that plants a copy with a fixed past timestamp (cli_smoke did, 2023-11-14) now has it swept mid-test.
