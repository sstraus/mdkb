# Recall options for non-English prompts (2026-09-30)

Question: which option lets automatic injection recall English documents for Italian prompts while still injecting nothing when nothing is relevant? Follows `multilingual-embedding-eval.md`; same data directory, same 40 IT->EN prompts, 38 IT negatives, English held-out fixture. Raw scores come from `examples/recall_options_eval.rs`; every gate is computed offline by `scripts/eval-recall-options/*.py`. No production code changed, no live store read.

**Status: incomplete.** The run was stopped by the coordinator (CPU load). Not measured: `bge-reranker-v2-m3` (download ok, run killed twice), the OpenRouter translation (waits for Boss approval), `bge-m3` (not in fastembed 4.9.1). The larger IT set was not built (see "Sample size").

## Method

- Gate protocol, same as the previous table: threshold = lowest step that admits no negative. `B` fits it on the 38 IT negatives (in-sample), `A` on the 40 EN negatives (fit on EN, test on IT). Cosine scores use a 0.01 grid (comparable with the old table); reranker, BM25 and RRF scores are not gridded.
- IT recall = hits@5 among the 40 IT positives that pass the gate. EN recall = same gate on the 36 EN positives. Intervals are Wilson 95%.
- Rerankers score the top-5 or top-10 candidates of the dense retriever (MiniLM or e5-small) on 232 real production chunks; the gate is the top reranker score.
- Threads: `fastembed` 4.9.1 sets ONNX intra-op threads to `available_parallelism()` with no option, so there is no thread cap. A `SIGSTOP`/`SIGCONT` throttle (`cpucap.py`, target 4 cores) measured 380% in one 30 s window, but the coordinator saw 585% over 22 min: it does not cap reliably (cause not diagnosed). Reranker latencies below ran **uncapped at Mac load 35-50**; treat them as upper bounds with noise (bge-base top-5 p95 18.8 s is one such outlier).

## Results (IT recall@5 at a gate that admits no IT negative)

| Option | IT recall, gate fit on IT neg (95% CI) | EN recall at that gate | IT recall, gate fit on EN neg / IT negatives admitted | Added latency p50 | RAM | Disk | Reindex | Cost/prompt |
|---|---|---|---|---|---|---|---|---|
| MiniLM, absolute cosine (now) | 0.000 (0.00-0.09) | 0.472 | 0.025 / 1 | 3.8 ms | 0.25 GB | 87 MB | - | 0 |
| e5-small, absolute cosine | 0.250 (0.14-0.40) | 0.444 | 0.250 / 0 | 9.4 ms | 1.4 GB | 465 MB | 24 min | 0 |
| e5-large, absolute cosine | 0.250 (0.14-0.40) | 0.417 (1 EN neg admitted) | 0.075 / 0 | not measured | 1.9 GB | 2.1 GB | not measured | 0 |
| 1. Relative gates (margin top1-top2, top1-mean(2..k), z-score) | <= 0.10, most 0.00-0.05 | 0.00-0.61 | up to 0.45 with 23-36 admitted | ~0 | - | - | - | 0 |
| 2a. jina-reranker-v2-multilingual on e5-small top-10 | **0.525** (0.37-0.67) | 0.333 | 0.650 / 16 | 3.0 s (p95 4.3 s) | 2.9 GB | 1.1 GB | none | 0 |
| 2a'. same, top-5 | 0.350 (0.22-0.50) | 0.333 | 0.475 / 14 | 1.8 s (p95 2.7 s) | 2.9 GB | 1.1 GB | none | 0 |
| 2a''. jina on MiniLM top-5 (no reindex) | 0.350 (0.22-0.50) | 0.417 | 0.400 / 6 | 1.8 s + 4 ms | 3.1 GB | 1.2 GB | none | 0 |
| 2b. bge-reranker-base on e5-small top-10 | 0.300 (0.18-0.45) | 0.444 | 0.350 / 5 | 2.3 s | 2.8 GB | 1.1 GB | none | 0 |
| 2c. bge-reranker-v2-m3 | not measured | - | - | - | - | 2.1 GB | none | 0 |
| 3. Hybrid RRF dense+BM25, gate on fused score | 0.100-0.125 (MiniLM / e5-small) | 0.64-0.72 but 21 of 40 EN negatives admitted | 0.000 | ~0.1 ms + dense | as dense | - | none | 0 |
| 3b. RRF order, gate on dense cosine of fused top-1 | same as plain dense | | | | | | | 0 |
| 4a. gemma4:e4b translation -> MiniLM | 0.125 (0.05-0.26) | unchanged (EN not translated) | 0.700 / 8 | 3.3 s (p95 7.4 s, loaded) | +8.8 GB | 8.8 GB | none | 0 |
| 4a'. gemma4:e4b translation -> e5-small | 0.050 (0.01-0.17) | unchanged | 0.325 / 7 | 3.3 s + 9 ms | +8.8 GB | 8.8 GB | none | 0 |
| 4b. LLM translation (OpenRouter) | pending Boss approval | | | | | | | not measured |
| 5. Lexical only (BM25 top score) | 0.200 (0.10-0.35) | 0.000 | 0.500 / 12 | 0.1 ms | ~0 | 0 | none | 0 |
| 5'. gemma translation + BM25 | 0.250 (0.14-0.40) | 0.000 | 0.650 / 31 | 3.3 s | +8.8 GB | - | none | 0 |
| 6. e5-small + jina top-10, per-language thresholds | 0.525 IT; EN 0.500 at its own EN-fit gate (MiniLM alone: 0.639) | | | 3.0 s | 4.3 GB | 1.6 GB | 24 min | 0 |

