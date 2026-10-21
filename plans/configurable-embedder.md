---
status: closed
approved_at: "2026-10-16T18:10:49.107Z"
updated: "2026-10-16T18:11:18.832Z"
started_at: "2026-10-16T18:11:18.832Z"
---
# Plan: Configurable embedding model (opt-in multilingual e5-small)

**Created:** 2026-10-16 | **Status:** Closed — gate failed | **Effort:** L | **Branch:** feat/configurable-embedder

## Closure (2026-10-21)

Step 1 failed: paired IT p = 1.00 (e5 top-5) and p = 0.38 (e5 top-10); neither meets p < 0.05. Native Mac embed+rank+rerank p95 is 753.6 ms and 1478.7 ms for the two candidates, above 700 ms. EN recall is 0.500. See the Mac rerun 2026-10 section of the evaluation report. Stop here; Steps 2-8 are not approved for execution. Keep the two jina-only follow-ups below.

## Summary

- Boss writes prompts in Italian. The memory corpus is English. MiniLM recall on Italian prompts is 0.000 at a clean gate (`docs/recall-options-eval.md`).
- Make the embedding model a global choice: `minilm` (default, unchanged) or `e5-small` (opt-in). Use one model per process, code index included.
- Step 1 is a gate. The prior eval shows e5-small alone (0.225-0.25) below the shipped jina-on-MiniLM path (0.35). Steps 2-8 run only if Step 1 shows that an e5 configuration beats that baseline on the Mac.

## Architecture Context

- `llm::EmbeddingService` (`src/llm/embeddings.rs:175`) hard-wires `AllMiniLML6V2`. A process-global `CACHED_SERVICE` (`src/llm/mod.rs:20`) serves every repo. `get_cached_service` never downloads. `get_or_download_service` is called only from `handle_embed` and eval.
- Vector stores: `embeddings`, `vec_documents`, `document_chunks`, `vec_chunks`, `memory_embeddings` and `vec_memory` live in `index.sqlite` (`src/store/vectors.rs:33`). `prior_clusters.embedding` is in `src/store/priors.rs`. `.mdkb/vectors.bin` holds the code vectors (`src/code/semantic.rs:38`; header is magic, version and dim only).
- Both models are 384-dim. A same-dim switch is invisible today: the `model` column is write-only, and `migrate_vector_table_if_needed` checks the dimension only.
- Pending means "no row" (`embedded_document_ids`, `count_pending_embeddings`). The model is not part of the check.
- Template to copy: `embedding_signature` + `reconcile_embedding_signature` (`src/code/duplication/embed.rs:75`, `duplication/store.rs:170`), and the closed-list model key `[code.duplication] model`.
- Global config precedent: `~/.mdkb/daemon.toml` `[priors]`, merged in `config::effective_priors` (`src/daemon/config.rs:125`).
- Daemon config hot-reload swaps `RepoHandle.config` only (`src/daemon/registry.rs:663`). It never swaps the embedder.
- ADR: one global model in `daemon.toml`, read at process start — the shared daemon and cross-repo search must never compare vectors from two models — per-repo key rejected (needs N resident models or refused repos).
- ADR: model change marks the store stale; it never deletes vectors; an explicit `mdkb embed --reembed` migrates — a config typo that falls back to defaults must not destroy vectors — automatic daemon re-embed rejected.

## Research Findings

- `docs/multilingual-embedding-eval.md`:
  - e5-small cosines are compressed: unrelated pairs sit near 0.80.
  - The clean English floor is 0.85, against 0.40 for MiniLM.
  - English recall@5 at that floor is 0.417, against 0.583 for MiniLM.
  - Italian recall at the clean gate is 0.225.
- `docs/recall-options-eval.md`:
  - jina on MiniLM top-5 (shipped) and jina on e5 top-5 both reach 0.35.
  - jina on e5 top-10 reaches 0.525, but takes 0.88 s on 32 cores and 2.2 s on 8.
  - These are box numbers, not Mac numbers.
