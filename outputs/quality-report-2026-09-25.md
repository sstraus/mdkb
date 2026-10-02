# mdkb code quality review — 2026-09-25

**Status:** measured review complete at the declared sample depth; all five selected files were sampled. The T2 gate did not converge because two attacks exposed defects and mutation left meaningful survivors. Review branch: `review/adversarial-tdd-trial`, attack-test commit `cc45457`. No production code was changed.

## Summary verdict

| Area | Verdict | Evidence |
|---|---|---|
| Store | Weak | The quarantine sweep deletes an expired collision copy that has no successful salvage report (reproduced, story 171-8ee1). `src/store/heal.rs` has 95.0% measured line coverage, showing why coverage alone is insufficient. |
| Memory | Weak | Fifteen archived vectors hide a live duplicate from the fixed-size KNN fetch (reproduced, story 172-e3c6). `src/store/memory.rs` has 97.8% measured line coverage. |
| Index/search | Adequate | A targeted update rejected a symlink outside the project root in a real store. `src/core/indexing.rs` has 85.7% line coverage. Parser branches remain a hotspot. |
| Daemon/MCP | Weak | `run_server` has CRAP 930 at 0% function coverage; `cli_mutate_impl` has CRAP 411 at 17.1%. The broader MCP files have good aggregate coverage, so these specific paths need focused tests. |
| CLI | Weak | `run_cli` has CCN 305 and CRAP 2175 despite 72.8% function coverage; `src/main.rs` has 58.0% line coverage. |
| Code graph | Adequate | `src/store/graph.rs` has 98.0% measured line coverage; the index/search and mutation findings limit confidence in paths that feed it. |

These are review judgments. Mutation results are sampled and do not prove the untested candidates.

## Method and scope

The review used the empty Git tree `4b825dc642cb6eb9a060e54bf8d69288fbee4904` as the diff base. `detect.sh` therefore selected the repository. Coverage ran `cargo llvm-cov nextest --lcov --output-path lcov.info` over library and integration targets; 2,716 tests passed and 41 `#[ignore]` tests were skipped. The 41 include tests that need an ONNX model. The CRAP command used `--all --json`; the figures below filter its output to `src/` (4,505 functions). They do not treat tests, docs, or generated files as production hotspots.

Only macOS behavior was executed. Windows behavior, ONNX-dependent tests, and external-service tests have no new evidence from this review. All Cargo commands used the mbx shim after a clean `mbx doctor` (0 failures, 0 warnings). Two coverage starts were abandoned while the inherited shell had `CARGO_TARGET_DIR` and nested `MBX_*` variables; a later clean-environment build was also slow, so the cause of those starts is not established. They are execution cost, not successful checks. From Boss's 14:54 rule onward, every shell command used `TMPDIR=$HOME/Gits/.tmp/mdkb-skill-trial/`; an already-running indexing mutation was stopped before its baseline completed. The machine-wide `BUILD_FREEZE` then barred further test commands. At 19:53 a temporary CPU-control rule prohibited worktree mutation; the 19:56 revision expressly exempted this mdkb trial. Indexing, server, and dispatch mutation then resumed with `nice -n 5` after load and memory checks.

## Measurements

| Step | Wall time | Maximum RSS | Result |
|---|---:|---:|---|
| `ensure.sh --dry-run` | 0.22 s | 36.1 MB | All five tools already present |
| `ensure.sh` | 0.15 s | 36.2 MB | No install |
| `detect.sh --base <empty>` | 0.25 s | 9.8 MB | Rust commands found |
| Full instrumented coverage | 1,005.23 s | 2.30 GB | 2,716 pass, 41 skipped; build 13m 18s, tests 156.53 s, report generation included |
| `crap.py --lcov ... --base <empty> --all --json` | 3.29 s | 110.1 MB | 4,505 `src/` functions; exit 1 because hotspots exceed 30 |
| Quarantine attack RED run | 260.41 s | 2.21 GB | Expected failure, story 171-8ee1 |
| Memory + index attack run | 112.83 s | 1.74 GB | Memory expected failure, index passed |
| Final focused attack run | 111.39 s | See raw log | Index passed; two defect tests ignored with story IDs |
| Review mutation, `heal.rs`, first attempt | 900.06 s | Not available (terminated before `time` footer) | Native `onig_sys` baseline build timed out before any mutant |
| Review targeted warmup | 319.37 s | 2.54 GB | `cargo nextest run --lib`; completed native build, 40 heal tests passed |
| Review mutation, `heal.rs`, warm retry | 369.10 s | 2.48 GB | 2 sampled mutants, both survived |
| Review mutation, `memory.rs` | 837.53 s | 1.71 GB | 5 sampled mutants: 4 detected, 1 unviable |
| Review mutation, `indexing.rs`, interrupted | 33.72 s | Not available (interrupted) | Stopped at Boss's machine-wide build freeze, before mutant execution |
| Review mutation, `indexing.rs`, resumed | 360.79 s | 1.65 GB | 2 sampled, both survived; baseline 135 s build + 3 s test |
| Review mutation, `server.rs` | 357.86 s | 2.43 GB | 3 sampled: 1 survived, 2 unviable; baseline 32 s build + 4 s test |
| Review mutation, `dispatch.rs` | 869.44 s | 2.50 GB | 7 sampled: 6 survived, 1 unviable; baseline 110 s build + 5 s test |
| Dispatch canary filter attempt | 410.59 s | Not available (interrupted) | Regex selected five variants, including four unrelated field deletions; stopped after one unrelated survivor, no canary conclusion |