Latency is the prompt path only. "Reindex" is the e5-small swap from the previous report (docs + memory 24 min wall, 217 CPU-min); e5-large was not timed (e5-base took 77 min).

## Findings

1. **The gate, not the language, is the bottleneck.** The EN translation control of the previous report reaches only 0.075-0.15 recall at a clean gate. 26 of the 38 negatives are in-domain, so they sit near real hits in every embedding space.
2. **Relative gating fails.** Margin and z-score gates keep IT recall at or below 0.10 when they admit no negative. Absolute cosine is not the problem; single-number gates on a dense ranking are.
3. **Hybrid RRF is not a gate.** The fused score is rank-only. 21 of 40 EN negatives reach the maximum 2/61 because the 12-memory pool makes both legs agree on rank 1. No threshold above the IT negatives' maximum (0.03252) keeps EN negatives out. Gating on the dense cosine of the fused top-1 equals plain dense. (An earlier grid of 0.0005 rounded the threshold to 0.033, above the maximum 0.03279, and printed 0.000 everywhere; fixed, the table above uses ungridded RRF thresholds.)
4. **A multilingual cross-encoder is the only option that moves the result.** jina-v2 on e5-small top-10: 0.525 against 0.250. Paired sign test on the 40 IT positives: 12 gained, 1 lost, p = 0.003 (p = 0.03 after a Bonferroni factor of 9 for the comparisons run). bge-reranker-base does not separate from e5-small (p = 0.75). jina on top-5 or on MiniLM candidates is not significant (p = 0.22-0.45).
5. **"Precision 1" is in-sample.** Fit the threshold on half of the IT negatives and test on the other half (300 random splits): held-out negative admission rate is 5-6% for every gate (plain e5-small, jina, bge-base), and at least one of the ~19 held-out negatives gets through in 50-54% of splits. A clean gate on 38 negatives bounds the false-positive rate only at 3/38 = 7.9% (95%).
6. **The reranker cost is the problem.** 1.8-3.0 s p50 per prompt at load 35-50, 2.9 GB RSS, on top of the hook's deadline (story 195). This run gives no unloaded latency; the coordinator's box measurement is needed before the option can be adopted. Top-5 is faster and loses 0.175 recall.
7. **Local translation does not help.** gemma4:e4b adds 3.3 s and 8.8 GB and lands at 0.05-0.125 with MiniLM/e5-small, below e5-small alone. Translation + BM25 reaches 0.25 but admits 31 EN-fit negatives. The LLM-translation row is pending approval.
8. **Lexical alone** (0.200) beats MiniLM (0.000) on IT because identifiers and file names survive translation, but its EN recall at that gate is 0.000: usable only behind language detection, as a fallback.

## Sample size

n = 40 gives a Wilson interval of about +/-0.14 and detects a paired recall difference of about 0.25 (what jina vs e5-small shows). For a difference of 0.15 at 80% power with 20% discordant pairs: about 70 labelled queries; for 0.10: about 155. Claiming false-positive rate under 2% with zero observed needs about 150 negatives.

`.mdkb/hook-events.jsonl` (8,623 events, 482 `user_prompt_submit`) stores no prompt text, so it cannot feed a larger set. A larger IT set needs real prompts from session transcripts plus hand labels of the answering chunks; not built here.

## Not measured, and why

- `bge-reranker-v2-m3`: model downloaded (2.1 GB); the run was killed for CPU load twice.
- `bge-m3` embeddings: not in fastembed 4.9.1; a user-defined ONNX (~2 GB) was not tried.
- OpenRouter translation: needs Boss approval (private prompts leave the machine).
- Unloaded latencies and RAM for rerankers: run on the rb box.

## Reproduce

```bash
FASTEMBED_CACHE_DIR=<dir> cargo run --release --example recall_options_eval -- embed <model> <data-dir> assets/eval/memory-recall.json > embed-<model>.json
... -- lex <data-dir> assets/eval/memory-recall.json > lex.json
... -- rerank <bge-base|jina-v2-ml|bge-v2-m3> <data-dir> assets/eval/memory-recall.json pool.json > rerank-<model>.json
python3 scripts/eval-recall-options/a3.py 0.01   # relative/RRF/lexical/translation rows
python3 scripts/eval-recall-options/a4.py jina-v2-ml bge-base  # paired test + split-half
```

The scripts read the private raw outputs from `~/Gits/.tmp/mdkb-ml/opt/` (not committed).