- Eval tooling exists:
  - `examples/recall_options_eval.rs` (`embed|lex|rerank`, prints RSS) and `scripts/eval-recall-options/*.py`.
  - The private data is in `~/Gits/.tmp/mdkb-ml/` (`it_set.json`, `corpus.json`, `sample.json`, `translations.json`) and is not committed.
  - The committed store is 12 memories (`assets/eval/memory-recall.json`).
- Five memory-write sites embed passage text with `embed_query`:
  - `core/memory.rs:120`
  - `store/memory.rs:1854`
  - `mcp/dispatch.rs:1750`, `:1863` and `:7323`
- `model_cached_at` looks for `snapshots/*/model.onnx`. e5 ships `onnx/model.onnx`, so a cached e5 would read as "not cached".
- fastembed 4.9.1 has `MultilingualE5Small`. Output is L2-normalized (`text_embedding/output.rs:43`), so the `cos = 1 - d²/2` identity in `store/hybrid.rs:93` holds. Re-check this for e5 in Step 2.
- MiniLM-fitted thresholds:
  - `MIN_RECALL_COSINE_DEFAULT` 0.40, also `RECALL_CANDIDATE_FLOOR`
  - `RECALL_AUTO_MIN_COSINE_DEFAULT` 0.50
  - `RECALL_DOCS_MIN_COSINE_DEFAULT` 0.57
  - `DOC_STEM_MIN_COSINE` 0.50
  - `NEAR_DUPLICATE_DISTANCE` 0.32 and `near_duplicate_similarity`
  - `SIMILAR_ENTRY_DISTANCE` 0.55
  - `PRIOR_MERGE_SIMILARITY` 0.70
  - code `threshold` 0.3
  - reranker floors -1.05 / -1.95 and `RERANK_POOL_SIZE` 5
- Live store audit (2026-10-16): mdkb `index.sqlite` holds 804 doc rows and 192 memory rows, all labelled `AllMiniLML6V2`. No legacy labels.

## Security Considerations

- New network download (e5 weights, 465 MB, HuggingFace). It is allowed only on `mdkb embed`, never on the hook or `update` paths. That is the same rule as for jina.
- The config key is a closed list. An unknown value is a load error, not a default.
- No new data exposure. Vectors stay local.

## Performance Considerations

- e5-small: about 1.5 GB RSS against 249 MB. Cold load 0.7-1.1 s against 0.09 s. Prompt embed p50 9.4 ms against 3.8 ms.
- Preload e5 in the daemon in the background, as the reranker does. Never cold-load it inside the 1000 ms hook deadline.
- Do not call `release_cached_service` after a code reindex when the model is e5. The reload costs about 1 s.
- Re-embed of 4 stores: about 24 min wall for docs and memory plus about 11 min for code (eval estimate). Run it niced (`embed_nice`), never from the daemon.
- Daemon with e5 and jina loaded: about 2.5-3.5 GB. Measure it on the Mac in Step 1 and Step 8.

## Open Questions

### Unresolved / waiting on signal
- Vocabulary-trimmed EN+IT e5 — waiting on: Step 8 RSS measurement. Revisit when daemon RSS with e5 is a real problem for Boss.
- Language vote handles IT/EN only (`recall_rerank`) — waiting on: a third-language user.

## Steps

### Step 1: Eval gate — does an e5 configuration beat jina-on-MiniLM on the Mac?
- **Test:** no code test. A measurement report appended to `docs/recall-options-eval.md` under a new section "Mac rerun 2026-10".
- **Implement:**
  - Run the existing harness on the private set for three configurations: MiniLM+jina top-5 (baseline), e5+jina top-5 and e5+jina top-10.
  - Record IT and EN recall at held-out gates, a paired sign test against the baseline, p50/p95 prompt latency and RSS.
