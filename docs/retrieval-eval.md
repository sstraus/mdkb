# Retrieval Eval

`mdkb eval` measures how well memory search finds the right entry. It is the yardstick every retrieval change is measured against: a change that lowers a number here is a regression until proven otherwise.

## What it runs

Each query goes through the production memory search (`store::memory::search_entries_hybrid_fts`: the FTS5 leg, the sqlite-vec leg, RRF fusion, the access-recency signal and the confidence re-rank) with the production `[search.memory]` weights, on a real store created with the same `Context::init` that `mdkb init` uses. The store lives in a scratch directory and is seeded from a fixture, so the numbers do not depend on the repository you run the command in.

Three modes exercise the same function with different legs fed:

| Mode | BM25 leg | Vector leg | Production case |
|---|---|---|---|
| `bm25` | query | none | cold model, or embeddings not yet backfilled |
| `embedding` | starved (a term no memory holds) | query embedding | isolates what the vector pass contributes |
| `hybrid` | query | query embedding | warm model |

Recall queries are OR-expanded with `store::search::build_recall_query`, which after story 084 is the expression **every** memory surface builds: the CLI `search --scope memory`, the MCP `search` tool with `scope: memory`, and the `UserPromptSubmit` hook. One number therefore covers all three. `OR` is only safe because admission is decided afterwards and absolutely — the expression generates candidates, it does not decide relevance.

## The fixture

`assets/eval/memory-recall.json` holds 12 memories, 36 recall queries (three per memory) and 40 labelled negatives. The authoring rule is stated at the top of the file: a query is written from memory of the topic, never from the document. Each memory gets a question a user would type, a paraphrase in different vocabulary, and a half-remembered fragment. A query must not share four consecutive words with the title or content of a memory it expects. The test `fixture::tests::held_out_queries_share_no_4gram_with_their_target` enforces the rule.

When a mode scores everything, make the queries harder. Never make the metric looser.

### The negatives, and why precision is reported

A negative is an in-domain query no memory answers: it carries the corpus vocabulary but not its answers, so the correct retrieval for one is the empty set and anything it returns is a false positive. `_negatives_rule` in the fixture states the rule and `fixture::tests::negatives_are_in_domain_and_answered_by_no_memory` enforces both halves — every negative shares a content word with some memory, and none shares a four-word run with any.

They exist because recall alone cannot judge an absolute relevance floor. Remove the floor and recall goes to 1.000, which reads as a pure win; the negatives are what price it. `precision = hits / (hits + false positives)`, reported as `n/a` when the fixture labels no negatives or when nothing was retrieved at all — in neither case is there something to be precise about.

## Running it

```bash
# All three modes; embedding and hybrid are skipped, with the reason printed,
# when the ONNX model is not cached.
mdkb eval recall
mdkb eval judge

# One mode
mdkb eval recall --mode bm25

# Fetch the model (~90 MB, into the fastembed cache) when it is not cached
mdkb eval recall --download

# Exit 1 when any mode that ran scores below a floor (what CI does). Both
# floors are needed: lowering the cosine gate RAISES recall, so a recall floor
# on its own cannot catch its removal.
mdkb eval recall --mode hybrid --min-recall 0.58 --min-precision 1.0

# Machine-readable: one object per mode, with `report` or `skipped`
mdkb eval recall --format json
```

The model lookup follows fastembed: `HF_HOME`, then `FASTEMBED_CACHE_DIR`, then `~/.cache/fastembed`.

## Baseline

Recorded 2026-09-16 on the fixture above, k = 5, AllMiniLML6V2, production `[search.memory]` defaults (`min_recall_cosine = 0.40`).

| Mode | recall@5 | MRR | precision | Judge accuracy (n=3) | Misses |
|---|---|---|---|---|---|
| bm25 | 0.028 | 0.028 | 1.000 | 0.667 | 35 of 36 |
| embedding | 0.583 | 0.583 | 1.000 | 1.000 | 15 of 36 |
| hybrid | 0.583 | 0.583 | 1.000 | 1.000 | 15 of 36 |

**BM25 alone retrieves one entry of 36, and nothing for any of the 40 negatives.** The floor admits on a vec0 distance within the cosine bound or on a strong lexical match, and a BM25-only run supplies no embedding — so the distance arm is unavailable and strong lexical (an identifier, a three-word phrase, or two rare shared terms) is the only way in. Every query and every negative *is* in the BM25 result set, because the expression is OR-expanded and the negatives are in-domain by construction; this row is therefore the direct measurement of the story's constraint that membership in that set is not evidence. The single hit, `proof key for code exchange in the authorization grant`, shares two rare terms with the oauth entry. It used to score 0.167 by ranking whatever BM25 returned. A cold model means memory recall is nearly silent, not wrong.

