# todo.md — deferred opportunities

Captured during the graph-augmented-recall work (plan: `plans/graph-augmented-recall.md`).
Each item is an idea that needed discussion, larger scope, or product validation, so it was
NOT implemented inline.

## From /wiz:review (deferred — pre-existing or refactor-scoped)

These were raised by the multi-agent review. The high-value, low-risk findings were fixed
inline (single-resolve `resolve_to_path`, static wiki-link regexes, redundant `ensure_handle_context`
removed, `path_like_tokens` non-markdown filter, error-visibility logging, E2E coverage for
permissionDecision/caps/wikilink). The following were deferred:

- **`resolve_ref_to_doc` is collection-unscoped (P2, data-safety).** `SELECT id FROM documents WHERE
  relative_path = ? LIMIT 1` ignores the `(collection, relative_path)` UNIQUE key, so in a
  multi-collection store two docs with the same relative path resolve non-deterministically. This is
  pre-existing graph behavior, now reachable from the UPS hot path. Proper fix threads a collection
  scope through `resolve_ref_to_doc`/`doc_graph_neighbors`. Single-collection stores (the common case)
  are unaffected. Complexity M. Priority P2.
- **`IndexFacade::open_existing` to harden the `code_index_hits` TOCTOU (P3).** `acquire_handle_code_index`
  uses `open_or_create`, so a delete racing the `.exists()` guard can create an empty DB. It's benign
  (empty → falls back to suggestion) and now documented, but an open-only variant would make the
  "never create on the hot path" contract enforced by the type rather than a guard. Complexity S. P3.
- **DRY the Bash pipeline parse (P3, arch/perf).** `bash_definition_symbol` duplicates the
  `unwrap_shell_c → pipeline_sources → tokenize_shell → extract_grep_pattern` skeleton from
  `classify_bash_search`, and a Bash definition search currently parses the command twice (once for the
  suggestion, once for the symbol). Extract `first_grep_pattern_from_command` and thread the parsed
  result so both the suggestion and the code-index symbol come from one parse. Pure string ops (no I/O),
  so this is cleanup not a bug. Complexity S. P3.
- **Push the `KIND_FRONTMATTER` filter into SQL (P3).** `doc_graph_neighbors` calls
  `get_outgoing(.., None)` and filters `source_kind` in Rust, pulling wikilink rows it discards. Add a
  `source_kind` filter to `get_outgoing` (or a sibling). Bounded by cap=3, so low impact. Complexity S. P3.
- **Consider moving `doc_graph_neighbors`'s query into `store::graph` (P3, arch).** The resolution loop is
  pure graph logic; only the `- path (relation)` formatting is a dispatch concern. Splitting query from
  formatting would let `canonical_key` stay private and keep the storage API cohesive. Complexity S. P3.

## From code-index reliability work (stories 033/034/035, 2026-07-05)

High-value/low-risk fixes shipped inline: FK root-cause (`insert_file` upsert rowid via `RETURNING id`
+ loud pipeline map-miss), incremental `IndexFacade::update` (no-op reindex 165s→0.85s live), file
size cap (`MAX_INDEXABLE_FILE_BYTES`), watcher no longer exits on late ctx + bounded send-failure log.
Deferred:

- **`MAX_INDEXABLE_FILE_BYTES` is a hardcoded const (P3, configurability).** The 1 MiB parse cap lives
  in `pipeline.rs`. Some repos legitimately have >1 MiB generated source they DO want indexed (or want a
  tighter cap). Surface it as `[code.indexing].max_file_bytes` in config, defaulting to the const.
  Complexity S. Priority P3.
- **`max AST depth exceeded` warning doesn't name the file (P3, DX).** `check_recursion_depth(depth, node)`
  logs only `row:col` (`parser.rs:78`), so a pathological file is unidentifiable from the log — the exact
  gap story 035's criterion #3 hit. The file path isn't in scope at the ~30 recursive call sites across
  all language parsers; threading it through is invasive. Cleaner: have `stage_parse` (which knows the
  path) tag the warning, or return a per-file "depth-exceeded" flag up to the pipeline. Complexity M. P3.
