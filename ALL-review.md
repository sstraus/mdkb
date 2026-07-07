---
date: "2026-07-07"
author: "main branch (mdkb 3.7.0)"
reviewers: "Multi-Agent segmented (6 blocks × specialized reviewers: security, rust, performance, data-safety, silent-failure, simplicity, architecture, test-quality)"
branch: "main"
confidence_threshold: 70
validated: true
status: "open"
---
# Code Review: mdkb — Full Codebase (segmented, 6 blocks)

**Date:** 2026-07-07
**Target:** entire `src/` + `tests/` on `main` (mdkb 3.7.0, ~68k LoC Rust)
**Strategy:** segmented — the app was split into functional blocks; specialized reviewers ran per block, then 5 fresh-context validator agents re-verified the top findings (flow-trace on all SEC/DATA). Performance impact was an explicit cross-cutting mandate for every reviewer, with emphasis on the per-turn hot paths (`mcp/dispatch`, `cli/hook_*`).

## Segmentation

| Block | Scope | Reviewers |
|-------|-------|-----------|
| A — MCP server | `src/mcp/*` | security, rust, performance |
| B — Store (SQLite) | `src/store/*` | data-safety, performance |
| C — Parsing (tree-sitter, 13 langs) | `src/code/parsing/*`, `types.rs`, `symbol.rs` | rust, simplicity |
| D — Indexing & code storage | `src/code/indexing/*`, `src/code/storage/*`, `semantic.rs`, `relationship.rs` | silent-failure, performance |
| E — CLI | `src/cli/*`, `src/main.rs` | rust, silent-failure, performance |
| F — Daemon/watcher/git | `src/daemon/*`, `src/watcher/*`, `src/git.rs` | rust (concurrency) |
| G — Domain/config/llm/metrics | `src/domain/*`, `src/config.rs`, `src/llm/*`, `src/metrics/*` | architecture, simplicity |
| H — Tests | `tests/*` | test-quality |

## Summary
- **P1 Critical:** 9 (all validator-confirmed)
- **P2 Important:** 21
- **P3 Nice-to-Have:** 18
- **Confidence threshold:** 70
- **Validated:** 27 findings re-checked by fresh-context validators — 26 confirmed, 1 retargeted (DATA-B1: right problem, wrong function), 0 dismissed.

Overall the codebase is strong: hexagonal intent, extensive schema-migration tests, deliberate fail-open hook contract, correct constant-time token compare, well-parallelized indexing pipeline with bounded channels. The critical issues cluster in two areas: (1) the MCP server's handling of caller-supplied paths/auth, and (2) the code-index refresh path, which can silently wipe or partially-build the index and repeats full-repo work on incremental runs.

---

## Fix Tracker

`[ ]` = fix not started · `[x]` = fix implemented & verified. Story IDs are wiz stories (`wiz-run stories-cli.js show <id>`). "todo.md" = deferred, no story yet.

### High-confidence findings → stories created
- [x] **055-68ec** (P1) — SEC-1, SEC-2 (MCP boundary: source_file confinement + HTTP auth)
- [x] **056-b79a** (P1) — PERF-A1, PERF-A2 (embedding/IO off the ctx mutex)
- [x] **057-ca93** (P1) — DATA-D1, ARCH-D1, BUG-D1 (index-path error honesty)
- [x] **058-8981** (P1) — PERF-D2 (code_files.rel_path index)
- [x] **059-7564** (P1) — PERF-D3 (incremental reindex re-embeds only changed symbols)
- [x] **060-8b4e** (P1) — TEST-1, TEST-2 (priors-mining + memory-graph coverage)
- [x] **061-8f5e** (P2) — BUG-C1/C2/C3, PERF-C1, DATA-C1, ARCH-C1, SIMPLE-C (parser shared helpers)
- [x] **062-4f06** (P2) — BUG-E1, BUG-E2, PERF-E1 (CLI correctness)
- [x] **063-6e8a** (P2) — BUG-F1, ARCH-F1 (daemon robustness)
- [x] **064-917d** (P2) — PERF-G1, PERF-G2 (domain hot-path perf)
- [x] **065-e71a** (P2) — BUG-B1, PERF-B1 (store error honesty + recall N+1)

