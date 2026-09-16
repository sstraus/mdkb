---
id: flaky-db-contention-multiprocess
title: db_contention_multiprocess flakes under full-suite load
entry_type: problem
source_type: auto_extracted
status: active
tags: [testing, flaky, sqlite, contention]
created_at: 1789553593
updated_at: 1789553593
---

tests/db_contention_multiprocess.rs::concurrent_processes_and_a_long_lived_connection_leave_one_sound_index failed once on 2026-09-16 during a full cargo test run: five hook_writer threads panicked at line 142, the 'memory rm {id}' assertion, and the test then failed at line 335. It passed standalone (9.1s) immediately after, and the full suite passed twice more (49 binaries, 2382 tests, exit 0). So: load-sensitive, not a code defect introduced by story 082 - that story touched prior_distill/dispatch/setup/config only, nothing on the SQLite write path. NOT yet root-caused: the stderr of the failing 'memory rm' was filtered out of the captured log, so whether it was a busy timeout or a quarantine is unknown. Next time it fails, capture the assertion message before re-running.