The two abandoned coverage starts consumed 385.51 s and 533.71 s. A non-instrumented targeted build was also stopped after 607.92 s of native dependency compilation. They are excluded from the successful coverage wall time but included in the trial's overall cost.

One-minute `vm.loadavg` at command start→end: review heal first attempt 26.86→45.22; targeted warmup 22.37→23.62; heal retry 20.09→32.25; memory mutation 36.99→48.23; indexing mutation 48.93→51.83 (interrupted), resumed indexing 23.53→29.59, server 16.62→47.11, dispatch 23.12→37.80, dispatch canary attempt 22.13→27.32. The setup, full coverage, CRAP, and attack timings above preceded Boss's start/end measurement note, so both endpoints are unavailable for those commands. Historical gate and reference-command endpoints are listed in “Execution impact.” The 30-second mutation load series are retained in the trial's raw measurement directory until handoff. The resumed indexing, server, and dispatch runs sampled load1 ranges of 18.85–57.50, 13.33–46.27, and 15.96–53.70 respectively. A later load1 sample reached 143.19 while this review had no heavy command active, illustrating the uncontrolled background contention.

## CRAP

There are 48 `src/` functions above CRAP 30 and 18 above 100. Fifty-three functions have no LCOV line records; the script scores those as 0% coverage. This is a conservative ranking, not proof that all 53 are compiled on this platform.

| Rank | CRAP | CCN | Function coverage | Function |
|---:|---:|---:|---:|---|
| 1 | 2175.19 | 305 | 72.8% | `src/main.rs:306 run_cli` |
| 2 | 930.00 | 30 | 0.0% | `src/mcp/server.rs:1053 run_server` |
| 3 | 462.47 | 33 | 26.7% | `src/core/cli_mutation.rs:260 execute_context_mutation` |
| 4 | 411.06 | 26 | 17.1% | `src/mcp/dispatch.rs:5770 cli_mutate_impl` |
| 5 | 342.00 | 18 | 0.0% | `src/cli/setup.rs:208 handle_setup_mcp_claude` |
| 6 | 298.88 | 63 | 61.0% | `src/mcp/dispatch.rs:2386 get_impl` |
| 7 | 240.00 | 15 | 0.0% | `src/core/memory.rs:971 handle_memory_condense` |
| 8 | 226.79 | 17 | 10.1% | `src/main.rs:2207 print_routed_result` |
| 9 | 210.00 | 14 | 0.0% | `src/core/ops.rs:767 handle_vsearch` |
| 10 | 156.00 | 12 | 0.0% | `src/mcp/https_server.rs:83 ensure_self_signed_cert` |
| 11 | 156.00 | 12 | 0.0% | `src/code/parsing/kotlin/mod.rs:744 find_defines_in_node` |
| 12 | 152.02 | 18 | 25.5% | `src/core/code.rs:446 handle_code_parse` |
| 13 | 132.17 | 25 | 44.4% | `src/mcp/dispatch.rs:2189 get_batch_impl` |
| 14 | 132.00 | 11 | 0.0% | `src/core/memory.rs:1170 find_related_entries` |
| 15 | 132.00 | 11 | 0.0% | `src/code/parsing/rust/parser.rs:745 collect_inner_docs` |
| 16 | 128.55 | 26 | 46.7% | `src/main.rs:2410 format_update_result` |
| 17 | 110.00 | 10 | 0.0% | `src/mcp/server.rs:429 init_and_take_for_reindex` |
| 18 | 110.00 | 10 | 0.0% | `src/code/parsing/kotlin/mod.rs:782 find_uses_in_node` |
| 19 | 90.00 | 9 | 0.0% | `src/main.rs:3033 format_condense_result` |
| 20 | 90.00 | 9 | 0.0% | `src/code/parsing/csharp/mod.rs:654 find_implementations_in_node` |
| 21 | 77.29 | 14 | 31.4% | `src/store/stats.rs:529 get_query_metrics` |
| 22 | 72.00 | 8 | 0.0% | `src/store/documents.rs:406 get_statuses_batch` |
| 23 | 72.00 | 8 | 0.0% | `src/cli/setup.rs:1337 handle_remove_mcp_codex` |
| 24 | 63.75 | 22 | 55.8% | `src/core/ops.rs:36 handle_embed` |
| 25 | 62.89 | 59 | 89.6% | `src/mcp/dispatch.rs:4605 hook_user_prompt_submit_impl_with_dedup` |