### Triaged cluster → stories created (Boss-approved dispositions)
- [x] **066-e75b** (P2) — ARCH-A1 (RAII guard for reindex take/restore)
- [x] **067-b2d7** (P2) — PERF-A3 (`hook_dedup` TTL/LRU eviction)
- [x] **068-2b35** (P2) — DATA-B1 (`busy_timeout`/WAL in `Context::open`)
- [x] **069-7256** (P3) — ARCH-G1 (DELETE unused `domain/traits.rs`)
- [x] **070-8f30** (P2) — SEC-3 (default-deny daemon root whitelist in `--global`)

### Still in todo.md (no story)
- [ ] ARCH-E2 — file split (`handlers.rs`/`main.rs`/`mod.rs`); dedicated refactor when scheduled.
- [ ] PERF-A4, PERF-D4, PERF-F1 — fold into the spawn_blocking/transaction scope of 056/057, or a later story.
- [ ] P3 batch (TLS hardening, missing SAVEPOINTs, FK/UNIQUE, embedding-dim migration, ~430 lines dead code, test hygiene).

---

## P1 — Critical (Block Merge)

### [SEC-1] Arbitrary local file read via `memory_write.source_file`, persisted into a searchable store
**File:** `src/mcp/dispatch.rs:460-481`
**Reviewer:** A-mcp-security-reviewer
**Confidence:** 90 (validator-confirmed, flow-traced)
**Severity:** P1
**Issue:** `resolve_source_file` calls `std::fs::read_to_string` on a raw caller-supplied path — no allowlist, no canonicalize-then-verify against the repo root, no size cap. The content is embedded and written into the memory DB as a permanent `search`/`get`-retrievable entry (cross-repo reachable in `--global` mode). Any MCP caller (a prompt-injected agent, or a network client when SEC-2 applies) can exfiltrate `~/.ssh/id_rsa`, `~/.aws/credentials`, etc. Error path also leaks OS-level file-existence info.
**Fix:** Canonicalize and require `starts_with(&handle.root)`; reject escapes/symlinks; cap size via `Metadata::len()` before reading; return a generic error without the raw OS string.

### [SEC-2] `--http`/`--https` can run with no auth despite being documented as required
**File:** `src/main.rs:393-410`, `src/mcp/common.rs:36-69`, `src/cli/mod.rs:145-147`
**Reviewer:** A-mcp-security-reviewer
**Confidence:** 80 (validator-confirmed)
**Severity:** P1
**Issue:** `--token` help says "required for HTTP/HTTPS," but `main.rs` never enforces it and `auth_middleware` allows all requests when `state.token` is `None`. `mdkb serve --http --bind 0.0.0.0:8080` (no token) exposes full read/write memory/doc access plus the SEC-1 file-read primitive to the network unauthenticated.
**Fix:** Hard-error when starting HTTP/HTTPS with `token.is_none()`; require an explicit `--allow-no-auth` opt-out if an insecure mode is genuinely wanted.

### [PERF-A1] CPU-bound ONNX embedding computed while holding the per-repo async mutex, on the per-turn hot path
**File:** `src/mcp/dispatch.rs:2662-2706` (`hook_user_prompt_submit`), `:937-948`, `:967-996`, `:1168-1211`
**Reviewer:** A-mcp-performance-reviewer / A-mcp-rust-reviewer (both)
**Confidence:** 88 (validator-confirmed)
**Severity:** P1
**Issue:** `embed_query` (10–100 ms ONNX inference) is called synchronously while `handle.ctx.lock().await` is held, on the `UserPromptSubmit` path that fires every conversation turn — blocking a tokio worker AND serializing every other tool/hook call against that repo. The codebase already documents and applies the correct pattern (`spawn_blocking` before the lock) in `memory_write_impl`; the recall path doesn't follow its own rule.
**Fix:** Pre-compute the query embedding via `tokio::task::spawn_blocking` before acquiring the ctx lock, at all four sites.

### [DATA-D1] `update()` treats a transient DB error as "empty index" and triggers a destructive full wipe
**File:** `src/code/indexing/mod.rs:150-153`, `:162-177`, `:483-496`
**Reviewer:** D-indexing-silent-failure-hunter
**Confidence:** 65 (validator-confirmed, flow-traced; downgraded from 70 — `busy_timeout=5000` on code-index DB mitigates the most likely SQLITE_BUSY)
**Severity:** P1
**Issue:** `file_count()` is `self.db.file_count().unwrap_or(0)` — any SQLite error becomes `0`. `update()` gates on `if self.file_count() == 0 { return self.reindex(root); }`, and `reindex()` calls `db.clear()` before rebuilding. A transient lock/I-O error on a populated DB is indistinguishable from "empty" → silent full wipe+rebuild. Reachable from the MCP `update` tool (`dispatch.rs:1713`) and the SessionStart code-refresh task (`dispatch.rs:2413`).
**Fix:** Make the count helpers return `Result<u64>` (or at least log the error like their siblings); `update()` must propagate the error, never reinterpret it as empty.

