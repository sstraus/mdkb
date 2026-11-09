# Daemon memory investigation (2026-10-17)

## Question and status

Why does the mdkb daemon reach about 3 GB of memory? **Partly explained, still open.** An isolated off/on/off test attributes about 1.2 GiB of RSS to enabling and loading Jina on a small store. The live daemon's remaining peak is not attributed. This report is separate from the Jina recall-efficiency report (`jina-efficiency-report-2026-10-17.md`). No live daemon was stopped and no live setting was changed for this investigation.

## Observations

| Local time (CEST) | PID / daemon start | RSS (`ps`, KiB) | Physical footprint (`vmmap`) | Peak footprint | Selected resident regions |
|---|---|---:|---:|---:|---|
| 17:37:46 | 11705 / 17:36:44 | 1,493,392 | 1.2 GB | 1.4 GB | IOAccelerator 572.9 MB; default malloc zone 632.6 MB |
| 23:05:01 | 56179 / 20:42:40 | 2,821,632 | 2.4 GB | 3.0 GB | IOAccelerator 563.2 MB; default malloc zone 2.1 GB |
| 23:07:22 | 56179 / 20:42:40 | 2,182,512 (23:07 `ps`) | 2.5 GB | 3.0 GB | IOAccelerator 247.3 MB; default malloc zone 1.4 GB; 506.7 MB swapped |
| 23:09:44 | 56179 / 20:42:40 | 1,804,864 | not measured | not measured | not measured |

The first and second rows are **different processes**. They do not measure growth of one daemon. The later RSS readings belong to the same PID and fall after the 23:05 reading. RSS, physical footprint, resident VM regions, and peak footprint answer different questions; do not add region totals to the footprint or treat the peak as current use. `vmmap` reported a large virtual address range, mostly guards and unallocated mappings; it is not physical RAM.

The scheduled 18:39 check did not run: automation run `01a14abb-891a-76cf-8aa4-fa0b625f6041` failed before starting a task with `Automation run configuration 'claude' not found`. The 23:05 reading was manual, not a one-hour follow-up. The scheduler failure was sent to the orchestrator peer separately.

## Isolated off/on/off experiment

A temporary Git repository with five small memory entries (`rust-errors`, `cargo-build`, `index-memory`, `hook-context`, `rust-testing`) and a separate `HOME`/daemon socket was created under `~/Gits/.tmp/mdkb-memory-ab-jUmS2E`. The six prompts asked, in Italian, about Rust errors, Rust tests, cargo checks, indexed notes, hook context, and targeted Rust tests. All three runs used the same installed mdkb binary, repository, cached model weights, and prompt set. Only `hooks.recall_rerank_it` changed. Runs were sequential: each isolated daemon was stopped before the next started. The live daemon and its configuration were not touched. The store was small and had no indexed documents or code, so this is a marginal model-load test, **not** a reproduction of the live 3 GB workload.

| Phase | Isolated PID | Hook evidence | RSS after prompts (KiB) | Physical footprint | Peak footprint |
|---|---:|---|---:|---:|---:|
| Jina off | 79652 | six prompts, `rerank_outcome=off` | 260,352 | 228.4 MB | 231.7 MB |
| Jina on | 91099 | first `loading`, then `ok`; five further prompts scored | 1,524,112 | 1.2 GB | 1.4 GB |
| Jina off again | 13896 | same six prompts, `rerank_outcome=off` | 257,008 | 231.3 MB | 231.3 MB |

The on-minus-first-off RSS difference is **1,263,760 KiB (1,234 MiB, 1.205 GiB)**. The reverse off run returned to about 251 MiB RSS, which rejects a simple explanation based on test order or permanent cache growth across these separate daemon processes. The model-on process stayed at about 1,523,984 KiB RSS after ten idle seconds. This does not isolate model weights from ONNX arenas, tokenizer data, or allocator retention inside the Jina-enabled process. It does show a substantial marginal cost from the Jina path on this host and workload. `vmmap` assigned about 555 MB resident to IOAccelerator and 579 MB to the default malloc zone in the on run, against about 18 MB and 164 MB in the first off run; neither category is a model ownership label.

## Code paths that can retain memory

- `src/llm/mod.rs::CACHED_SERVICE` keeps a shared MiniLM ONNX session for queries. `release_cached_service` clears the cache and requests allocator collection after selected indexing operations. This does not prove when the live daemon last released it.
- `src/llm/rerank.rs::ENGINE` keeps the Jina int8 session after the first eligible prompt. It has no idle unload. The source and the prior benchmark estimate about 0.95 GB of loaded RSS for that model. The current daemon's orchestrator hook log has a `loading` outcome followed by completed `below_gate` reranks after its 20:42 start, so the model was used in this process.
- `src/llm/mod.rs::get_or_download_service` initializes outside its global mutex and checks the cache again afterwards. Concurrent first users can initialize more than one session before only one enters the cache. This is a **code-level possibility**, not an observed cause of this peak. A separate concurrency test and an allocation trace would be needed before changing it.
- The ONNX allocator can retain arenas while a session lives (`src/llm/mod.rs` documents this). The `vmmap` malloc and IOAccelerator categories do not identify a model or distinguish live model weights from allocator capacity. The 23:05 to 23:09 RSS decline also prevents a claim of monotonic leakage from these snapshots.

## Falsifiable next measurements

1. Take timestamped `ps` and `vmmap -summary` samples for **one unchanged PID**, before and after named workloads and during idle. Record the workload, hook outcome, and whether the process restarted. Do not infer a leak from two samples with different PIDs.
2. Reproduce the **additional** live peak with a representative isolated store and named indexing/query workload, not only the five-entry model-load fixture. Measure the same PID after startup, MiniLM use, Jina use, indexing, and idle; compare with the off/on/off result above. Do not use the live store as a test fixture without an explicit scope and data review.
3. If memory still grows beyond the isolated model-loading step, trace allocation stacks or use a heap profiler during repeated indexing and concurrent first queries. Check the session-initialization race separately. Do not call that race a production root cause without a reproducer.

No live daemon restart, live config edit, model download, or cache deletion was performed here. The measured peak is real. Jina accounts for about 1.2 GiB of **marginal RSS in the small isolated workload**; the owner of the remaining live peak is still unknown.