`run_cli` is a large command dispatcher: its CCN inflates CRAP, but its untested branches still need a consumer-level review. `store/heal.rs` and `store/memory.rs` have no functions above 30, yet the two measured defects are there. Thus the unexplained hotspots are the uncovered mutation, socket, condense, and parser paths; a low CRAP score must not remove high-consequence state transitions from review scope.

## Risk selection

The five files were ranked by consequence first, then by CRAP within a domain. The CRAP sum is descriptive, not a formal risk score: a longer file naturally contributes more functions. This ranking puts irreversible store changes and externally reachable state transitions ahead of low-consequence formatting complexity.

| Rank | File | CRAP sum / maximum | Why selected |
|---:|---|---:|---|
| 1 | `src/store/heal.rs` | 179.2 / 13.3 | Irreversible quarantine deletion; a measured defect exists. |
| 2 | `src/store/memory.rs` | 473.5 / 25.3 | Memory write admission and duplicate identity; a measured defect exists. |
| 3 | `src/core/indexing.rs` | 216.9 / 35.3 | Path identity and deletion boundaries can lose or misattribute indexed content. |
| 4 | `src/mcp/server.rs` | 1,468.6 / 930.0 | Daemon socket, root authorization, and watcher lifecycle expose state to clients. |
| 5 | `src/mcp/dispatch.rs` | 1,962.9 / 411.1 | MCP operations and hooks mutate store and session state. |

`src/core/cli_mutation.rs` has a maximum CRAP of 462.5 and remains a priority recommendation; the five-file limit favored the store, index, and daemon entry points above this additional CLI path. The selected files contain 121, 412, 105, 205, and 691 enumerated mutants respectively (1,534 total).

## Mutation

The five selected files enumerate 1,534 candidates. The declared sampling method is `--shard 1/100` per file, always `--in-place --file <path> --test-tool=nextest --timeout 300`, with a relevant nextest target. The mutation budget is 90 minutes total, including the first timed-out baseline. The machine-wide build freeze arrived after 2,140.41 s (35m 40s) of mutation commands. After the mdkb-specific exception, indexing, server, and dispatch consumed another 1,588.09 s. A further 410.59 s canary-filter attempt was stopped. Total mutation wall is 4,139.09 s (68m 59s), leaving 21m 1s of the declared 90-minute ceiling unused because all five file samples were complete and the canary filter did not select cleanly. The 319.37 s targeted warmup is recorded separately as setup cost. **No convergence claim is made.**

| File | Enumerated | Sample completed | Detected | Survived | Unviable | Sample score | Untested / status |
|---|---:|---:|---:|---:|---:|---:|---|
| `src/store/heal.rs` | 121 | 2 | 0 | 2 | 0 | 0% | 119 untested |
| `src/store/memory.rs` | 412 | 5 | 4 | 0 | 1 | 100% of 4 viable | 407 untested; the RED duplicate defect remains |
| `src/core/indexing.rs` | 105 | 2 | 0 | 2 | 0 | 0% | 103 untested; first attempt stopped by freeze |
| `src/mcp/server.rs` | 205 | 3 | 0 | 1 | 2 | 0% of 1 viable | 202 untested |
| `src/mcp/dispatch.rs` | 691 | 7 | 0 | 6 | 1 | 0% raw; `mutants.py` marks run invalid | 684 untested |

