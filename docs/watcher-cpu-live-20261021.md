# Isolated daemon watcher CPU check — 2026-10-21

Story: #015-88aa. The coordinator narrowed the remaining live check to the same watcher event flood, bounded CPU, and no rescans of another repository. This experiment does not deploy a binary or change the shared daemon.

## Contract and fixture

Use two private repositories with a root document collection (`_root`, `.`, `**/*.md`) and the default code index enabled. Prove both watchers are active by editing a document in each repository and observing a completed document-reindex operation for each. Then flood only the noisy repository with the original regression's 300 object-file writes under `target/debug` and 300 fake WAL-file writes under `.mdkb`.

The oracle is the native filesystem watcher, the isolated daemon's own PID CPU counters, its debug mutation log, and persisted document rows. A second document edit in the noisy repository during the measurement proves continued watcher delivery. The quiet repository must not run a document reindex, and its persisted document rows must remain identical.

Prespecified checks: zero full-rescan and channel-overflow warnings, zero quiet-repository document reindexes, exactly one noisy-repository document reindex for the control edit, and mean CPU below 20% of one core.

## Native execution and isolation

Source: batch branch at `d25fc77`, with production code inherited unchanged from main `9423a71`.

The worktree binary was built through `tools/build/build-slot.sh cargo build --bin mdkb` with the mbx cargo shim. `mbx doctor` reported zero failures and warnings. The binary was copied immediately to a private evidence path before execution. No installed binary was replaced.

- Private HOME: `/Users/stefano.straus/Gits/.tmp/b3cp3/home`.
- Noisy repository: `/Users/stefano.straus/Gits/.tmp/b3cp3/noisy`.
- Quiet repository: `/Users/stefano.straus/Gits/.tmp/b3cp3/quiet`.
- Daemon PID: `78898`, started and stopped by the experiment harness.
- Private daemon configuration whitelists only the experiment root.
- Setup commands use `MDKB_NO_DAEMON=1` and `MDKB_NO_SPAWN=1` to prevent detached children.
- Daemon runs in the noisy repository, not the repository-container directory.
- Private hook-socket calls open both repositories before the native liveness edits.

A pilot writes three files per excluded prefix. The only scale change is then to 300 files per prefix. The 600-write burst repeats every minute for 30 minutes. Paths and parent directories are precreated so directory creation is not confused with the artifact-write regression. Code indexing remains enabled. The harness records each burst's actual time and duration rather than assuming exact timing.

## Results

Measurement window: **2026-10-21 22:03:10.057361 UTC to 22:33:10.213298 UTC**.

| Observation | Result |
| --- | ---: |
| Wall time | 1800.148 seconds |
| CPU samples | 181 |
| Full 600-write bursts | 30 |
| Small pilot writes | 6 |
| Daemon CPU time across sample endpoints | 0.57 seconds |
| Mean CPU, one-core basis | 0.03175% |
| Highest sampled `ps` CPU | 2.3% |
| Highest interval CPU from cumulative process time | 0.34255% |
| Peak sampled RSS | 69,184 KiB |
| Full rescans | 0 |
| Channel-overflow warnings | 0 |
| Noisy-repository document reindexes | 1, intentional document control edit |
| Quiet-repository document reindexes | 0 |
| Quiet persisted document rows | unchanged |
| Harness exit | 0 |
| Daemon shutdown exit | 0 |

Debug logs identify each document-reindex mutation by repository. After the baseline log offset, only the noisy repository's intentional document edit produces a reindex. The single daemon log has no full-rescan warning anywhere in the measurement segment, so there is no unattributed full rescan to assign to either repository.

At the end, the harness sends SIGTERM only to its own `Popen` child. The log confirms signal receipt, socket unlinking, and both watcher tasks being aborted. No shared daemon lifecycle operation was used.

## Before evidence, failed pilots, and limits

The original RED evidence remains in the main story worklog: on pre-fix `9663330`, `root_collection_churn_does_not_overflow_or_feed_recovery` admitted `target=300` and `.mdkb=302` events into a 100-slot channel and set overflow recovery. The same 300-plus-300 path classes now cause no full recovery in a live native daemon. This batch changes no production behavior and adds no new regression test; the pre-fix evidence is reused, not rerun against already-fixed trunk.