### [ARCH-D1] Pipeline stage threads are never joined — panics and per-stage errors vanish, index reports success
**File:** `src/code/indexing/pipeline.rs:150-193`
**Reviewer:** D-indexing-silent-failure-hunter
**Confidence:** 78 (validator-confirmed)
**Severity:** P1
**Issue:** DISCOVER/READ/PARSE/COLLECT are spawned with handles immediately dropped. A stage panic just drops its channel sender; downstream sees a clean close and `stage_index` returns `Ok(stats)` with partial data. Combined with `reindex()`'s `db.clear()`, a mid-run panic silently leaves a mostly-empty DB reporting success.
**Fix:** Store every `JoinHandle` and `.join()` after `stage_index`; surface a thread panic as an `anyhow::Error` instead of trusting the channel close.

### [BUG-D1] Parser-construction failures silently swallowed — a whole language can vanish from the index
**File:** `src/code/indexing/pipeline.rs:269-311` (`create_parser`), `:315-338` (`stage_parse`)
**Reviewer:** D-indexing-silent-failure-hunter
**Confidence:** 75 (validator-confirmed)
**Severity:** P1
**Issue:** `create_parser` maps every constructor through `.ok()` with no logging; `stage_parse` caches the `None` and does `errors += 1; continue;` with no warning, and that `errors` count is discarded because the thread is never joined (see ARCH-D1). An entire language disappears from the index with the only symptom being "fewer symbols than expected."
**Fix:** `tracing::error!` in the `create_parser` failure branch; join the PARSE thread and fold `errors` into `IndexStats` (add `files_failed_to_parse`).

### [PERF-D2] Missing index on `code_files.rel_path` makes full/incremental indexing O(n²)
**File:** `src/code/storage/sqlite.rs:88-125`, `src/code/storage/schema.rs:11-20`
**Reviewer:** D-indexing-performance-reviewer
**Confidence:** 85 (validator-confirmed)
**Severity:** P1
**Issue:** `insert_file` unconditionally runs two legacy-cleanup DELETEs filtered on `rel_path` (only `path` and `hash` are indexed, not `rel_path`) for every file inserted — an unindexed full-table scan per insert, O(n²) over a full index.
**Fix:** `CREATE INDEX idx_files_rel_path ON code_files(rel_path);`, or gate the legacy cleanup behind a one-time migration instead of running it on every insert.

### [PERF-D3] Incremental `index_directory` re-embeds the entire symbol table instead of just changed symbols
**File:** `src/code/indexing/mod.rs:137`, `:559-592`
**Reviewer:** D-indexing-performance-reviewer / D-indexing-silent-failure-hunter (both)
**Confidence:** 82 (validator-confirmed)
**Severity:** P1
**Issue:** After an incremental run (possibly 1 changed file), `generate_symbol_embeddings()` loads `all_symbols()` and re-runs ONNX inference for every symbol, discarding the existing vector store — while `index_files` correctly uses `generate_symbol_embeddings_for_files`. Reachable via a plain `mdkb code index` re-run.
**Fix:** Route the incremental branch through `generate_symbol_embeddings_for_files(&changed, root)`; keep the full variant only for fresh index.

### [TEST-1] Zero integration coverage for the AI-distilled priors mining pipeline (flagship 3.7.0 feature)
**File:** `tests/` (absent); ref `src/domain/prior_*`, `src/store/priors.rs`
**Reviewer:** H-tests-test-quality-reviewer
**Confidence:** 90
**Severity:** P1
**Issue:** No test sets `mining_enabled = true`; the episode→candidate→distill(external CLI)→promote→inject loop has no end-to-end proof it works. `run_distiller_cli` (real `Command` spawn) has no coverage for missing binary, non-zero exit, malformed stdout, or timeout — despite hooks that must never crash.
**Fix:** Integration test with `mining_enabled = true` + a stub `distiller_program`, driving a Stop hook with a candidate-tripping transcript, asserting a promoted prior is injected; unit-test `run_distiller_cli` failure modes.

---

## P2 — Important