The two open survivors in `heal.rs` are `src/store/heal.rs:54:75: replace / with %` and `... replace / with *`. Both change `QUARANTINE_RETENTION_DAYS` away from 15 while the 40 targeted heal tests pass. They are meaningful gaps in the retention value exposed through status, not equivalent mutants. The two `indexing.rs` survivors replace `UpdateRequest::is_targeted` with `true` or `false`; the selected `graph_identity` target did not prove the full-versus-targeted branch. Full-suite LCOV recorded 100% coverage for this function, so this is a test-selection gap, with repository-wide mutation strength not established. The `server.rs` survivor deletes `whitelist_dirs` from `McpServer::hook_runtime`. That function is gated by `http-server`, while Cargo defaults to no features; the default mutation run did not compile this path. It is a meaningful feature-coverage gap, not evidence that a tested runtime ignored the whitelist. The six `dispatch.rs` survivors are all meaningful for the selected 165-test target: `format_symbol` → `xyzzy`; `format_symbol_with_file_tokens` → empty or `xyzzy`; `relative_time_ago` → empty or `xyzzy`; and `relative_time_ago` subtraction → addition. They leave symbol output and relative-time output unproven by that target. The full-suite LCOV reports 100% function coverage for all three helpers, so this is first a test-selection gap; it does not establish that the entire repository suite would miss them. The one unviable dispatch mutant replaces `resolve_document` with `Ok(Default::default())`. No survivor in the completed `memory.rs` sample needs classification. The fixed `--shard 1/100` selection was shallow: all seven dispatch mutants came from lines 111–161 of an 11,000-line file, not from the high-CRAP dispatchers. This sample cannot characterize the whole file. The raw total for the 19 sampled mutants is 4 detected / 11 survived / 4 unviable, or 26.7% detected among 15 viable, but `mutants.py` refused to score dispatch. The timed-out, frozen, and stopped-canary runs have no score.

## Attacks

| Behavior | Test | Result |
|---|---|---|
| An expired collision quarantine with no successful report remains recoverable when a sibling has a successful report | `a_successful_collision_copy_does_not_authorize_deleting_an_unsalvaged_sibling` | RED: the unsalvaged copy was deleted. Test retained with `#[ignore = "defect: 171-8ee1"]`. |
| A live semantic memory duplicate remains visible behind more archived vectors than the fetch limit | `an_archive_larger_than_the_fetch_window_does_not_hide_a_live_duplicate` | RED: `find_duplicate` returned `None`. Test retained with `#[ignore = "defect: 172-e3c6"]`. |
| A symlink in a targeted collection update points to a file outside the project | `targeted_index_rejects_a_symlink_to_a_file_outside_the_root` | PASS: the error named the root escape and no document was indexed. |

These tests exercise a real SQLite store and the production indexing entry point. They do not modify production code. The two defect tests were run without `#[ignore]` before receiving their story IDs.

## CONVERGE gate table

Review adaptation: T2 (deletion and memory admission), base `4b825dc642cb6eb9a060e54bf8d69288fbee4904` (empty tree). The skill's one-iteration table is shown below; no production fix was in scope, so the two RED defects and open mutation survivors prevent convergence.

| It. | Attacks (new RED) | CRAP hotspots (explained / fixed) | Mutation: detected / survived / unviable, score | Survivors: killed / equivalent / low-value | New tests |
|---|---|---|---|---|---|
| 1 | 2 RED defects (171-8ee1, 172-e3c6); 1 symlink boundary held | 48 functions >30, 18 >100; dispatcher complexity explained, socket/mutation/condense paths remain open | Five-file sample: raw 4 / 11 / 4, 26.7% over 15 viable; dispatch normalizer marked INVALID | 0 / 0 / 0; 11 meaningful sample survivors open | 3 attack tests committed, 2 ignored with defect IDs |

## Execution impact

Three recent `fix` commits with added tests were replayed at their own commits with `base=<commit>^`. The test baseline is one targeted warm `cargo nextest run`. Gate times include targeted LCOV, `crap.py --base <parent> --all --json`, `cargo mutants --in-diff` with the changed test target, and `mutants.py`; attack tests were optional for this replay. A full-suite cargo-mutants baseline for the small commit also failed under socket-test startup contention (story 173-3a46); its 177.96 s is an incident cost outside the scoped gate below. Cold non-instrumented worktree warmups took 214.39, 574.88, and 559.03 s respectively and are likewise separate.

