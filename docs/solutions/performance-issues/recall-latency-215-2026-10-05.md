# Recall latency 215: warm TUIC replay on 2026-10-05

## Result

The three reported TUIC inputs do not reproduce a search over 300 ms on the
installed macOS daemon with a warm index and load1 5.09. Five replay requests
returned in 67–119 ms; their search phases were 49–67 ms. No code, cache or
configuration change is justified by this experiment. This verifies the
low-load replay branch of criterion 3 for today's samples. It does not establish
an exclusive cause for each historical phase; criterion 1 remains open.

## Historical evidence

Times below are CEST (UTC+2). Hook rows come from
`~/Gits/personal/tuicommander/.mdkb/hook-events.jsonl`; the slow log contains the
same rows, without additional attribution or prompt text.

| Time / Unix timestamp | Total | Search | Embed | Lock wait | Evidence and limit |
| --- | ---: | ---: | ---: | ---: | --- |
| 15:47:47 / 1791208067 | 1002 ms, deadline | 773 ms | 171 ms | 0 ms | Build admissions recorded load1 17 at 15:44:14 and 11 at 15:44:52; both admitted tests finished before this prompt. No simultaneous load measurement. |
| 16:44:17 / 1791211457 | 792 ms, fired | 692 ms | 81 ms | 0 ms | Daemon logged `hook read held_ms=692` at 14:44:17.651400Z. Other repositories reindexed for 371–805 ms in the same minute and a watcher channel overflowed at 14:44:16. No simultaneous load measurement. |
| 16:54:08 / 1791212048 | 883 ms, skipped | 824 ms | 43 ms | 0 ms | Daemon logged `hook read held_ms=824` at 14:54:08.946692Z. TUIC nextest was admitted at 16:50:36 with load1 9; the build ledger records duration 211 seconds (ending about 16:54:07). This is adjacent build activity, not a measured load at the hook. |

`~/Gits/.tmp/build-slot/admissions.tsv` supplies the numeric load measurements;
`builds.tsv` supplies commands and durations. The nearby health reports
`1791207698.md`, `1791207820.md` and `1791210811.md` do not record load averages.
The 16:33 report records one 300 ms UI freeze at 16:24:47.919, not at a sample.
Remote rb suites do not by themselves prove local CPU contention.

All three rows have context=0 and lock_wait=0. Rerank was off on the first two;
the third had rerank=0 and outcome=no_budget. These measured phases do not
explain the search time. The search phase includes blocking-pool scheduling,
memory injection and observation queries, document recall and, when enabled,
rerank candidate retrieval. A held-slot duration does not distinguish CPU
execution, descheduling or I/O. Host contention is plausible, not proven.

## Recorded-input replay

The input sources are Codex rollouts:

- `rollout-2026-10-05T15-47-37-01a10c51-eba8-7fa0-916f-da98087c11f2.jsonl`
- `rollout-2026-10-05T16-44-07-01a10c85-a5d8-7b92-ac1d-daaec0ca1b41.jsonl`

Each startup has an AGENTS block and a task block around the hook timestamp.
Because the hook log has no input fingerprint, both blocks were replayed;
neither is silently assumed to be the exact historical input. The 16:54 input
is the recorded 138-character `BG DONE` message. No hand-written fixture was
used. Raw prompts remain in the task evidence directory, not this document.

The replay sent length-prefixed JSON-RPC `hook.user_prompt_submit` requests to
`~/.mdkb/daemon-hook.sock`, with root set to the main TUIC checkout and a fresh
session ID for each request to avoid dedup bypass. It used the existing daemon
(pid 78335), without restart or configuration change. Every request recorded
`sysctl -n vm.loadavg` immediately before dispatch. Normal hook telemetry and
recall ledger writes occurred in the TUIC store.

| Input | Characters | Socket total | Hook total | Search |
| --- | ---: | ---: | ---: | ---: |
| 15:47 AGENTS | 35620 | 119.3 ms | 119 ms | 67 ms |
| 15:47 task | 11562 | 95.5 ms | 95 ms | 53 ms |
| 16:44 AGENTS | 35599 | 115.4 ms | 115 ms | 62 ms |
| 16:44 task | 11042 | 98.1 ms | 97 ms | 53 ms |
| 16:54 BG DONE | 138 | 67.7 ms | 67 ms | 49 ms |

All five measured load averages were `{ 5.09 8.59 10.48 }`; the criterion uses
**load1**, not the five- or fifteen-minute averages. Event timestamps were
1791223441–1791223442 (20:04:01–20:04:02 CEST).
Earlier warm-up/replay requests at load1 6.88–8.73 returned in 64–210 ms with
search 52–148 ms; those are not substituted for the low-load evidence.

The current index contains 3830 documents, 12797 chunks, 2087 memories,
3700 document embeddings and 2087 memory embeddings. Historical index contents
were not snapshotted. The current replay is therefore not a controlled replay
of the historical database bytes or concurrent process state.

Evidence: `~/Gits/.tmp/mdkb-215/replay.json` and `replay-low.json`, plus the
recorded input files `prompts-1547.json`, `prompts-1644.json`, `prompts-1654.json`.
The historical source log is `~/.mdkb/logs/daemon.log`.

## Validation and remaining work

Native macOS runtime evidence only; no Cargo build, suite or mutation run.
Instruction-link validation passes in this worktree. Documentation checks:
`git diff --check` and the required `fmt-changed.sh <worktree> main...HEAD`.
The runtime replay checks criterion 3 for this assigned sample set; it does not
close the original cerebro/prior-phase attribution problem. Per-event host
load and a search subphase CPU/I/O profile are missing for criterion 1.