### [TEST-2] Memory graph (typed edges, contradicts-on-conflict, 1-hop expansion, [STALE-DEP]) has no integration coverage
**File:** `tests/` (absent); ref `src/store/memory_graph.rs`, `src/mcp/dispatch.rs`
**Reviewer:** H-tests-test-quality-reviewer · **Confidence:** 88 · **Severity:** P2
**Issue:** None of the schema-v14 memory-graph features are exercised: `relates=[...]` typed-edge creation, `on_conflict="contradicts"`, `graph(scope="memory")` traversal, post-recall neighbor expansion, `[STALE-DEP]` marker.
**Fix:** Add integration tests covering each (write+edge+`graph` query; contradicts write; STALE-DEP; capped `(via ...)` expansion).

### [PERF-B1] `has_stale_dependency` N+1 fan-out on the recall/warmup hot path
**File:** `src/store/memory_graph.rs:304-320` (called `dispatch.rs:2472,2735`)
**Reviewer:** B-store-performance-reviewer · **Confidence:** 80 (validator-confirmed; "unbounded" corrected — capped by warmup_limit 10/hooks 50 and recall_limit 5) · **Severity:** P2
**Issue:** Per entry: 2 `outgoing()` (fresh prepare each) + 1 `target_health()` per edge, looped over every warmup/recall entry on SessionStart and UserPromptSubmit.
**Fix:** Batch with a single `IN (...)` query; use `prepare_cached`.

### [PERF-A2] Blocking `resolve_source_file` read executed directly on the async runtime
**File:** `src/mcp/dispatch.rs:469` (from `:721`, `:786-789`)
**Reviewer:** A-mcp-security-reviewer · **Confidence:** 80 (validator-confirmed) · **Severity:** P2
**Fix:** Wrap the read in `spawn_blocking`, consistent with the embedding step below it.

### [PERF-A3] Daemon-lifetime `hook_dedup` map grows unboundedly across sessions
**File:** `src/mcp/dispatch.rs:52-63,121-137`
**Reviewer:** A-mcp-performance-reviewer · **Confidence:** 75 (validator-confirmed) · **Severity:** P2
**Issue:** Entries only evicted on same-key session_start/stop/wrapup; abnormally-ended sessions leak forever.
**Fix:** TTL-based sweep or LRU cap on `sessions.len()`.

### [PERF-A4] `PRAGMA incremental_vacuum` run inline in async while holding the per-repo mutex
**File:** `src/mcp/dispatch.rs:216-226`, `src/mcp/server.rs:564-576`
**Reviewer:** A-mcp-performance-reviewer · **Confidence:** 68 (validator-confirmed) · **Severity:** P2
**Fix:** Move `run_optimize`/`maybe_incremental_vacuum` into `spawn_blocking` (as `update_impl` already does).

### [BUG-A1] Byte-index string slice can panic on non-char-boundary (disambiguation error path)
**File:** `src/mcp/server.rs:510-521`
**Reviewer:** A-mcp-rust-reviewer · **Confidence:** 75 (validator-confirmed) · **Severity:** P2
**Issue:** `&s[..57]` on a signature (source-derived, may be multi-byte UTF-8) with no `is_char_boundary` check → panic crashing the MCP call.
**Fix:** Reuse `truncate_text` (already present in the same file).

### [BUG-A2] `GetParams.format = "summary"` silently ignored for document retrieval
**File:** `src/mcp/dispatch.rs:1482-1665`, `src/mcp/tools.rs:67-69`
**Reviewer:** A-mcp-rust-reviewer · **Confidence:** 70 (validator-confirmed) · **Severity:** P2
**Fix:** Implement summary for documents, or annotate the doc as memory-only.

### [ARCH-A1] Missing panic-safety around code-index/doc reindex critical sections
**File:** `src/mcp/dispatch.rs:2367-2432`, `src/mcp/server.rs:991-1088`
**Reviewer:** A-mcp-rust-reviewer · **Confidence:** 55 · **Severity:** P2
**Issue:** Resource is `take()`n from its `Mutex` and only restored after the reindex call returns; a panic leaves the flag `true` and the resource `None` forever (handle wedged until daemon restart).
**Fix:** RAII guard (generalize the existing `FlightGuard`) that restores in `Drop`.

### [BUG-C1] Substring-based visibility detection misclassifies identifiers containing modifier keywords
**File:** `src/code/parsing/csharp/mod.rs:557`, `swift/mod.rs:376`, `php/mod.rs:477`, `typescript/parser.rs:1252`
**Reviewer:** C-parsing (rust + simplicity) · **Confidence:** 85 (validator-confirmed) · **Severity:** P2
**Issue:** `first_line.contains("public")` on raw decl text; `private string publicKey;` → misclassified Public. TS scans the whole node body, so a `// private helper` comment flips visibility. Java/Kotlin do it correctly via modifier AST nodes.
**Fix:** Inspect the `modifiers`/`visibility_modifier` child node (as Java/Kotlin already do).

