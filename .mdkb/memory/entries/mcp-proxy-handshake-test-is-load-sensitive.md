---
id: mcp-proxy-handshake-test-is-load-sensitive
title: The mcp_proxy handshake test uses a 2s wall clock and fails under build contention
entry_type: problem
source_type: auto_extracted
status: active
tags: [testing, flake, timeout, mcp-proxy]
created_at: 1790071843
updated_at: 1790071843
---

Observed 2026-09-22, once in three full-suite runs.

cli::mcp_proxy::tests::daemon_disconnect_keeps_stdio_open_and_replays_handshake failed with 'timed out waiting for NDJSON line: Elapsed(())' at src/cli/mcp_proxy.rs:516.

Diagnosed, not dismissed. read_json_line wraps reader.read_line in tokio::time::timeout(Duration::from_secs(2)). Two seconds is a WALL-CLOCK budget, so it measures machine load, not the behaviour under test — which is that stdio stays open and the handshake replays after a daemon disconnect.

Cause of the failure: I started cargo test while a cargo clippy --all-targets build was still finishing, so the run competed for the build directory (the log carries 'Blocking waiting for file lock on build directory' and dozens of crates compiling). Re-running the same commit on a quiet machine: 2716 passed, 43 ignored, 57 suites, and the test passed 5/5 in isolation. The same suite took 168s uncontended and 269s while other sessions were busy, which is the same signal.

Two things follow. First, the machine rule in AGENTS.md is real: do not start a suite against a build directory another cargo is still writing. Second, the 2s timeout is a genuine test-quality defect — a hang guard should be far enough out that scheduling noise cannot reach it. NOT changed here, because the file is outside the work that surfaced it; reported to Boss instead.