- **Code:**
  ```sh
  D=~/Gits/.tmp/mdkb-ml; A=assets/eval/memory-recall.json
  cargo run --release --example recall_options_eval -- embed e5-small "$D" "$A" > "$D/out3/embed-e5.json"
  cargo run --release --example recall_options_eval -- embed minilm   "$D" "$A" > "$D/out3/embed-minilm.json"
  cargo run --release --example recall_options_eval -- rerank jina "$D" "$A" "$D/out3/pool-e5-top10.json" > "$D/out3/rerank-e5-10.json"
  python3 scripts/eval-recall-options/a6.py   # per-language gates, held-out splits
  ```
- **Constraint:** gate rule, decided before the run. Proceed only if an e5 configuration has:
  - IT recall that beats the baseline with paired sign test p < 0.05;
  - EN recall ≥ 0.417;
  - p95 embed+rerank ≤ 700 ms on the Mac.
  Otherwise stop. Set the plan Status to `Closed — gate failed`, and keep the jina-only follow-ups below.
- **Validation:** the report section exists and states PASS or FAIL against the rule.

### Step 2: Behaviour-preserving refactor — one model spec, three embedding roles
- **Test:** `src/llm/embeddings.rs` tests:
  - `e5_spec_reads_as_cached_when_onnx_subdir_file_exists` — catches the `snapshots/*/model.onnx` check rejecting a cached e5.
  - `memory_writes_embed_as_passage` (spec with distinct prefixes, recording fake) — catches a write site left on `embed_query`.
- **Implement:**
  - `ModelSpec` table: name, fastembed variant, dim, query prefix, passage prefix, model file.
  - `EmbeddingService::new(spec)`.
  - `embed_query` / `embed_passages` apply the prefixes.
  - Move the 5 memory-write sites to `embed_passages`.
  - Symmetric memory-vs-memory checks (dedup, similar, audit, priors) use passage on both sides.
  - Delete the duplicate `EMBEDDING_DIM` constants (`code/semantic.rs:20`, `store/vectors.rs:704`).
  - MiniLM has empty prefixes, so its behaviour is unchanged.
- **Code:**
  ```rust
  pub struct ModelSpec { pub name: &'static str, pub variant: EmbeddingModel, pub dim: usize,
                         pub query_prefix: &'static str, pub passage_prefix: &'static str,
                         pub model_file: &'static str }
  pub const MINILM: ModelSpec = ModelSpec { name: "AllMiniLML6V2", variant: EmbeddingModel::AllMiniLML6V2,
      dim: 384, query_prefix: "", passage_prefix: "", model_file: "model.onnx" };
  pub const E5_SMALL: ModelSpec = ModelSpec { name: "MultilingualE5Small", variant: EmbeddingModel::MultilingualE5Small,
      dim: 384, query_prefix: "query: ", passage_prefix: "passage: ", model_file: "onnx/model.onnx" };
  ```
- **Constraint:** a separate commit, `refactor(embed): ...`, with no behaviour change. Update the tests that pin `MODEL_NAME` (`code/duplication/embed.rs:858`, `llm/embeddings.rs:448`) to pin `MINILM.name`.
- **Validation:** `cargo nextest run -E 'test(embeddings) | test(memory) | test(semantic)'`

### Step 3: Global model selection in `daemon.toml`
- **Test:** `src/daemon/config.rs` tests:
  - `unknown_embedding_model_is_a_load_error` — catches the fallback to MiniLM on a typo.
  - `cli_and_daemon_resolve_same_model` — catches a CLI process embedding with the default while the daemon uses e5.
- **Implement:**
  - `[embedding] model = "minilm" | "e5-small"` in `~/.mdkb/daemon.toml`.
  - Resolve it once per process into a `&'static ModelSpec`, and feed it to `get_cached_service`.
  - A per-repo `[search] embedding_model` is rejected, and doctor names the global file.
  - The daemon preloads e5 in the background at start.