### [BUG-C2] Recursion-depth guard applied inconsistently — 11 of 13 languages recurse unbounded
**File:** `src/code/parsing/{python,rust,java,kotlin,csharp,cpp,c_lang,swift,gdscript,lua,php}` traversal fns
**Reviewer:** C-parsing-simplicity-reviewer · **Confidence:** 90 (validator-confirmed) · **Severity:** P2
**Issue:** Only go/typescript thread `depth` through `find_calls_in_node`/`find_method_calls_in_node`; the others guard only `extract_symbols` → stack-overflow risk on pathologically nested source.
**Fix:** Thread `depth` through all recursive traversals, mirroring go/typescript.

### [BUG-C3] `Symbol`'s `Display` impl can panic on non-ASCII doc comments
**File:** `src/code/symbol.rs:142-149`
**Reviewer:** C-parsing-rust-reviewer · **Confidence:** 85 (validator-confirmed) · **Severity:** P2
**Issue:** `&doc[..100]` byte-len-guarded only, no char-boundary check → panic on multi-byte char at offset 100. Reachable from MCP/CLI output. `safe_truncate_str` exists but isn't reused.
**Fix:** Reuse `safe_truncate_str`.

### [DATA-C1] `Range` column truncation via `as u16` can silently corrupt location data
**File:** `src/code/types.rs:26-29`; 46 cast sites across 13 modules
**Reviewer:** C-parsing-rust-reviewer · **Confidence:** 75 (validator-confirmed, flow-traced to persisted DB + MCP responses) · **Severity:** P2
**Issue:** tree-sitter column (usize byte offset) cast to `u16` unchecked; lines >65535 bytes (minified/generated) wrap silently; corrupted column persists and is returned in MCP responses.
**Fix:** Use `u32` for columns (or saturating `min(u16::MAX)` + warn), applied via a shared `node_range` helper.

### [PERF-C1] Rust parser recomputes containing function via ancestor-walk for every AST node
**File:** `src/code/parsing/rust/parser.rs:770-869` (helper `744-756`)
**Reviewer:** C-parsing-rust-reviewer · **Confidence:** 90 (validator-confirmed) · **Severity:** P2
**Issue:** `find_containing_function` (walks parent to root) called unconditionally per node → O(n_nodes × depth). All 12 other languages thread `current_fn` down; Rust is the sole outlier.
**Fix:** Thread `current_fn` through the recursion; drop `find_containing_function`.

### [ARCH-C1] `MethodCallResolver` is entirely dead code
**File:** `src/code/parsing/method_call.rs:71-124`
**Reviewer:** C-parsing-simplicity-reviewer · **Confidence:** 95 (validator-confirmed) · **Severity:** P2
**Fix:** Delete (~75 lines incl. tests), or track as a tagged TODO if intended for near-term wiring.

### [PERF-D4] Per-file `delete_by_file` calls outside a transaction during incremental reindex
**File:** `src/code/indexing/mod.rs:129-132,297-300`
**Reviewer:** D-indexing-performance-reviewer · **Confidence:** 68 (validator-confirmed; WAL mitigates fsync cost) · **Severity:** P2
**Fix:** Wrap the delete loop in one `BEGIN`/`COMMIT`, or add a bulk `delete_by_files`.

### [BUG-E1] `mdkb get` with comma-separated IDs swallows per-ID errors and always exits 0
**File:** `src/main.rs:243-258`
**Reviewer:** E-cli-silent-failure-hunter · **Confidence:** 90 (validator-confirmed) · **Severity:** P2
**Fix:** Track `had_error` and return `Err`/exit non-zero after the loop.

### [BUG-E2] `handle_get` runs the same collection scan twice for path-like IDs
**File:** `src/cli/handlers.rs:493-530`
**Reviewer:** E-cli-performance-reviewer · **Confidence:** 90 (validator-confirmed) · **Severity:** P2
**Fix:** Run the `list_collections`+`get_document_by_path` loop once regardless of input shape.