| Commit / size | Warm RED/GREEN test | Coverage | CRAP | Mutation | Analysis | Mutants: detected / survived / unviable | Gate / baseline | Gate / assumed story |
|---|---:|---:|---:|---:|---:|---|---:|---:|
| `6affd05` small, 70 changed lines | 4.92 s | 232.79 s | 0.58 s | 139.38 s | 0.11 s | 0 / 0 / 1 | 75.8× | 10.4% of 1 h |
| `cad9a2e` medium, 83 lines | 1.96 s | 793.67 s | 0.62 s | 392.59 s | 0.10 s | 2 / 1 / 1 | 605.6× | 16.5% of 2 h |
| `9de01f1` large, 290 lines | 3.06 s | 457.66 s | 1.33 s | 3,013.64 s | 0.18 s | 12 / 6 / 1 | 1,134.9× | 24.1% of 4 h |

The author and committer timestamps coincide for these commits; Git cannot reveal when authoring began. The 1/2/4-hour story durations are explicit planning assumptions, not measured spans. The medium survivor changes the near-duplicate fetch factor from multiplication to addition, leaving a live duplicate beyond the reduced window unproven. The large survivors cover `resolve_single_root`, the exact display cap in `format_coverage_list`, and `McpServer::search` returning a default result or choosing fan-out for one versus multiple roots. These gates did **not** converge; open meaningful survivors remain. The small commit had no viable mutant, so `mutants.py`'s reported score of 1.0 is not evidence of test strength.

The same warm reference command in the review worktree was `cargo llvm-cov nextest --lcov ... -E 'test(/targeted_index_rejects_a_symlink_to_a_file_outside_the_root/)'`. Without this trial's mutation process it took 10.38, 10.08, and 9.93 s (median 10.08 s). During the medium commit's mutation run it took 12.03, 31.76, and 27.99 s (median 27.99 s), a **177.7% median slowdown**. This is an observational comparison: other agents were building simultaneously, and the one-minute load changed between samples. “Idle” here means only that this review had no mutation running; it was **not an idle machine**. Boss reported background load about 28–33 on 14 cores. The no-mutation sample start loads were 43.53, 44.25, and 33.30 (end snapshots were not captured); during-mutation start→end loads were 22.08→27.32, 24.65→46.21, and 44.73→42.30. A separate low-load reference sample below 14 was not obtained: when that load was observed during the later no-mutation wait, `BUILD_FREEZE` had not granted this agent a build slot.

`sysctl vm.loadavg` was sampled every 30 s during mutation: small scoped gate 5 samples, one-minute range 20.44–38.87; medium gate 14 samples, 21.17–49.49; large gate 101 samples, 6.23–110.49. Start/end one-minute loads for the medium coverage, CRAP, and mutation steps were 33.41→41.23, 30.34→30.34, and 23.02→49.49; for the large steps they were 64.26→51.68, 65.94→65.94, and 44.35→33.49. The small-commit timings and the original review setup/coverage preceded the start/end snapshot requirement; their endpoints are unavailable and are not reconstructed.

**Recommended tiers and budgets:** low risk is at most 50 changed production lines, no persistence/socket/hook boundary, and no changed function above CRAP 30: at most 2 mutants or 5 min. Medium is 51–200 lines or a changed CRAP hotspot: at most 5 mutants or 15 min. High is more than 200 lines **or any** store write/migration, deletion, memory admission, index identity, daemon socket, hook, or externally visible API boundary: target the affected behavior with attack tests and at most 20 mutants or 45 min. In each tier report the enumerated and untested remainder; a survivor is not waived by budget exhaustion. The smaller of the count and wall budget stops mutation. Serialize LCOV and mutation gates with a machine-wide lock when agents finish together, while permitting lightweight targeted RED/GREEN runs outside the lock. The measured median slowdown, load above 100, and repeated package rebuilds support serialization; the exact share caused by mdkb cannot be isolated from the other builds.

## Recommendations

1. Fix story 171-8ee1 before the next retention sweep can remove an unsalvaged collision copy; key salvage authorization to the exact copy, not its timestamp.
2. Fix story 172-e3c6 by making active status part of candidate retrieval or by continuing until the live-neighbour requirement is met.
3. Select mutation targets by the high-risk functions and their unchanged consumers, not by a shallow file shard. Include `http-server` when testing `McpServer::hook_runtime`; route `is_targeted` and output-format mutants through the consumers that call them.
4. Add a test for the retention days in the public status payload and for single versus multiple root fan-out through `McpServer::search`; mutation found these gaps despite green targeted tests.
5. Add behavior tests for the high-CRAP daemon socket, mutation routing, and memory condense paths. Use consequence alongside CRAP: the two measured defects came from files with no CRAP hotspot. Apply the tier budgets and machine-wide gate lock in “Execution impact.”