The prior production before/after 30-minute ambient measurements are also retained in the story: before mean 7.8096% and after mean 49.0034% of one core, including a startup spike. A later 10-minute steady-state window averaged 1.43166%. Those workloads were not matched; none proves a causal CPU improvement. This controlled run establishes bounded CPU for the named artifact flood, not an explanation for every earlier production spike.

Two failed experiment attempts are retained:

1. Launching the daemon from the repository-container directory was refused before socket creation. Retry used the private noisy repository as cwd.
2. A worktree build-artifact launch established both watchers and processed the document control, then retired after its executable disappeared. The log names executable replacement as its reason. A private binary copy removed that environmental dependency. The experiment does not diagnose why the worktree artifact disappeared.

The fixture has no installed ONNX embedding model. Initial and control document updates log an auto-embed initialization warning, then persist successfully. This check covers watcher admission/recovery, not embedding inference CPU, model download behavior, Windows, or rb. It does not reproduce the production set of registered repositories. CPU sampling starts after the pilot writes, so its cumulative CPU delta excludes the initial pilot interval; the repeated full bursts remain inside the sampled window.

## Evidence

Root: `/Users/stefano.straus/Gits/.tmp/results/`.

- `mdkb-b3-cpu-r3.log`: native build, binary hash, and successful experiment summary.
- `mdkb-b3-live/daemon_cpu.py`: complete reproducible harness.
- `mdkb-b3-live/cpu-summary.json`: actual timestamps, burst cardinalities and durations, CPU aggregates, rescan/reindex counts.
- `mdkb-b3-live/cpu-samples.jsonl`: 181 timestamped process CPU/RSS samples.
- `mdkb-b3-live/isolated-daemon.log`: registration, watcher liveness, per-repository mutation operations, and shutdown.
- `mdkb-b3-live/fixture-setup.log`: fixture initialization and initial indexing.
- `mdkb-b3-live/daemon-exit.txt`: graceful daemon exit status.
- `mdkb-b3-cpu.log`, `mdkb-b3-cpu-r2.log`, and `mdkb-b3-live/pilot-*` / `r2-*`: failed-pilot evidence.

No production code, protocol, CLI output, or TUICommander consumer was changed by this verification.

## Batch gate and remaining uncertainty

Both native gate runs used one build-slot admission and the mbx cargo shim. Formatting, all-target check, workspace nextest, and all-target Clippy ran independently, so a test failure did not hide later diagnostics. Tests used a per-command temporary directory under `~/Gits/.tmp`.

| Gate | UTC start / end | Wall time | Result |
| --- | --- | ---: | --- |
| 1 | 2026-10-21 22:34:59.225977 / 22:38:54.436022 | 235.204 s | fmt/check/Clippy passed; nextest 3378 passed, 1 failed, 50 skipped |
| 2 | 2026-10-21 22:41:39.906017 / 22:43:35.009395 | 115.102 s | fmt/check/Clippy passed; nextest 3379 passed, 50 skipped |

Gate 1 failed `e2e_daemon_watcher::an_unrouted_burst_neither_drops_events_nor_forces_a_rescan`: one document reindex instead of zero; its code-reindex assertion passed. The failure list is `~/Gits/.tmp/results/mdkb-b3-failures-1.md`.

Temporary per-test DEBUG tracing was used to inspect actual native delivery. The failing test passed alone (4.565 s), and all ten watcher integration tests passed together (see `mdkb-b3-unrouted-diagnostic.log` and `mdkb-b3-watcher-diagnostic.log`). In the isolated trace, no artifact or directory path was delivered during the burst; only the final real source control was delivered. The original failure did not recur. The directory/setup-notification hypothesis remains unproved, not a verified cause.

Temporary tracing was removed before gate 2. **No production or test change was made to obtain the second pass.** No failure was disabled, weakened, or deleted. The intermittent failure remains a critic follow-up; a green retry is not a fix claim. Native gate logs and `gate-summary-1.json`, `gate-summary-2.json`, and matching step JSON files are retained under the evidence root.

`fmt-changed.sh main...HEAD` and `git diff --check` passed. The instruction-link validator still reports the existing root `CLAUDE.md` relative-symlink violation; it was not changed. rb was not retried because the coordinator explicitly authorized native gates during its outage. There is no new Windows evidence. The coordinator owns the separate critic and landing.