### [PERF-E1] Every CLI invocation builds a full multi-threaded Tokio runtime, including the one-shot hook client
**File:** `src/main.rs:56-60`
**Reviewer:** E-cli-silent-failure-hunter / E-cli-performance-reviewer · **Confidence:** 80 (validator-confirmed) · **Severity:** P2
**Fix:** `new_current_thread()` for the default/hook path; multi-thread only for `serve`/`mcp`.

### [BUG-F1] Watcher event channel silently drops changes under backpressure
**File:** `src/watcher/mod.rs:74-76`
**Reviewer:** F-daemon-rust-reviewer · **Confidence:** 80 (validator-confirmed) · **Severity:** P2
**Issue:** `let _ = tx.try_send(change)` into `channel(100)`; consumer stops draining during multi-second flushes; a >100-event burst (git checkout) is dropped with no rescan fallback → files silently stale.
**Fix:** Warn-once on `try_send` failure (mirror `reindex_send_warned`); set a "missed events" flag forcing a full rescan on next flush.

### [ARCH-F1] Daemon shutdown does not drain in-flight connections
**File:** `src/daemon/ipc_server.rs:112-133`
**Reviewer:** F-daemon-rust-reviewer · **Confidence:** 85 (validator-confirmed) · **Severity:** P2
**Issue:** Only accept-loop tasks are awaited; per-connection tasks are fire-and-forget, hard-aborted at runtime teardown on SIGTERM (possibly mid SQLite write).
**Fix:** Track connection handlers in a `JoinSet`; await them with a bounded grace period before unlinking sockets.

### [PERF-F1] Cold repo-open does blocking I/O on the async task, serialized behind one global lock
**File:** `src/daemon/registry.rs:195-235`
**Reviewer:** F-daemon-rust-reviewer · **Confidence:** 70 (validator-confirmed) · **Severity:** P2
**Fix:** Do whitelist/eviction/placeholder under the gate, then `spawn_blocking` the canonicalize + `Config::load`.

### [BUG-B1] `get_document_status` silently converts real DB errors into "not found"
**File:** `src/store/evolution.rs:235-255`
**Reviewer:** B-store-performance-reviewer · **Confidence:** 85 (validator-confirmed) · **Severity:** P2
**Fix:** Use `.optional()?` (as sibling functions do); default to `Current` only when the row genuinely doesn't exist.

### [DATA-B1] Production DB open path sets no `busy_timeout` (and no WAL/synchronous)
**File:** `src/cli/handlers.rs:82-117` (`Context::open`) — NOT `src/store/mod.rs:setup_pragmas`
**Reviewer:** B-store-data-safety-reviewer · **Confidence:** 55 (validator retargeted: `Store::setup_pragmas` is dead code; the real path is `Context::open`, which sets only `foreign_keys=ON`) · **Severity:** P2
**Issue:** Daemon + one-shot CLI processes open the same file as independent connections. Without `busy_timeout`, ordinary write-lock contention becomes an immediate `SQLITE_BUSY` failure. Compounds DATA-D1.
**Fix:** Add `PRAGMA busy_timeout` (and confirm WAL/synchronous intent) in `Context::open`.

### [ARCH-G1] Hexagonal ports in `domain/traits.rs` are never implemented in production
**File:** `src/domain/traits.rs:1-280`
**Reviewer:** G-domain-architecture-reviewer / G-domain-simplicity-reviewer · **Confidence:** 90 · **Severity:** P2
**Issue:** `DocumentStore`/`CollectionStore`/`SearchEngine`/`TagStore`/`LinkStore` have exactly one implementer — a test-only mock. The real store is free functions on `&Connection`; the advertised DIP boundary doesn't exist. ~280 lines of dead abstraction.
**Fix:** Decide with Boss — either wire `Store` to implement the traits (real ports & adapters) or delete `traits.rs`.

### [PERF-G1] `frontmatter.rs` recompiles regexes on every call, on the per-document indexing path
**File:** `src/domain/frontmatter.rs:84,100-101`
**Reviewer:** G-domain (architecture + simplicity, both) · **Confidence:** 90 · **Severity:** P2
**Issue:** `extract_title`/`strip_markdown_formatting` compile up to 3 fresh `Regex` per document during `mdkb update`. The fix pattern (`OnceLock`) already exists in `links.rs`/`prior_distill.rs`.
**Fix:** Hoist `h1_regex`/`bold_regex`/`italic_regex` into `OnceLock` accessors.

### [PERF-G2] Document embedding is not batched across documents, unlike the code-intel path
**File:** `src/cli/handlers.rs:1279` (cf. `src/code/semantic.rs:383-398`)
**Reviewer:** G-domain-architecture-reviewer · **Confidence:** 80 · **Severity:** P2
**Fix:** Collect single-chunk docs and `embed_documents` in `EMBED_BATCH_SIZE` groups.