- **Code:**
  ```rust
  pub fn active_model() -> &'static ModelSpec {
      static SPEC: OnceLock<&'static ModelSpec> = OnceLock::new();
      SPEC.get_or_init(|| daemon_config::load_global().embedding.spec())
  }
  ```
- **Depends on:** Step 2
- **Validation:** `cargo nextest run -E 'test(daemon::config) | test(llm)'`

### Step 4: Store signature, stale state, no cross-model comparison
- **Test:**
  - `src/store/vectors.rs`: `signature_mismatch_marks_stale_and_keeps_rows` — catches a reconcile that deletes vectors.
  - `src/store/vectors.rs`: `stale_store_vector_search_returns_lexical_only` — catches a vector path comparing an e5 query with MiniLM rows.
  - `src/code/semantic.rs`: `vectors_bin_v2_header_carries_signature` — catches a same-dim switch that still opens.
- **Implement:**
  - `meta(key, value)` row `embedding_signature = "{name}:{dim}:{prefix_scheme}"` in `index.sqlite`.
  - `vectors.bin` `FORMAT_VERSION` 2 with the signature in the header.
  - On mismatch, the store state is `Stale`. Every vector read path checks it and returns lexical only: doc, chunk and memory KNN, the `vec_distance_L2` fallback, graph neighbour cosine, audit, priors and code semantic.
  - Writes leave rows pending.
  - A missing signature on an old store is treated as MiniLM, not stale.
- **Code:**
  ```rust
  pub enum VectorState { Current, Stale { stored: String, active: &'static str } }
  pub fn vector_state(conn: &Connection, spec: &ModelSpec) -> Result<VectorState>
  ```
- **Constraint:** an old store with no `meta` row and all `model = 'AllMiniLML6V2'` must read `Current` under MiniLM. Default users get no re-embed.
- **Depends on:** Step 3
- **Validation:** `cargo nextest run -E 'test(vectors) | test(semantic) | test(hybrid)'`

### Step 5: `mdkb embed --reembed` — explicit, niced, resumable migration
- **Test:** `tests/` integration `reembed_interrupted_then_resumed_reaches_current` — catches a signature written before every table is migrated.
- **Implement:**
  - Under `embed_nice`, delete the rows `WHERE model != active` per table.
  - Re-embed docs, chunks, memory and prior clusters, then run the code `--force` path.
  - Write the signature last.
  - Fetch the e5 weights here only.
  - The daemon run of `embed` uses the same path off-lock.
- **Code:**
  ```rust
  #[arg(long)] reembed: bool, // CLI: mdkb embed --reembed
  if args.reembed { ops::reembed_all(&ctx, active_model())?; }
  ```
- **Depends on:** Step 4
- **Validation:** `cargo nextest run -E 'test(reembed) | test(cli_smoke)'`

### Step 6: Per-model thresholds
- **Test:** `src/config.rs`: `e5_defaults_apply_when_repo_config_froze_minilm_values` — catches a frozen 0.50 floor admitting everything under e5.
- **Implement:**
  - `ModelSpec` carries a `Thresholds` struct with every MiniLM-fitted value listed above.
  - The e5 values come from Step 1. Values not covered by Step 1 (dedup, similar, prior merge, code) are fitted on the committed fixture plus a sample from Boss's stores, and the fit is documented in the doc comment.
  - Per-repo floor keys apply only when the active model is MiniLM. Under e5, doctor warns that they are ignored, and overrides go in `daemon.toml [embedding.thresholds]`.
  - `RERANK_POOL_SIZE` becomes a per-model value.
- **Code:**
  ```rust
  pub struct Thresholds { pub min_recall_cosine: f32, pub recall_auto_min_cosine: f32,
      pub recall_docs_min_cosine: f32, pub doc_stem_min_cosine: f32, pub near_duplicate_distance: f32,
      pub similar_entry_distance: f32, pub prior_merge_similarity: f32, pub code_threshold: f32,
      pub rerank_pool: usize, pub rerank_min_it: f32, pub rerank_min_en: f32 }
  ```
