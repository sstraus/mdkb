---
id: tuic-silent-number-traps-closed
title: The two TUIC silent-number traps are closed
entry_type: decision
source_type: auto_extracted
status: active
tags: [tuicommander, consumers, api, contract-test]
created_at: 1789933434
updated_at: 1789933434
---

Follow-up to tuic-consumer-surface-and-silent-degradation. Both traps are fixed in tuicommander 685e58c3 (stories 798-4e59, 799-51d8).

1. code_find: MdkbFindResult keeps mdkb's total; capped() returns None when mdkb sent no count, so 'unknown' never collapses into 'complete'. showing is checked against the rows that arrived and a mismatch is an error, not a rendered guess. The Tauri command returns {symbols, total, capped} - free to widen because no TS caller consumes it yet.

2. Line convention: mdkb_reports_symbol_lines_zero_based runs the INSTALLED mdkb over a fixture and asserts symbols_in_file reports line_start 4 for a function on human line 5. It spawns a daemon under an isolated HOME - mdkb's daemon_home() is home_dir()/.mdkb, so overriding HOME on the CHILD process only (never std::env::set_var) gives a private pid file and sockets and cannot disturb the developer's daemon. That trick is reusable for any test that needs a real mdkb daemon.

KNOWN GAP: the contract test no-ops with a stderr note when mdkb is absent, and CI runners have no mdkb. Upstream breakage is caught on a developer machine, not in CI.