MRR equals recall@5 in both model modes: every surviving hit is at rank 1. The floor removes the weak candidates that used to sit above it.

Read the numbers with the corpus size in mind: 12 memories and k = 5 means the top-5 covers 42% of the corpus.

### Where 0.40 comes from

`config::MIN_RECALL_COSINE_DEFAULT` is read off this curve, printed by `fixture::tests::print_the_precision_recall_curve_over_tau` (`#[ignore]`, needs the model) over the 36 queries and the 40 negatives in hybrid mode:

| tau | recall@5 | precision | hits | false positives |
|---|---|---|---|---|
| 0.00 | 1.000 | 0.474 | 36 | 40 |
| 0.15 | 1.000 | 0.486 | 36 | 38 |
| 0.20 | 0.972 | 0.507 | 35 | 34 |
| 0.25 | 0.917 | 0.589 | 33 | 23 |
| 0.30 | 0.861 | 0.705 | 31 | 13 |
| 0.35 | 0.722 | 0.839 | 26 | 5 |
| **0.40** | **0.583** | **1.000** | **21** | **0** |
| 0.45 | 0.444 | 1.000 | 16 | 0 |
| **0.50** | **0.417** | **1.000** | **15** | **0** |
| 0.55 | 0.306 | 1.000 | 11 | 0 |
| 0.60 | 0.278 | 1.000 | 10 | 0 |
| 0.90 | 0.028 | 1.000 | 1 | 0 |

The rule is the lowest floor that admits no labelled negative, which is 0.40. F1 would peak flat across 0.30-0.35 instead, and 0.35 is the better trade if a miss and a false positive cost the same. On this path they do not: recall is injected into a prompt nobody asked to enrich, so a wrong entry is charged on every turn of the conversation while a missing one costs one explicit search. The test asserts the rule, not the number, so changing the fixture reopens the choice rather than silently invalidating it.

The price is 15 of 36 held-out queries no longer retrieving their memory, several of them well-formed questions (`which entry should a bounded cache drop when it is full`). Recovering them needs a floor below 0.40, which this curve prices at 5 false positives per 5 recovered hits (tau 0.35) — a one-for-one trade, and still an open question rather than a settled one. It is not what the second floor below does.

### Where 0.50 comes from

`config::RECALL_AUTO_MIN_COSINE_DEFAULT` is the floor for a prompt that carries **no** sigil — see the two-floor table in the README. Precision cannot choose it: every floor from 0.40 up admits none of the 40 negatives, so they all score 1.000 and the curve has nothing left to say about correctness above 0.40.

The rule is the recall curve instead — the **plateau**, the floor whose step costs less recall@5 than the step before it and the step after it:

| step | recall@5 lost |
|---|---|
| 0.40 → 0.45 | 0.139 |
| **0.45 → 0.50** | **0.027** |
| 0.50 → 0.55 | 0.111 |

0.50 is the cheapest extra margin the curve offers: it buys a stricter gate for a prompt nobody asked to enrich at roughly a fifth of what either neighbouring step costs. `print_the_precision_recall_curve_over_tau` asserts that property, not the number, so a fixture change reopens this choice the same way it reopens 0.40.

This is a *calibration* rather than a *decision*: it says which floor is cheapest, not whether always-on recall is worth having. That is what `hooks.user_prompt_submit_shadow` is for — the fixture cannot rank floors above 0.40, so the sigil default stays `true` until a week of shadow rows says otherwise.

Floors, enforced by tests and by CI:

| Mode | Floor | Where |
|---|---|---|
| bm25 | exactly 1 hit of 36 and zero false positives | `fixture::tests::committed_fixture_bm25_baseline_holds` (always runs). No CI recall floor: the interesting half is the precision, and the unit test pins both exactly |
| embedding | recall@5 >= 0.58 and precision == 1.000 | `committed_fixture_embedding_baseline_holds` (`#[ignore]`, needs the model); CI `--min-recall 0.58 --min-precision 1.0` |
| hybrid | recall@5 >= 0.58 and precision == 1.000 | `committed_fixture_hybrid_baseline_holds` (`#[ignore]`, needs the model); CI `--min-recall 0.58 --min-precision 1.0` |

When you change the fixture or retrieval on purpose, re-run all three modes and the tau curve, then update this table and the floors in the same commit.

## CI

The `Test` job in `.github/workflows/ci.yml` restores the fastembed cache with `actions/cache` keyed on the model name, warms it on a miss (best effort, network permitting), then runs the eval. A cache miss without network still passes: the embedding and hybrid modes print `skipped: ONNX model not cached at ...`, and the bm25 mode carries no floor, so the `Test` job's `committed_fixture_bm25_baseline_holds` is what guards that mode.
