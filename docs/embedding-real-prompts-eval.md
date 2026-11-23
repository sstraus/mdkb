# Granite versus MiniLM on real prompts (2026-11-22)

Question: on the prompts a developer actually types, does `granite107m` inject
better memory than `minilm`, and is the default automatic floor (`0.782`) the
right one?

## Answer

Granite ranks clearly better. The default floor is the best of the floors
tested, but its own error rate is not known precisely: on the first set of 80
prompts it injected 6 relevant memories of 10, on a held-out set of 100 prompts
1 relevant of 9 (3 wrong, 5 marginal). With 9-10 injections per set, one label
moves the rate by 10 points. MiniLM was measured on the first set only.

First set (80 prompts), default floors per model:

| Automatic recall | Memories injected | Relevant | Marginal | Wrong | Answerable prompts served |
|---|---:|---:|---:|---:|---:|
| MiniLM, floor `0.50` | 15 | 2 (13%) | 8 | 5 (33%) | 2 of 18 |
| Granite, floor `0.782` (default) | 10 | 6 (60%) | 3 | 1 (10%) | 4 of 18 |
| Granite, floor `0.74` | 48 | 11 (23%) | 17 | 20 (42%) | 8 of 18 |
| Granite, floor `0.70` | 136 | 18 (13%) | 44 | 74 (54%) | 12 of 18 |

Ranking with no floor, over the 18 prompts that have at least one relevant
memory in the store:

| | MiniLM | Granite |
|---|---:|---:|
| Relevant memory at rank 1 | 5 | 10 |
| Relevant memory in the top 5 | 12 | 18 |
| Relevant pairs found in the top 5 (of 35) | 19 | 31 |

Injecting only the top memory does not change the trade-off: at `0.782` it gives
3 relevant and 0 wrong; at `0.74`, 7 relevant and 7 wrong.

Held-out set (100 prompts not in the first set, rules fixed before it was
judged; 27 answerable), Granite only:

| Automatic floor | Served | Injected | Relevant | Marginal | Wrong |
|---|---:|---:|---:|---:|---:|
| `0.782` (default) | 1 of 27 | 9 | 1 | 5 | 3 (33%) |
| `0.77` | 3 of 27 | 19 | 3 | 7 | 9 (47%) |
| `0.76` | 5 of 27 | 30 | 5 | 13 | 12 (40%) |
| `0.74` | 9 of 27 | 73 | 9 | 22 | 42 (58%) |
| `0.70` | 15 of 27 | 179 | 15 | 64 | 100 (56%) |

`0.76` looked free on the first set (1 wrong of 18) and is not on the held-out
set. Three admission rules were also tried on the held-out set and none served
more prompts with at most 15% wrong: cosine plus a lexical condition (3-4
served, 25-31% wrong), relative gates over the candidate pool (1-3 served,
33-39% wrong), and the previous turn added to the query (at most 2 of 36
served, 38% wrong). Cosine separates relevant from wrong pairs with an area
under the curve of 0.76 on the held-out set: the median relevant pair scores
0.739 and the 90th percentile of wrong pairs 0.748.

## Reading

- **The encoder is not the limit; the floor is.** Granite already ranks a
  relevant memory near the top for every answerable prompt. A single cosine floor
  cannot separate relevant from unrelated memories, so lowering it buys more
  wrong entries than relevant ones. This matches the earlier finding in
  [recall-options-eval.md](recall-options-eval.md).
- **A cross-encoder reranker is not the way out.** mdkb shipped one
  (jina-reranker-v2, int8) on top of MiniLM and removed it. In production it
  injected memory for about 10% of Italian prompts, timed out on others at its
  700 ms cap, and kept about 1.4 GB more resident. Granite without a reranker
  matches that pipeline's ranking quality (nDCG@10 within 0.01) at a fraction of
  the latency and memory, and admits far fewer unanswerable queries.
- **Most interactive prompts have nothing to recall.** Only 18 of 80 prompts had
  any relevant memory in the store. The rest are short turns such as "ok, go
  ahead". For those, the correct result is an empty injection, and both models
  mostly return one.
- **An empty-result rate is not a quality metric.** Developer telemetry counts
  how often recall admits something. A workload dominated by long agent prompts
  admits memory on almost every turn; a workload of short interactive prompts
  admits it on few. Compare encoders on the same prompts, as here, before reading
  a change in that rate as a regression.

## Method

- Store: a consistent online backup of one active repository store, 3,997 memory
  entries, mostly English with some Italian. Two copies: one kept as Granite, one
  rebuilt with `mdkb embed --reembed` under `minilm` (32,808 vectors). Both
  copies report a current identity, no pending model, and one vector per memory.
- Prompts: 80 prompts typed by the developer in that repository between
  2026-11-04 and 2026-11-22, sampled at random with a fixed seed from the session
  transcripts. Mostly Italian; some English; some agent notifications that reach
  the hook as prompts. Tool results and command expansions are excluded.
- Pipeline: every prompt goes through `mdkb hook user-prompt-submit`
  in-process (`MDKB_NO_DAEMON=1`), with the hook deadline disabled so that model
  loading does not cut the run short, and telemetry off. A unique session ID per
  call keeps one prompt from suppressing another's results. The floor sweep
  changes only `hooks.recall_auto_min_cosine` on the Granite copy.
- Ranking: `mdkb search --scope memory --limit 5` on each copy, with the explicit
  memory floor set to `0.0`.
- Judging: 675 distinct prompt-memory pairs (every injected memory and every
  top-5 memory, both models, all floors) are shuffled and labelled by an LLM judge
  that does not know which model produced a pair: 2 relevant, 1 marginal, 0 not
  relevant. A prompt with no recoverable topic makes every memory a 0.

## Limits

- One judge model and no human adjudication.
- 18 and 27 answerable prompts, 9-10 injections at the default floor: the ranking
  result and the floor ordering hold on both sets; the default's own error rate
  does not, and needs a sample of several hundred prompts.
- The held-out set has more machine-generated prompts (agent notifications,
  coordinator messages): 9 of 100 against 5 of 80.
- Recall is measured against the pool of what either model ranked in its top 5
  or injected. A relevant memory that neither model surfaced is not counted.
- One store and one developer's prompts. A store in another language mix or a
  workload of agent prompts can behave differently.
- The prompts and memories stay private; this document reports only the counts.