- **Constraint:** the MiniLM values are identical to today's constants. `eval/fixture.rs:416` keeps passing unchanged.
- **Depends on:** Step 2, Step 1
- **Validation:** `cargo nextest run -E 'test(config) | test(fixture) | test(recall)'`

### Step 7: Make the model and its state visible
- **Test:** `tests/cli_smoke.rs`: `stats_json_reports_embedding_model_and_state` — catches a stale store that reports nothing.
- **Implement:**
  - Add `embedding_model` and `embedding_state` to `stats --format json`. The fields are additive.
  - Doctor warns on `Stale`, naming `mdkb embed --reembed`.
  - Add `embedding_model` to the `user_prompt_submit` hook-events rows.
  - Check the TUICommander dashboard plugin parses the new stats JSON. Add a model line there, bump `manifest.json`, and commit in that repo.
- **Code:**
  ```rust
  pub embedding_model: &'static str,   // stats_report.rs, next to pending_embeddings
  pub embedding_state: &'static str,   // "current" | "stale"
  ```
- **Depends on:** Step 4
- **Validation:** `cargo nextest run -E 'test(cli_smoke) | test(stats)'`

### Step 8: Batch verification and live trial
- **Test:** no new test. One batch run.
- **Implement:**
  - Run clippy once on the touched crates, plus the full `cargo nextest run --lib`.
  - Build the release binary and switch Boss's machine to e5. Run `mdkb embed --reembed` on the four stores.
  - Measure daemon RSS and hook p95 from `hook-events.jsonl`, and the share of Italian prompts that injected recall, before and after.
  - Append the result to the Step 1 report section.
- **Code:**
  ```sh
  cargo clippy --all-targets -- -D warnings && cargo nextest run --lib
  # hook p95 + share injected (outcome=fired) over the last 100 prompts
  grep user_prompt_submit .mdkb/hook-events.jsonl | tail -100 | python3 -c 'import sys,json
  r=[json.loads(l) for l in sys.stdin]; e=sorted(x["elapsed_ms"] for x in r)
  print("p95",e[int(.95*len(e))-1],"fired",sum(x["outcome"]=="fired" for x in r),"/",len(r))'
  ```
- **Depends on:** Steps 2-7
- **Validation:** clippy is clean, the suite is green, and the live numbers are recorded.

## Acceptance Criteria

- [ ] Step 1 report states PASS or FAIL against the pre-registered gate. On FAIL the plan closes there.
- [ ] Default (MiniLM) users: no re-embed, no changed thresholds, no changed recall output (fixture tests unchanged).
- [ ] `[embedding] model = "e5-small"` + `mdkb embed --reembed` yields `embedding_state = current`, with every vector store tagged `MultilingualE5Small`.
- [ ] While stale, no query vector is compared with a vector from another model (Step 4 tests).
- [ ] A typo in `[embedding] model` is a load error and never triggers a migration.
- [ ] The hook never cold-loads e5 inside its deadline.
- [ ] The TUICommander dashboard is checked against the new stats JSON, and the plugin is updated and pushed if it changed.
- [ ] All tests passing.

## Checklist (non-TDD cleanup)

- [ ] Clippy clean on touched modules (once, Step 8)
- [ ] `CHANGES.md` entry; `docs/` config reference for `[embedding]`
- [ ] Remove the stale `semantic_search.model` mention in the `CodeDuplicationConfig` doc
- [ ] No leftover `TODO`/`FIXME`

## Follow-ups (outside this plan)

- IMPROVEMENT: doctor warns when `recall_rerank_it` is on and the jina weights are missing (the cause of `rerank_outcome = not_cached` on every Italian prompt).
- IMPROVEMENT: reranker idle unload (no unload path exists; `Load::Idle` only at start).