- **FSEvents stream death has no auto-restart (P2, resilience).** When `watcher.recv()` returns `None`
  (backend died) the watcher now logs an actionable error and exits, but recovery still needs a daemon
  restart. A supervised respawn (recreate the FileWatcher, keep the same `reindex_rx`) would self-heal.
  Requires care around the consumed receiver. Complexity M. Priority P2.
- **Consume injected reindex paths independent of ctx (P3, robustness).** During the ctx-wait the watcher
  buffers injected paths (channel cap 64) but doesn't drain them until ctx arrives; a burst of >64 edits
  before the first client request would drop the overflow. Restructure so the injected-path branch runs
  before/without the collection-list resolution (which is the only thing that needs ctx). Complexity M. P3.

## 0. Pin the Rust toolchain so `cargo fmt --check` is deterministic

- **Problem:** There is no `rust-toolchain.toml`. CI uses `dtolnay/rust-toolchain@stable`,
  which floats to the latest stable. rustfmt `1.9.0` (2026-05-25) reformats code that earlier
  rustfmt versions left alone — so `cargo fmt --check` fails on `main` too (verified:
  `main:src/git.rs:421`, `main:src/store/memory.rs`), independent of any feature work. This
  branch normalized `git.rs` + `memory.rs` to 1.9.0's canonical form to make `make check` green,
  but the drift will recur on the next rustfmt bump.
- **Proposed solution:** Add a `rust-toolchain.toml` pinning a specific channel/version (with
  `components = ["rustfmt", "clippy"]`) so local and CI format identically and deterministically.
- **Benefits:** `cargo fmt --check` becomes reproducible; no more "works on my machine" fmt
  churn; contributors don't fight rustfmt version skew.
- **Trade-offs:** Requires periodic intentional bumps of the pinned version (which is the point).
- **Complexity:** S.
- **Priority:** P2 (infra hygiene; prevents recurring red `make check`).

## 1. Code-index path base is inconsistent (subdir vs root indexing)

- **Problem:** `mdkb code index src/` stores `code_symbols.file_path` relative to the indexed
  argument (`lib.rs`), while `mdkb code index` (from root) stores it root-relative
  (`src/lib.rs`). The new PreToolUse `file:line` injection surfaces whatever is stored — so a
  project indexed via a subdir argument emits hints that are NOT openable from the repo root.
- **Proposed solution:** Always store `file_path` relative to the project root regardless of the
  indexing argument (canonicalize the indexed path against root, then strip the root prefix).
- **Benefits:** Hints (and `code search`/`code find` output) are always openable from root;
  removes a latent foot-gun; one source of truth for paths.
- **Trade-offs:** Touches the indexing pipeline and requires a one-time reindex; needs an audit of
  all `file_path` consumers (`delete_by_file`, mtime compare, FTS).
- **Complexity:** M.
- **Priority:** P2 (the production hint path indexes from root, so it's correct today; this is
  defense-in-depth + DX).

## 2. Doc-graph recall only resolves explicit paths, not doc titles/slugs

- **Problem:** `path_like_tokens` intentionally detects only `.md` tokens, `/`-paths, and
  `[[wikilinks]]`. A prompt that names a doc by its human title ("the authentication design doc")
  won't trigger neighbor injection. Bare-word resolution was excluded to avoid resolving every
  noun to a document.
- **Proposed solution:** Add an optional fuzzy pass: when a prompt contains a multi-word phrase
  that strongly matches a single document title (high-confidence FTS over `documents.title`),
  treat it as a doc reference and inject its neighbors.
- **Benefits:** Graph value reaches natural-language prompts, not just path-literal ones.
- **Trade-offs:** False positives risk noise on the recall hot path; needs a confidence gate and
  measurement before enabling.
- **Complexity:** M.
- **Priority:** P3 (validate demand via telemetry first).

## 3. Memory→memory graph edges (the original option B) — ✅ DONE

- **Status:** Shipped via `plans/memory-edges-phase23.md` (schema v14 `memory_edges`, typed edges,
  `memory_write` `relates`/`on_conflict`, `graph scope=memory`, CLI `memory link`, provenance,
  post-recall 1-hop expansion, `[STALE-DEP]` flag). The `DEFERRED (2026-06-30)` comment in
  `hook_user_prompt_submit_impl` is lifted.