## Skill feedback (adversarial-tdd)

`ensure.sh --dry-run` cost 0.22 s (36.1 MB RSS), `ensure.sh` 0.15 s (36.2 MB), and `detect.sh --base <empty>` 0.25 s (9.8 MB). Review LCOV cost 1,005.23 s (2.30 GB): 13m 18s build, 156.53 s tests, remaining report work. Review `crap.py` cost 3.29 s (110.1 MB). The three attack runs cost 260.41 s, 112.83 s, and 111.39 s. Mutation cost 900.06 s interrupted baseline, 369.10 s heal retry, 837.53 s memory, 33.72 s indexing interrupted by freeze, 360.79 s indexing retry, 357.86 s server, 869.44 s dispatch, and 410.59 s stopped canary filter; `mutants.py` takes about 0.1–0.4 s per result set. Historical commit gates are split by step in “Execution impact.”

The empty-tree base is worth adding as an explicit **review mode**, with a declared mutation sample and untested remainder. `crap.py` scores missing LCOV records as 0%; it should distinguish excluded/not compiled code from executed-zero code. `--all` on a small diff also ranks every function in the changed file, not only touched functions, which needs clearer labeling in a review.

The survivor normalizer gives a misleading perfect score when nothing viable ran. Exact command: `python3 ~/.claude/skills/adversarial-tdd/scripts/mutants.py --engine cargo-mutants --results .../small-fix-mutants-scoped/mutants.out --json`; output: `{"detected":0,"survived":0,"unviable":1,"score":1.0}`. A denominator of zero should be “not scored.” A full-suite cargo-mutants baseline also failed two daemon socket tests at their 5 s startup deadline under load; story 173-3a46 records the observed failure and passing targeted rerun. The skill should show how to select impacted nextest targets for a diff while recording the excluded consumers.

`cargo mutants --in-place --file src/store/heal.rs --shard 1/100 --test-tool=nextest --timeout 300 -- --lib -E 'test(/store::heal::tests/)'` spent 900.06 s in its unmutated `onig_sys` build and ended `ERROR interrupted; ERROR scenario execution internal error err=interrupted phase=Build` at the measured timeout. `mbx explain --last` reported `no cache misses were recorded`; this was not diagnosed as a cold-cache miss. A subsequent targeted build completed in 319.37 s and the warm mutation in 369.10 s. Cargo-mutants' log shows it invokes `cargo nextest run --no-run --verbose --package=mdkb@3.10.0` even when the execution filter is `--lib` or `--test cross_repo_search`; build cost dominates.

The 19:53 CPU-control order briefly prohibited `cargo mutants` in worktrees; the 19:56 revision allowed this mdkb worktree. Command `python3 ~/.claude/skills/adversarial-tdd/scripts/mutants.py --engine cargo-mutants --results .../review-dispatch-mutants/mutants.out --json` returned `{"engine":"cargo-mutants","invalid":true,"reason":"INVALID RUN (cargo-mutants): 0 of 6 mutants detected. The tests were probably not run against the mutants. Check the engine output (Stryker: 'tests per mutant'), then prove the setup with a canary: apply one obviously breaking mutation by hand and confirm a test fails."}`. Its baseline log shows `Starting 165 tests across 1 binary` and each viable mutant log records 165 passing tests. The same engine caught four memory-file mutants. This heuristic conflates inadequate test selection with absent test execution. A non-editing canary attempt using `--re 'replace ActiveFlagGuard::arm.*with None'` unexpectedly selected five variants, including four `delete field` variants that do not match the regex; it was stopped after 410.59 s and the source was restored. The skill should document how to verify the selected mutant list before a canary and how to interpret an invalid normalizer result when nextest demonstrably ran tests. The skill file changed at 14:24 and `gate-lock.sh` appeared at 14:34 while this trial was active. Its current text now requires a machine-wide lock and `cargo llvm-cov --no-clean`; earlier measurements used the commands specified in the brief and predate this guidance. This is a moving target for a measured trial, and the before/after configurations should not be presented as one controlled timing. The current scripts still do not impose a tier-specific mutation wall budget or collect start/end load automatically. The review did not hand-edit a production canary because the brief prohibits production code changes; caught memory-file variants empirically showed that the engine ran tests, but this is not the skill's literal canary procedure.