### [ARCH-E1] `DispatchContext` constructed by hand 3× with a public `Arc<Mutex<T>>` field + magic constant
**File:** `src/main.rs:2625`, `src/cli/hook_client.rs:207,264`
**Reviewer:** E-cli-rust-reviewer · **Confidence:** 80 · **Severity:** P2
**Fix:** Add `DispatchContext::ephemeral()`/`::new(...)`; keep `hook_dedup` type private.

### [ARCH-E2] File-size/SRP: `handlers.rs` ~3800, `main.rs` ~2718, `mod.rs` ~1085 lines
**File:** `src/cli/handlers.rs`, `src/main.rs`, `src/cli/mod.rs`
**Reviewer:** E-cli-rust-reviewer · **Confidence:** 90 · **Severity:** P2
**Fix:** Extract by domain (`handlers/{collections,search,memory,...}.rs`, `cli::render`); dedicated refactor story, non-blocking.

---

## P3 — Nice-to-Have

- **[SEC-A4/BUG-A: TLS]** Self-signed cert SANs cover only loopback; expiry never re-checked despite comment; key briefly world-readable before chmod (Unix) and unrestricted on non-Unix. `src/mcp/https_server.rs:98-153`. (45-90)
- **[SEC-3]** `root` param + empty-default whitelist lets a daemon client point at arbitrary dirs (auto-creates `.mdkb/`, spawns watcher). `daemon/config.rs:68,134`. (75, validator-confirmed)
- **[STYLE-A1]** Duplicated `mcp_error` in `server.rs` vs shared `mod.rs`. (95)
- **[PERF-A5/A6/A7]** N+1 collection doc-count in `status`; `resolve_document` scans every collection per lookup (compounded in batch `get`); intermediate `serde_json::Value` alloc for symbol serialization. (60-82)
- **[PERF-B*]** graph BFS per-edge queries; no `prepare_cached` on search hot path; `merge_small_chunks` O(n²) `Vec::remove`; O(n²) `find` in memory hybrid ranking; missing `query_events(created_at)` index + redundant percentile COUNTs. `src/store/{graph,search,chunks,memory,stats}.rs`. (55-80)
- **[DATA-B2]** `rename_collection` no `ON UPDATE CASCADE` → opaque FK error on non-empty collections; untested. (85, validator-confirmed)
- **[DATA-B3..6]** Missing `SAVEPOINT` in `priors::integrate_candidate_with_embedding` and `stats::record_call`; `find_or_create_agent_session` TOCTOU (no UNIQUE on `sessions.agent`); `query_events.session_id` no FK (dangles after prune). (70)
- **[ARCH-B1]** `EMBEDDING_DIM` hardcoded in 3 DDL literals; dimension-change migration only covers `vec_documents`, not `vec_chunks`/`vec_memory`. (75, validator-confirmed)
- **[PERF-B/DATA]** `archive_missing_sessions` N+1 write; `list_prunable_sessions` lacks compound index. (55-80)
- **[BUG-C2/DATA-C1]** Substring const/let detection false-positives; `CachingParser` bare-hash cache identity (no collision fallback). (40-60)
- **[SIMPLE-C]** `node_range()` duplicated in 12 modules + inlined 11× in Rust (~150 lines); block-doc stripping duplicated in 5 modules (~70 lines); ~285 lines removable total. (90-95)
- **[SILENT-D]** `file_mtime(...).unwrap_or(0)` unlogged; autovacuum-setup `let _ =`; discarded `stage_parse`/`stage_collect` diagnostics. (45-60)
- **[SILENT-E]** Bulk commands (`update`/`export`/`import`) report per-item errors but exit 0; malformed hook stdin discarded with no trace; `memory condense` DB errors == "not found"; post-VACUUM "0 KB" on stat failure. (40-65)
- **[STYLE-E1]** Inconsistent `unwrap()` vs `unwrap_or_default()` on `to_string_pretty` across `format_*`. (45)
- **[STYLE-F]** `FileWatcher::watch/unwatch` take `&PathBuf` (`clippy::ptr_arg`); `_debouncer` misleading underscore; duplicated pid-path fallbacks that disagree; `ensure_base_dir_0700` follows symlinks; no read timeout on hook socket. (40-90)
- **[G]** `Config::from_env_with_defaults` / `TokenCounter` / `mcp.include_token_count` dead; `UsageMetrics` closed-set; `config.rs` cohesion (1447 lines). (60-95)
- **[TEST-3/4, PERF-1/2, DOC-1]** FTS5 special-char integration under-covered; `smoke_hook_stop` only default path; flaky p50 timing assertion; two 1s mtime sleeps (use `filetime`); stale `cli_smoke` count in CLAUDE.md. (55-85)