- **Problem:** Memories are not nodes in the doc graph (`edges.source_doc_id` FKs `documents.id`;
  memory ids are TEXT slugs with no `documents` row), so 1-hop memory→memory expansion in recall
  is impossible without new storage. Marked `DEFERRED (2026-06-30)` in
  `hook_user_prompt_submit_impl`.
- **Proposed solution:** A `memory_edges` table (memory_id ↔ memory_id, typed relation) populated
  from explicit cross-references in memory content, plus a post-recall 1-hop expansion step.
- **Benefits:** Recall can surface related decisions/problems even when FTS misses them.
- **Trade-offs:** Low yield at the current ~12-entry corpus; adds write-path complexity and an
  edge-maintenance burden. Second opinion concurred it's premature.
- **Complexity:** M–L.
- **Priority:** P3 (revisit when the memory corpus is large enough that hybrid recall stops being
  near-total).

## 4. Session transcript indexing re-embeds the whole file on every append (story 036) — ✅ DONE

- **Problem/opportunity:** `handle_session_index` (`src/cli/handlers.rs:4732`) dedups session
  chunks by **file mtime** (`existing_doc.file_modified_at == file_mtime`). A Claude transcript
  (`~/.claude/projects/<proj>/*.jsonl`, 10–20 MB, append-only) bumps its mtime on every growth, so
  ALL of its chunks fail the mtime check and get re-embedded — even chunks whose content is
  byte-identical. This was a primary driver of the content-table leak and reindex CPU. Crucially,
  chunk boundaries are **stable from turn 0** (`parse_session_file`, `src/domain/sessions.rs:216`:
  fixed stride `chunk_size - overlap = 8`, key `{sid}-chunk-{NNN}`), so on append only the tail
  chunk(s) actually change — the earlier chunks are unchanged content under a stable key.
- **Proposed solution (recommended: incremental / content-hash dedup):** Switch the skip condition
  from file-mtime to the **per-chunk content hash** the document layer already computes
  (`index_document_in_tx`). Compute the hash of `sdoc.content` up front and skip when it equals
  `existing_doc.hash`. Append-only growth then re-embeds only the genuinely new/changed tail chunks.
  Alternatives considered: (b) summarize/truncate — lossy, needs a summarizer, hurts recall value;
  (c) drop `claude_sessions` — cheapest but loses session recall entirely; (d) keep full — the bug.
- **Benefits:** Bounds re-embedding to the delta; lossless; consistent with the code-index
  content-hash philosophy (stories 033/035); kills the leak/CPU driver.
