# Multilingual embedding evaluation (2026-09-30)

Question: does a multilingual model fix recall for Italian prompts over English docs and memories, and at what cost? Measured with `examples/multilingual_eval.rs`; nothing in the production path was changed and no live store was read or written.

## Method

- Candidates supported by fastembed 4.9.1: `ParaphraseMLMiniLML12V2` (`para-ml-l12`), `MultilingualE5Small`, `MultilingualE5Base`, against the current `AllMiniLML6V2`. E5 gets the `query: ` / `passage: ` prefixes it needs. `ParaphraseMLMiniLML12V2Q` (Qdrant, quantized) does not run under the bundled ORT: `Missing Input: encoder.layer.0.attention.output.LayerNorm.weight` on the first `embed`. `BGEM3` is not in fastembed 4.9.1 (only through a user-defined ONNX, ~2 GB); not tried.
- IT->EN set: 40 Italian prompts Boss wrote (orchestrator sessions), each labelled with the English chunk(s) that answer it, against a 443-chunk corpus (orchestrator docs, `tools/README.md`, mdkb `docs/`). 38 Italian negatives (26 in-domain, 12 off-topic). Each prompt also has a hand-made English translation, run against the same corpus as the control.
- English held-out: `assets/eval/memory-recall.json`, embedding mode (vec leg only: cosine of `title content` vs query, top 5, absolute floor). The harness reproduces the recorded baseline for MiniLM exactly (0.583 at 0.40, curve rows 0.444/0.417/0.278 at 0.45/0.50/0.60).
- Floor = lowest 0.01 step that admits no negative of the set named. Precision = hits / (hits + negatives with any result).
- The corpus and prompt files quote private prompts and internal documents, so they are not committed; the harness takes them from a data directory (`corpus.json`, `it_set.json`, `sample.json`).
- n = 40 queries: one query is 2.5 points, a 95% interval is about +/-0.14. English pool is 12 memories, Italian pool is 443 chunks: floors are not interchangeable between them.

## Results

| | minilm-l6 (now) | para-ml-l12 | e5-small | e5-base |
|---|---|---|---|---|
| dimension | 384 | 384 | 384 | 768 |
| onnx on disk | 87 MB | 465 MB | 465 MB | 1.1 GB |
| RSS after load / peak at batch 32 | 249 MB / 4.6 GB | 1.45 GB / 4.8 GB | 1.49 GB / 4.8 GB | 2.9 GB / 4.3 GB |
| warm load | 0.09 s | 0.8 s | 0.7 s | 1.1 s |
| prompt embed p50 / p95 | 3.8 / 9.2 ms | 12.3 / 30.7 ms | 9.4 / 19.2 ms | 27.2 / 56.9 ms |
| chunk embed (batch 1) p50 / p95 | 58 / 89 ms | 173 / 318 ms | 106 / 188 ms | 315 / 489 ms |
| chunk, batch 32: wall / CPU per text | 44 / 317 ms | 87 / 648 ms | 67 / 595 ms | 214 / 1858 ms |
| IT prompt vs its EN translation, cosine median (min-max) | 0.31 (0.02-0.64) | 0.89 (0.55-0.98) | 0.94 (0.88-0.97) | 0.93 (0.83-0.97) |
| EN floor (precision 1.000) | 0.40 | 0.52 | 0.85 | 0.83 |
| EN recall@5 at that floor | 0.583 | 0.306 | 0.417 | 0.444 |
| EN recall@5, no floor | 1.000 | 0.972 | 1.000 | 1.000 |
| IT->EN recall@5, no floor | 0.400 | 0.575 | 0.475 | 0.525 |
| IT->EN recall@5 at the EN floor | 0.025 | 0.275 | 0.225 | 0.275 |
| IT negatives admitted at the EN floor (of 38) / precision | 1 / 0.50 | 3 / 0.79 | 0 / 1.00 | 5 / 0.69 |
| floor that admits no IT negative | 0.44 | 0.68 | 0.85 | 0.84 |
| IT->EN recall@5 / EN recall@5 at that floor | 0.000 / 0.472 | 0.000 / 0.028 | 0.225 / 0.417 | 0.225 / 0.250 |
| control: EN translation of the prompts, recall@5 no floor / at its clean floor | 0.725 / 0.150 | 0.625 / 0.075 | 0.625 / 0.075 | 0.650 / 0.100 |

Timings ran while other agents used the machine; two full runs agree within noise (prompt p50 3.8-4.4, 9.2-12.3, 9.0-9.4, 27-29 ms).

### Reindex cost (documents + memory, estimated from the batch-32 sample)

Counts from copies of the four stores: texts = chunks + single-chunk documents + memory entries (orchestrator 241, tuicommander 14,848, ego 3,506, mdkb 3,070; 21,665 in total). The code-symbol vector files (`vectors.bin`, 89,835 vectors) also change dimension for nothing but the model; they are listed separately and are an estimate scaled by token count (symbol ~40 tokens vs chunk ~350), not a measurement.

| | minilm-l6 | para-ml-l12 | e5-small | e5-base |
|---|---|---|---|---|
| docs + memory, wall | 16 min | 31 min | 24 min | 77 min |
| docs + memory, CPU | 115 CPU-min | 234 | 217 | 670 |
| of which tuicommander, wall | 11 min | 21 min | 17 min | 53 min |
| code vectors, wall (estimate) | 7.6 min | 14.9 min | 11.4 min | 36.7 min |

CPU is roughly eight cores busy: ONNX runs with all threads. Any switch also needs a schema change of the vec0 tables when the dimension changes (e5-base only) and a model-name migration.

## Reading

- Multilingual models align the languages (pair cosine 0.31 -> 0.89-0.94) and lift unfloored IT recall from 0.40 to 0.475-0.575. The gain is real but capped by the corpus: the English control tops out at 0.63-0.73, because conversational prompts meet terse rule text.
- The absolute floor wastes the gain. E5 cosines are compressed (unrelated pairs sit near 0.80), so the floor that keeps English precision at 1.000 is 0.83-0.85 and English recall@5 falls from 0.583 to 0.42-0.44. para-ml-l12 spreads scores better but is weaker in English (0.306) and leaks Italian negatives at its English floor.
- At a floor that admits no negative, the best Italian recall is 0.225-0.275 (base: 0.025), on 40 queries.

## Reproduce

```bash
FASTEMBED_CACHE_DIR=<dir> cargo run --release --example multilingual_eval -- \
    <minilm-l6|para-ml-l12|e5-small|e5-base> <data-dir> assets/eval/memory-recall.json > out.json
```

The output holds the raw top-10 cosines of every query; floors and recall are computed from it offline.