---

## Dismissed by Validator
None. Of 27 validated findings, 26 confirmed and 1 (DATA-B1) retargeted to the correct function with adjusted confidence 55 — kept because the underlying concurrency risk is real (production is in fact worse than the original finding described).

---

## Cross-Cutting Analysis

### Root causes
| Root cause | Findings | Single fix |
|---|---|---|
| **Blocking/CPU work inside the async ctx mutex** | PERF-A1, PERF-A2, PERF-A4, PERF-F1 | Establish one rule + helper: never hold `ctx.lock().await` across `embed_*`, file I/O, or vacuum — always `spawn_blocking` first. The pattern is already documented in `memory_write_impl`; propagate it. |
| **Silent-error → destructive/incomplete index** | DATA-D1, ARCH-D1, BUG-D1 | Stop swallowing SQLite/thread errors on the index path: count helpers return `Result`, pipeline threads joined, parser-construction logged. Fixes the "reports success on a broken index" class. |
| **Copy-paste template drift across 13 language parsers** | BUG-C1, BUG-C2, PERF-C1, DATA-C1, SIMPLE-C, BUG-C3 | Extract shared helpers into `parsing/parser.rs` (`node_range` with saturating cast, `strip_block_doc`, threaded `depth`, structural visibility) — one fix closes 6 findings and ~285 lines. |
| **Unvalidated caller-supplied input at the MCP boundary** | SEC-1, SEC-2, SEC-3 | A boundary-validation pass on `source_file`/`root`/auth: confine paths to repo root, enforce token, default-deny whitelist. |
| **UTF-8 byte-slice truncation** | BUG-A1, BUG-C3 | One `safe_truncate` used everywhere (already exists in `parsing/parser.rs`). |
| **N+1 / unbatched DB access** | PERF-B1, PERF-A5, PERF-A6, PERF-D4, DATA-B-writes | Batch with `IN (...)`; adopt `prepare_cached` on hot paths. |
| **Config re-parsed per handler call** | PERF-G3 | Cache `Config` in `Context` once. |

### Single-fix opportunities (highest impact/effort)
1. **`spawn_blocking` discipline on the async lock** → PERF-A1 (P1) + PERF-A2/A4/F1. ~1 helper + 5 call sites.
2. **Index-path error honesty** → DATA-D1 + ARCH-D1 + BUG-D1 (three P1s). Return `Result` from counts, join threads, log parser failures.
3. **Shared parser helpers in `parser.rs`** → 6 Block-C findings + ~285 lines removed.
4. **`code_files.rel_path` index** → PERF-D2 (P1), also speeds `run_repairs`.
5. **MCP boundary validation** → SEC-1/2/3 in one focused pass.

### Context files (read before fixing)
| File | Why | Referenced by |
|---|---|---|
| `src/mcp/dispatch.rs` (`memory_write_impl` 707-736) | The correct spawn_blocking-before-lock pattern to copy | A-perf, A-rust |
| `src/code/indexing/mod.rs` (`index_files` vs `index_directory`) | Correct incremental embedding path to mirror | D-perf, D-silent |
| `src/code/parsing/parser.rs` (`safe_truncate_str`, `check_recursion_depth`) | Shared helpers to reuse | C-rust, C-simplicity |
| `src/cli/handlers.rs` (`Context::open`) | The real production DB-open path (not `store::mod`) | B-data-safety |
| `src/store/search.rs` (`populate_superseded_by`) | Existing correct batching pattern | B-perf |

## Recommended Actions
1. **Before merge:** SEC-1, SEC-2, PERF-A1, and the index-path P1 cluster (DATA-D1, ARCH-D1, BUG-D1). PERF-D2/D3 are cheap and high-value — do them in the same pass.
2. **This cycle:** the P2 performance/N+1 items and the Block-C shared-helper refactor; add TEST-1/TEST-2 coverage for the 3.7.0 subsystems.
3. **Follow-up:** P3 cleanups, dead-code removal (~400+ lines across G + C), file-size refactors, test hygiene (`filetime`, flaky p50).