- **Trade-offs:** Hash-per-chunk cost on each pass (cheap vs. re-embedding); the final partial chunk
  still re-embeds each time its content grows (acceptable — it's one chunk). If chunking ever moves
  to variable/end-anchored boundaries, the stable-key assumption breaks and needs revisiting.
- **Complexity:** S (dedup predicate swap; no schema change — `documents.hash` already exists).
- **Priority:** P3 (per story), but high value/low risk once the direction is confirmed.
- **Status:** ✅ Shipped (commit `f2e213b`). Boss chose incremental; `handle_session_index`
  now dedups by `documents::compute_hash(sdoc.content)` vs `existing_doc.hash` (not file mtime),
  so an append re-embeds only the changed tail. Regression + delta-bounded cost tests added
  (`test_handle_session_index_append_skips_unchanged_chunks_despite_mtime_bump`,
  `_append_cost_is_delta_bounded`). Noise-stripping confirmed already handled in
  `domain/sessions.rs` (user/assistant text blocks only).
- **Remaining (not blocking):** the final partial chunk still re-embeds as it grows (one chunk);
  and `<system-reminder>`/`<local-command-*>` wrappers embedded as plain text inside user blocks
  are still indexed (a small `extract_text` strip pass would remove them) — Boss judged this low
  priority.

## From mdkb×wiz synergy-audit implementation (stories 039-044, 2026-07-06)

High-value/low-risk fixes shipped inline across these stories. Observations deferred:

- **`update_entry` performs a partial-field UPDATE (P2, data-safety).** It writes only
  title/content/entry_type/tags/status/superseded_by/expires_at/due_at/source_type (source_type
  added in story 040). It silently omits `source_path`, `confirmations`, `last_confirmed_at`,
  `access_count`, `last_accessed`. Those are intentionally owned by other paths (confirm_entry,
  access tracking), so today it's safe-by-omission — but it's a footgun: any caller that reads an
  entry, mutates one field, and calls `update_entry` expecting a full write would silently drop the
  omitted columns. Proposed: split into `update_entry_content` (the current surgical set) vs a
  full-row `upsert`, or document the contract loudly. Complexity S. Priority P2.
- **CLI top-level errors render as verbose Debug (P3, DX).** `mdkb memory confirm ghost ...` prints
  `Error { kind: InvalidQuery("Memory entry not found: ghost"), backtrace: <disabled> }` instead of
  a clean one-line message. This is the `main()` error path (`{:?}` on the top-level error), affecting
  every command. A `Display` impl or a `main` that formats `err` with `{}` would fix it globally.
  Complexity S. Priority P3.
- **`memory confirm` (and MCP `memory_confirm`) update the DB but not the on-disk `.md` projection
  (P3, consistency).** Confirmations/last_confirmed_at live only in the DB after a confirm; the
  markdown file's frontmatter goes stale until the next full write. DB is the source of truth for the
  confidence signal so search/warmup are correct, but the file drifts. Story 048 (memory storage
  reconciliation: DB source of truth, files projection) is the proper home for this. Complexity M. P3.

- **One-shot CLI hook invocations spawn a file watcher (P2, waste + surprising I/O).**
  `RepoRegistry::get_or_open` unconditionally calls `spawn_watcher_for_handle`, even from
  `run_hook_in_process` (MDKB_NO_DAEMON=1), where the process exits milliseconds later. The watcher
  is pointless there and can race to bootstrap `code.sqlite` if given enough wall-clock time (this
  surfaced during story 045 as a hook creating the code index; worked around by not forcing a DB open
  on the hook path). Proper fix: thread a `spawn_watcher: bool` (or `one_shot`) flag through
  `get_or_open` so one-shot CLI paths skip the watcher entirely. Complexity M. Priority P2.

- **`[models].inactivity_timeout_secs` is also dead (P3, cleanup).** After story 046 removed the
  dead `embedding_repo`/`embedding_file` keys, `inactivity_timeout_secs` is the sole remaining
  `[models]` field and has zero readers (model unload-on-idle was never wired; `release_cached_service`
  is called explicitly, not on a timer). Either wire it to an actual idle-unload timer or drop the
  whole `[models]` section. Left in place because removing the last field / the section is a broader
  behavior change than story 046 scoped. Complexity S. Priority P3.

## From mdkb-indexing-automation plan (stories 050-054, 2026-07-06)

- **Clippy debt: 349 pre-existing warnings on committed `main` (P3, maintainability).**
  Problem: `cargo clippy --lib --tests` on committed HEAD (524c84a) reports 349 warnings (e.g.
  `let...else` candidates, unnested or-patterns, unreadable literals, `unused async for function
  with no await` on `hook_post_tool_use_impl`). Measured, not assumed (via a `git worktree` at
  HEAD). None are from the indexing-automation work (all new code verified clippy-clean per
  line-range), but the baseline noise makes it impossible to enforce "clippy clean" as a CI gate.
  Proposed: a dedicated clippy-cleanup pass (`cargo clippy --fix` for the mechanical ones, manual
  for the rest), then wire `-D warnings` into CI. Benefit: real lints stop hiding in the noise; the
  plan-checklist "clippy clean" item becomes truthfully enforceable. Trade-off: a large, mostly
  mechanical diff that touches many files (review churn). Complexity M. Priority P3.

- **Watcher spawn threads 9-10 positional params (P3, code-org).** `run_file_watcher` /
  `run_file_watcher_inner` (src/mcp/server.rs) now carry `#[allow(clippy::too_many_arguments)]`
  (matching the repo pattern; the inner fn was already 9/7 before this plan). Proposed: bundle the
  code-watch tunables (`code_enabled`, `code_ignore_patterns`, `respect_gitignore`, `debounce_ms`,
  `batch_idle_ms`) into a small `CodeWatchConfig` struct threaded through
  `spawn_watcher_for_handle`. Benefit: drops the arg count under the lint threshold, removes both
  `#[allow]`s, one cohesive type. Trade-off: touches registry + server + 3 test call sites. Left out
  of story 053 as scope creep (the story was about config-driving two values, not restructuring the
  watcher signature). Complexity S. Priority P3.

- **Umbrella-store host cleanup still pending Boss (P3, hygiene).** Story 054's two [MANUAL] criteria
  were rejected (destructive, outside the project working dir). All three stores confirmed present:
  `~/Gits/.mdkb`, `~/Gits/LS/.mdkb`, `~/Gits/CC_Playground/.mdkb`. Story 052's nested-`.mdkb` prune
  already stops them re-indexing sub-repos, so this is optional hygiene. To retire non-destructively:
  `printf '' > ~/Gits/.mdkb/.mdkbignore-hooks` (per store); or remove: `rm -rf ~/Gits/.mdkb
  ~/Gits/LS/.mdkb ~/Gits/CC_Playground/.mdkb`. Complexity S (manual). Priority P3.

## From full-codebase /wiz:review (segmented, 2026-07-07)

Full audit of `src/` + `tests/` on `main` (6 blocks × specialized reviewers + 5 validators). Nothing
was fixed inline — this was an audit, and every item needs a decision, touches a per-turn hot path
that must be re-tested, or is a cross-cutting refactor. Full detail + confidence/validator verdicts in
`ALL-review.md`. Summary of deferred items by severity.

**Status tracker** (`[ ]` = not started, `[x]` = done & verified). Findings 055-070 are now wiz stories
(`wiz-run stories-cli.js list`); the items below are what remains OUTSIDE those stories, tracked here:

- [x] ARCH-G1 — promoted to story **069-7256** (decision: DELETE `domain/traits.rs`)
- [x] SEC-3 — promoted to story **070-8f30** (decision: default-deny whitelist in `--global`)
- [x] ARCH-A1 — promoted to story **066-e75b** (RAII guard for reindex take/restore)
- [x] PERF-A3 — promoted to story **067-b2d7** (`hook_dedup` TTL/LRU eviction)
- [x] DATA-B1 — promoted to story **068-2b35** (`busy_timeout`/WAL in `Context::open`)
- [x] BUG-A1 — folded into story **055-68ec** as an extra criterion (plan open-question default):
      char-boundary-safe truncation in `server.rs` disambiguation_error
- [ ] BUG-A2 — `GetParams.format = "summary"` silently ignored for document retrieval
      (`dispatch.rs:1482-1665`, `tools.rs:67-69`, conf 70). Proposed: implement summary for documents
      or document format as memory-only. Benefit: API honesty — callers asking for summary get one.
      Trade-off: summary quality for arbitrary docs vs. a doc-comment fix. Complexity S. Priority P3
      (plan open-question default: not storified this cycle)
- [ ] TEST-STALE-EMBED — `tests/e2e_llm.rs::test_handle_embed_generates_embeddings` (ignore-gated)
      asserts `generated == 3, skipped == 0`, but `setup_test_env` now runs `handle_update`, which
      auto-embeds docs (the 3.7.0 non-aggressive auto-embed feature). By the time `handle_embed` runs
      the docs are already embedded, so the real result is `generated == 0, skipped == 3`. Confirmed
      pre-existing (fails identically without the story-064 changes). Proposed: update the assertion to
      the post-auto-embed contract (or seed docs without triggering auto-embed). Benefit: the ignored
      LLM suite reflects current behavior. Complexity trivial. Priority P3 (test hygiene). Not fixed in
      064 to avoid silently rewriting a test expectation outside the story's scope.
- [ ] 061 REMAINDER (criterion 3) — add recursion-depth guards to the secondary
      call/relationship walks (`find_calls_in_node`, `find_method_calls_in_node`,
      `find_implementations_in_node`, `find_defines_in_node`, `find_uses_in_node`)
      in **cpp, csharp, java, kotlin** (~16 fns). Same mechanical transform already
      applied to rust/c_lang/php/swift/lua/gdscript: add `depth: usize` param,
      `if !check_recursion_depth(depth, *node) { return; }` at the top, `depth + 1`
      in the recursive self-call, `0` at the `_impl` caller. Main `extract_symbols`
      walks are already guarded in all 13; this closes the drift on the secondary
      walks (stack-overflow safety for pathological ASTs). Complexity S, Priority P2.
- [ ] 060 (P1) — Integration coverage for priors mining + memory graph: NOT STARTED.
      Needs an E2E test with mining_enabled + a stub distiller driving a Stop hook
      (assert promoted prior injected), run_distiller_cli failure-mode unit tests
      (missing binary, non-zero exit), and memory-graph tests (relates edges via
      graph scope=memory, on_conflict=contradicts, STALE-DEP marker, capped 1-hop
      expansion). Large fixture effort. Priority P1.
- [ ] PERF-A4 / PERF-D4 / PERF-F1 — spawn_blocking + transaction cluster (fold into story 056/057 or a later story)
- [ ] P3 batch: TLS hardening, missing SAVEPOINTs, FK/UNIQUE, embedding-dim migration gap, ~430 lines
      dead code, test hygiene (all low/trivial complexity)

(Note: `[x]` above means "triaged into a story", not "fix shipped". Implementation status lives on the
stories themselves.) The severity summary below is the full finding list as reviewed:

### P1 (block-merge)

- **SEC-1 — Arbitrary file read via `memory_write.source_file` (P1).** `resolve_source_file`
  (`dispatch.rs:460`) reads any caller path (no allowlist/size cap) and persists it into the searchable
  memory DB → exfiltration primitive. Fix: canonicalize + `starts_with(root)`, size cap, generic error.
  Not inline: security-sensitive, needs threat-model confirmation on legit out-of-repo reads. Value:
  closes a prompt-injection/network exfil path. Complexity S. Validator-confirmed 90.
- **SEC-2 — HTTP/HTTPS runs unauthenticated (P1).** `--token` documented as required but never enforced;
  `auth_middleware` allows all when token is None. Fix: hard-error on `--http/--https` without a token
  (`--allow-no-auth` opt-out). Not inline: a behavior change that breaks tokenless setups. Complexity S.
  Validator-confirmed 80.
- **PERF-A1 — ONNX embed under the async ctx mutex on the per-turn path (P1).** `embed_query` runs sync
  inside `ctx.lock().await` in `hook_user_prompt_submit` + 3 search sites. Fix: `spawn_blocking` before
  the lock (pattern already in `memory_write_impl`). Not inline: hot-path change, must be benchmarked.
  Value: removes per-turn stall + cross-call serialization. Complexity S. Validator-confirmed 88.
- **DATA-D1 — Transient DB error → silent destructive index wipe (P1).** `file_count().unwrap_or(0)`
  makes any error look "empty"; `update()` → `reindex()` → `db.clear()`. Fix: count helpers return
  `Result`, `update()` propagates. Not inline: touches the index-refresh contract. Complexity M.
  Validator-confirmed 65.
- **ARCH-D1 + BUG-D1 — Pipeline threads never joined; parser-construction failures swallowed (P1).**
  Stage panics/errors look like clean channel closes → partial index reports success; a whole language
  can vanish. Fix: join stage handles, surface panics, log parser failures, fold `errors` into stats.
  Not inline: concurrency restructure needing care. Complexity M. Validator-confirmed 78/75.
- **PERF-D2 — Missing `code_files.rel_path` index → O(n²) indexing (P1).** `insert_file` runs unindexed
  `rel_path` DELETEs per file. Fix: add the index (or gate the legacy cleanup). Not inline: schema
  change + reindex, wanted a batched decision. Value: O(n²)→O(n log n). Complexity trivial.
  Validator-confirmed 85.
- **PERF-D3 — Incremental reindex re-embeds the whole symbol table (P1).** `index_directory` calls the
  full `generate_symbol_embeddings()` for a 1-file change. Fix: use the existing
  `generate_symbol_embeddings_for_files`. Not inline: hot-path behavior change. Value: incremental cost
  ∝ change set, not repo size. Complexity S. Validator-confirmed 82.
- **TEST-1 + TEST-2 — No integration coverage for priors-mining + memory-graph (P1).** The two flagship
  3.7.0 subsystems have zero E2E tests; `run_distiller_cli` failure modes untested. Fix: E2E tests with
  stub distiller + memory-graph edge/contradicts/STALE-DEP/expansion assertions. Not inline: fixture
  effort beyond an audit. Value: protects the release's headline features. Complexity M. Confidence 90/88.

### P2 (this cycle)

- **PERF-A2/A4/F1 — companion blocking-under-lock sites.** `resolve_source_file` read, `incremental_vacuum`,
  cold repo-open on `open_gate`. Fix: `spawn_blocking` / move I/O off the gate. Complexity L-M. P2.
- **Block-C parser template drift (BUG-C1/C2/C3, PERF-C1, DATA-C1, SIMPLE-C).** Substring visibility
  misclassification, inconsistent recursion guards, Rust per-node ancestor walk (O(n·depth)), unchecked
  `as u16` column truncation, UTF-8 slice panic in `Symbol::Display`, ~285 duplicated lines. Fix: extract
  shared helpers into `parsing/parser.rs`. One refactor closes 6 findings. Complexity M. P2.
- **N+1 / unbatched DB access (PERF-B1, PERF-A5/A6, PERF-D4, DATA writes).** Batch with `IN (...)`, single
  `GROUP BY`, transaction-wrap delete loops, `prepare_cached`. Complexity L-M. P2.
- **DATA-B1 (retargeted) — `Context::open` sets no `busy_timeout`/WAL.** The real production DB-open path
  (not the dead `Store::setup_pragmas`) sets only `foreign_keys=ON`; concurrent daemon+CLI writers get
  immediate SQLITE_BUSY. Fix: add `PRAGMA busy_timeout` in `Context::open`. Complexity trivial. P2.
- **BUG-B1 — `get_document_status` swallows real DB errors** (`.ok()` → treated as not-found). Fix:
  `.optional()?`. Complexity trivial. P2.
- **ARCH-A1 — no panic-safety around reindex take/restore** (wedges the handle forever on panic). Fix:
  RAII guard generalizing `FlightGuard`. Complexity L-M. P2.
- **BUG-E1/E2 + PERF-E1 — CLI exit-code 0 on all-fail `get`; double collection scan; multi-thread runtime
  for one-shot hooks.** Fix: `had_error` non-zero exit; single scan; `new_current_thread()` for
  non-serve. Complexity S. P2.
- **BUG-F1 + ARCH-F1 — watcher drops events under backpressure; shutdown doesn't drain connections.**
  Fix: warn-once + rescan flag; `JoinSet` drain with grace period. Complexity M. P2.
- **ARCH-G1 — `domain/traits.rs` is a dead abstraction (~280 lines, test-only impl).** Decide with Boss:
  wire `Store` to the ports, or delete. Needs a decision. Complexity S(delete)/H(wire). P2.
- **PERF-G1/G2/G3 — regex recompiled per document; unbatched doc embedding; `Config` re-parsed per
  handler call.** Fix: `OnceLock` regexes, batch `embed_documents`, cache `Config` in `Context`.
  Complexity S. P2.
- **ARCH-E1/E2 — hand-built `DispatchContext` ×3 (public `Arc<Mutex>` + magic const); `handlers.rs`
  ~3800 / `main.rs` ~2718 / `mod.rs` ~1085 lines.** Fix: constructor; extract by domain. Complexity M. P2.

### P3 (nice-to-have, batched)

TLS hardening (SEC-A4); `root` default-deny whitelist (SEC-3); missing SAVEPOINTs/FK/UNIQUE
(DATA-B3..6); embedding-dimension migration gap (ARCH-B1); store micro-perf (graph BFS per-edge queries,
`prepare_cached`, `merge_small_chunks` O(n²), `query_events(created_at)` index); silent-failure logging
(SILENT-D/E: unlogged mtime/autovacuum, bulk-command exit codes, `memory condense` masking, VACUUM
"0 KB"); dead code (`Config::from_env_with_defaults`, `TokenCounter`, `mcp.include_token_count`,
`MethodCallResolver`, ~430 lines); style (duplicate `mcp_error`, `&PathBuf` args, `_debouncer`,
diverging pid-path fallbacks, symlink-following chmod, no hook-socket read timeout); test hygiene
(FTS5 special-char coverage, `smoke_hook_stop` mining path, flaky p50 assertion, two 1s mtime sleeps →
`filetime`, stale `cli_smoke` count in CLAUDE.md). Each low/trivial complexity.
