---
date: "2026-09-12"
author: "main branch (mdkb 3.8.0)"
reviewers: "9 parallel audit agents (architecture, storage, code-intel, runtime, agent-ergonomics, tests, deps/security, + 2 external-research) + Fable second opinion"
branch: "main"
status: "open"
supersedes_context: "ALL-review.md (3.7.0, 2026-07-07) — its 9 P1s are closed; this review starts from there"
---

# Closing review: mdkb 3.8.0

**Scale (measured):** 132 files, 105,953 LOC in `src/` = 61,279 production + 44,674 inline test.
18,809 LOC in `tests/`. 2,332 test functions (2,107 `#[test]` + 225 `#[tokio::test]`, 41 `#[ignore]`).
12 MCP tools. 506 locked crates. 53 MB release binary. 273 commits over ~8 months.

**Method.** Nine agents audited in parallel with a mandate to be brutal and to measure, not infer.
Numbers below came from running the real binary over the real MCP protocol, querying the live
`.mdkb/index.sqlite` and `.mdkb/code.sqlite`, reading the live daemon log, mining 8,483 rows of
`hook-events.jsonl`, and lifting the resolution SQL into a temp view to reproduce its own asserted
tier distribution before trusting it. Two claims that decide the report were re-verified by hand.

## Re-measured — 2026-09-13 at `145fc48`

`scripts/remeasure-closing-review.py` read both live databases in read-only mode after the P1 fixes
and reproduced the current call-resolution cascade. All **32 of 32** active durable entries clear
the 0.3 confidence floor. The call graph resolves **14,061 edges into 21,061 candidate slots**, with
**0 non-callable slots**. Confident tier-1/2 call evidence suppresses **460 of 19,110** possible
unordered pairs across 196 indexed files (**2.4%**, down from the raw-name join's 27%).

The daemon measurement is necessarily a process/file-descriptor check rather than a SQLite query:
discarded stderr bytes are never stored in either database. The pre-fix daemon that was still alive
had advanced `/dev/null` to **191,156 bytes**. After rebuilding and `mdkb daemon restart`, fd 2 points
to `~/.mdkb/logs/daemon.log` and discarded stderr is **0 bytes**. The script reports `unavailable`
rather than inventing a number when `pgrep` or `lsof` is absent.

---

## Verdict in one paragraph

The engineering is better than the product. Craft indicators are strong: 5 clippy warnings across
61k production lines with `pedantic` on, zero SQL injection in a large dynamic-SQL layer, 29
justified `unsafe` blocks, a three-lock cross-process hierarchy with stated and tested ordering, and
comments that record the incident, the measurement and the rejected hypothesis behind nearly every
non-obvious decision. Against that: **the memory product — the reason mdkb exists — has never
actually run.** Automatic recall is off by default and, when on, is gated by a filter that its own
config documents as relevance and its code applies to age. Six months of the author's own daily use
produced 3 confirmations, 0 refutations, 0 prior injections, 0 reminders, 0 telemetry rows. The
code-intelligence half returns ≥49.5% false call targets with no confidence signal. And two
transaction-lifetime bugs can wedge the daemon's write lock permanently, on a path whose errors are
logged to `/dev/null` in every real installation.

The closing move is not more features. It is: **make failures visible, stop them destroying the one
non-rederivable store, delete what has no user, unify the paths that have diverged, and narrow every
promise to what the code actually delivers.**

---

## The two findings that decide everything

### F1 — Automatic recall filters on age, not relevance

`src/config.rs:573` documents `min_recall_score` as *"Minimum hybrid score for a recall result to be
injected."* Default 0.3 (`src/config.rs:620`).
`src/mcp/dispatch.rs:3701` passes it to `apply_min_confidence`, which
(`src/mcp/server.rs:2151`) filters on `e.confidence()` — belief × Ebbinghaus decay ×
source_authority — never on the hybrid score computed at `src/store/memory.rs:1281`.

With 0 confirmations (59 of 62 live entries), `confidence()` is a pure function of age:

| source_type | day 0 | crosses 0.3 | crosses warmup floor 0.25 |
|---|---|---|---|
| `user_statement` | 0.425 | day 31 | day 48 |
| `auto_extracted` | 0.350 | day 14 | day 30 |
| `official_docs` | 0.500 | day 45 | — |

**Measured: 6 of 39 active durable+prior entries clear the gate.** `caching-parser-tree-sitter` — a
real architectural decision — sits at 0.061. A perfect semantic match on it is retrieved and then
silently discarded for being old. No log, no counter.

`EntryType::is_durable()` (`src/store/memory.rs:290`) argues in its own doc comment that decisions
and problems do not rot. The confidence formula ignores it.

### F2 — `ROLLBACK TO` without `RELEASE` wedges the daemon's write lock forever

Four sites: `src/store/vectors.rs:236`, `:584`, `src/store/evolution.rs:152`,
`src/store/memory.rs:1886`. In each, `RELEASE` is on the `Ok` path and `ROLLBACK TO` on the error
path. `ROLLBACK TO` does not pop the savepoint and does not end the transaction — verified
empirically: `in_transaction` stays true, another connection gets `database is locked`.

Live trigger: `store_memory_embedding` runs on the daemon's long-lived connection outside any
transaction (`src/mcp/dispatch.rs:967`) and its error is swallowed with `tracing::warn!`. One failed
`vec_memory` insert ⇒ the savepoint stack never unwinds ⇒ the daemon holds a write transaction for
its whole life, `-wal` grows unbounded because no checkpoint can pass the open mark, and every
uncommitted write since is lost on restart. Self-perpetuating: the next `SAVEPOINT`/`RELEASE` pair
pops only the innermost same-named savepoint.

Same shape, different mechanism: `handle_session_index` (`src/core/sessions.rs:71`) opens
`BEGIN IMMEDIATE` and has no rollback on the `?` escapes at `:133`/`:146`. Its caller's comment
(`src/mcp/dispatch.rs:2218`) says *"Every failure here is a warning, not an error."*
`with_transaction` exists at `src/core/indexing.rs:36` to prevent exactly this and is not used.

**And none of it is observable.** `src/daemon/spawn.rs:80-90` spawns the daemon with `Stdio::null()`
on all three fds and without `--detach`, so `redirect_stdio_to_log()` never runs. Verified on the
live daemon: `2w CHR 3,2 0t4417 /dev/null` — 4,417 bytes of stderr already written and thrown away.
`daemon.log` only fills when someone runs `mdkb daemon restart` by hand.

---

## Findings by area

### A. Memory product semantics — over-engineered, and under-engineered where it counts

Live store: 62 entries (48 active, 11 archived, 3 superseded) — decision 14, problem 15, handoff 18,
prior 12, topic 3, reminder 0.

| mechanic | lifetime output |
|---|---|
| confirmations | 3 events on 3 entries |
| corrections / refutations | **0, ever** |
| prior injections | **0** (73 candidates, 58 clusters, 2 promoted) |
| memory graph edges | 3 rows, one timestamp |
| reminders created | **0** |
| `evolution` table | **0 rows** |
| `query_events` telemetry | **0 rows** (off by default) |
| `access_count` increments | 11, over six months |

- **Refutation is a lie in the tool surface.** `MemoryEntry` has no `corrections` field; the column's
  only writers are tests. Two production filters (`memory.rs:1712`, `memory_graph.rs:373`) are
  permanent no-ops. Worse, `confirm_entry(id, -1)` (`memory.rs:544`) does `MAX(0, confirmations-1)`
  **and** sets `last_confirmed_at = now` (`:569`) — on the 59 entries at 0 confirmations, refuting
  leaves belief unchanged and **resets the decay clock**. Refuting makes an entry more confident.
- **The priors pipeline (~2,300 lines) cannot deliver its output**, for three independent reasons:
  (a) `trigger_matches` (`priors.rs:614`) handles only `pre_tool` and `prompt`; live cluster kinds
  include `post_tool` 13, `repo` 10, `stop` 9 — **both promoted clusters are unmatchable kinds**;
  (b) `PRIOR_CONFIDENCE_GATE = 0.7` is mathematically unreachable for `auto_extracted` entries
  (multiplier 0.70, belief < 1.0) — and `priors.rs:76-78` says so in a comment, then builds a second
  parallel score rather than fixing the first; (c) both promoted clusters have decayed below the
  injectability floor anyway.
- **The miner learns the harness, not the codebase.** `is_correction`
  (`src/domain/prior_detect.rs:47`) fires on standalone `no`, `stop`, `wrong`, `instead` — and Boss
  writes in Italian, where `no` and `stop` are ordinary words. 9 of 58 clusters (16%) are the same
  lesson split apart because `canonical_trigger_key` hashes the LLM's free-text `when:` field; each
  is stuck at 1 session and cannot reach `PROMOTION_MIN_SESSIONS = 2`. The lesson in question is
  about the wiz budget hook. **Nothing ever deletes a candidate or a cluster** — the only
  `DELETE FROM prior_clusters` is in `#[cfg(test)]`.
- **`src/eval/` measures a tautology.** 12 queries lifted near-verbatim from the 12 documents they
  must retrieve (content *"…PKCE code_verifier and code_challenge"* / query *"pkce code verifier
  challenge"*). recall@5 = 1.000, MRR = 1.000, judge accuracy = 1.000. No headroom to detect a
  regression. Not in CI. And it bypasses production: BM25 only, fresh in-memory DB, no embeddings,
  no fusion, no re-rank, no filters. **There is no measurement of recall quality in this codebase.**
- **TTL is silently lost on every clone.** `to_markdown` (`memory_file.rs:223`) omits `expires_at`.
  On a fresh clone, import produces `expires_at: None`. `PRIOR_TTL_SECS` exists because *"a prior
  states what an agent did wrong once; that stops being true when the code changes"* — git-share it
  and it never stops being true. Fix: project `ttl_secs` (a merge-stable duration), not the absolute
  timestamp the comment rightly objects to.
- **`access_count` is not a usage signal but three systems treat it as one.** Only `get_entry` bumps
  it; search deliberately does not. 43 of 48 active entries are tied at 0, so warmup ordering — whose
  primary sort key it is — is effectively rowid order.
- **Scaling** (measured on a synthetic 100k-row store with the real schema):
  `newest_handoff_for_scope` 606 ms on the session-start path against a 200 ms budget (selects
  `content` for every active handoff, sorts by `updated_at` with no index);
  `list_projection_state` 235 ms + ~120 MB resident; `archive_expired` 40 ms full scan. No index on
  `updated_at`, `expires_at` or `due_at`.

**What genuinely works:** `src/core/memory_sync.rs` is the best-reasoned subsystem in the repo —
content-hash rather than mtime change detection (git restamps mtimes on checkout), whole-directory
classification before any mutation, a bulk-archive circuit breaker that git-proven deletions bypass,
conflict-marker quarantine, and a `gitignore_shadow` check that reports a blanket `.mdkb/` rule
rather than silently repairing a file mdkb does not own. Every one of those is a real incident fixed
properly and explained. The corpus itself is good: the 29 lines warmup emits are specific and useful.

### B. Code intelligence — the promise exceeds the delivery

Measured against the repo's own index (195 files, 7,098 symbols, 41,484 `Calls` edges, 100% Rust),
by reproducing the source's own asserted tier distribution first.

- **≥49.5% of what the call-graph API returns is false by construction.** 28,042 candidate slots for
  14,160 edges; a call site has exactly one true target. `get_called_functions`
  (`src/code/storage/sqlite.rs:681`) returns a flat `Vec<Symbol>` with **no tier**, so the caller
  cannot tell a tier-1 certainty from a 4.19-way guess. `get_call_targets` (`:700`) *does* classify
  — the right shape exists and is not used by the widely-called API.
- **One missing SQL predicate is 29.1% of it.** `sqlite.rs:1158` joins on `s.name = r.to_name` with
  no `s.kind` filter: 8,154 kept slots point at `Field` (6,443), `TypeAlias` (1,602), `Module` (95),
  `Constant` (14). `UsageMetrics.get` is a struct field that absorbs 552 slots because every `.get(`
  in the repo joins to it. A `Calls` edge never legitimately targets a field.
- **The receiver-type work — the most recent months — covers 1 of 13 languages.** Only
  `rust/parser.rs:823` populates `receiver_type`. The tier-1 and tier-2 receiver arms
  (`sqlite.rs:1103-1110`) are dead for the other twelve, and nothing in the docs says so.
- **`mdkb coupling` bypasses the cascade entirely** (`src/core/coupling.rs:212`): the raw bare-name
  join, no tier, no kind filter. **10,184 of 37,830 file pairs (27%) are suppressed by name
  coincidence**, biased toward exactly the generic names that carry no dependency.
- **All C++ `.h` headers are parsed with the C grammar** (`language.rs:30-48`), and there is **no
  `has_error()` check anywhere in `src/code`** — a failed parse is indistinguishable from a small file.
- **286 measured caller misattributions**: `symbol_lookup` is keyed on the *call site's* line, so the
  precise lookup misses whenever the call is not on the declaration line and a `name_in_file`
  fallback guesses the last same-named symbol. 17 `default` fns in `config.rs` collapse onto one.
- **`to_receiver_type` is never invalidated** (`UPDATE … WHERE to_receiver_type IS NULL`) and the
  unanimity rule makes resolution order-dependent: two indexes of the same tree can disagree.
- **Eight silent truncations** with no signal to the caller: 1 MiB file cap, 500 AST depth, 200-byte
  receiver expression (keeps the tail), 500-char embed text, 256-token dup bodies, 256-of-768
  embedding dims, 5-line/30-node dup minimums, 100-files-per-commit coupling cap. The first and last
  are the ones that change a user's conclusion: a generated file vanishing from the symbol table, a
  large refactor vanishing from coupling.
- **Duplication detection** is the right primitive (64-bit simhash over 3-shingles of AST node kinds,
  text discarded — rename-invariant) with a correct failure asymmetry (suppression fails open at
  `MAX_SUPPRESSING_TIER = 2`). But the Hamming threshold was retuned on one repo, the semantic pass's
  model and dimension choices rest on **nine hand-written toy cases** (`src/eval/embedding_gap.rs`),
  and `unclustered_pairs` materialises n² pairs — ~3.2 GB at 20k candidate bodies.

**What is genuinely good:** query-time resolution was the right architectural call — the graph is
always consistent with the current symbol table, removing a whole class of stale-edge bugs. The
parser layer is *properly factored*: the audit went looking for the 13-language copy-paste the brief
suspected and did not find it. `RESOLUTION_VERSION` invalidates through *both* change detectors.
And the duplication report's own comment states the bottom bucket is "about 2 in 6" true positives
rather than rounding up — someone chose to keep an inconvenient number.

### C. Runtime — solid foundations, sharp edges

| # | severity | finding | location |
|---|---|---|---|
| 1 | **P0** | auto-spawned daemon logs to `/dev/null`; `daemon.log` 16.5 MB, unrotated, last line 2026-08-30 | `daemon/spawn.rs:80`, `cli/daemon.rs:322` |
| 2 | P1 | a malformed `daemon.toml` makes **every hook exit 1** — reproduced; violates the stated "never fail-exit the host" contract | `cli/hook_client.rs:356` |
| 3 | P1 | flush timer reset by events it then ignores → the 30 s idle flush can starve for the whole of a `cargo build` | `mcp/server.rs:1537` |
| 4 | P1 | watcher channel (100) overflows in production — **9 episodes in 4 minutes in the live log**, each forcing a full rescan; recovery is starved by #3 | `watcher/mod.rs:73` |
| 5 | P1 | LRU eviction with outstanding `Arc` clones ⇒ **two watchers and two `Context` mutexes on one repo** (live log shows 5 distinct watcher tasks in one window) | `daemon/registry.rs:268` |
| 6 | P1 | distiller subprocess: no timeout, no single-flight, untracked at shutdown (orphaned on exit) | `domain/prior_distill.rs:283` |
| 7 | P2 | the 1.55 s spawn backoff sits **outside** the per-event budget it is supposed to respect; every `cargo build` retires the daemon, creating a ≥5 s degraded window | `cli/hook_client.rs:598` |
| 8 | P2 | no I/O timeouts, no cancellation anywhere; 64 idle connections wedge the accept loop *and* shutdown | `daemon/ipc_server.rs:398` |
| 9 | P3 | HTTPS: expiry comment claims a check the code does not do; private-key write/chmod TOCTOU | `mcp/https_server.rs:96` |

Measured hook latency over 8,483 live events: `session_start` p50 306 ms / p90 978 ms / **max 54.8 s**;
`pre_tool_use` p50 0 / p99 18 ms / **max 38.6 s**. The warm path is excellent; the tails are
user-visible freezes an agent cannot diagnose or escape.

**What is solid:** the `Unstarted`/`Undetermined` failure classification
(`cli/hook_client.rs:539`) — classifying by *"can the daemon have started writing?"* rather than by
where the call broke — is the best design in the codebase, and its eight tests test the reasoning.
`bind_socket_0600` binds under a pid-stamped staging name, chmods, then atomically renames, tested
against 200 racing rebinds. The two-tier shutdown drain separates a 600 s work grace from a 5 s
socket grace and asserts the ten-minute wait with a paused clock. Double-fork detach is done right,
with a comment showing the author knew why fork-with-threads is UB.

### D. Storage — mature across processes, immature within a connection

Beyond F2: `is_structurally_sound` (`store/heal.rs:113`) maps **any** SQLite error — `BUSY`,
`IOERR`, `NOMEM`, `CANTOPEN` — to "corrupt", and that answer renames the live database and creates
an empty one; only 5 tables are salvaged. The probe connections set **no `busy_timeout`**, so a
transient lock fails instantly. The sibling path (`code/storage/sqlite.rs:947`) matches specifically
on `DatabaseCorrupt | NotADatabase` and propagates the rest. **The database holding the only
non-rederivable data has the less careful classifier.**

Also: migrations delete/rename `.md` projection files *inside* the migration transaction
(`store/schema.rs:643`, `:806`) — a later failure rolls the rows back and leaves the files gone;
`EMBEDDING_DIM` is hardcoded as `FLOAT[384]` in three DDL sites and the dimension migration covers
1 of 3 vector tables; the `model` column is written in three places and **read nowhere**, so nothing
invalidates vectors when the embedding model changes; `code.sqlite` gets `busy_timeout` only as a
side effect of `run_repairs`, *after* all its DDL has already run at `busy_timeout = 0`.

**What is solid:** the three-lock hierarchy (`.writer` / `.mutation` / `.live`) with separated jobs,
a stated ordering rule and tests for it. `store/identity.rs` — recognising that every cross-process
guarantee is keyed on the db path *as a string*, that macOS firmlinks defeat `canonicalize`, and
that the answer is record-and-refuse rather than assume. `Context::open_read_only` enforcing
read-only through `SQLITE_OPEN_READ_ONLY` rather than promising it. "Probe on a throwaway
connection", because a long-lived pager reports a torn file as healthy. `tests/db_contention_multiprocess.rs`
states in its own header that the thread-based test *"has always passed while production stores kept
going corrupt"* and that routing *"does not PROVE the corruption fixed"*.

### E. Architecture — sound shape, damage concentrated in two files

Real dependency shape: `domain ← store ← core ← {cli, mcp, daemon}`, with `code` as a parallel stack.
But `mcp` references `store` **126** times against `core` **98** — the adapter is a second
application layer.

- **Two implementations of "write a memory entry", already diverged in production.**
  `core/memory.rs:23` (CLI) vs `mcp/dispatch.rs:764` (MCP). The CLI path: **a `prior` with no
  explicit `--ttl` never expires**, no near-duplicate rejection, no `on_conflict="contradicts"`, no
  relation edges, no session/agent provenance. Schema migration **v22 exists solely to retro-date
  priors written without a TTL** — and is scoped `AND source_type = 'auto_extracted'`, so a
  CLI-written `user_statement` prior stays permanent forever. A store-layer band-aid over a
  write-path divergence.
- **Two renderers for every mutation**, with string literals duplicated verbatim across 28 commands
  (`main.rs:262`/`:1849`, `:285`/`:1857`, `:289`/`:1863`, `:1239`/`:1970`); exactly one line was ever
  extracted to a shared helper. And **`tests/cli_smoke.rs` is not hermetic** — 134 calls with no env
  isolation, reaching for the developer's real daemon socket at `$HOME`, so the routed renderer is
  essentially never exercised.
- **Two routers over the same tool impls.** `memory_list` clamps to 200 in `dispatch_call`
  (`:4599`) and is **unbounded** through `server.rs:776` — an MCP client can request `limit=100000`
  on a server whose entire premise is the token budget. Three tools are parsed by hand with
  `params.get(...)` despite typed, schema-advertised structs existing.
- **A circular import** between `dispatch.rs` and `server.rs` blocks both of the above fixes, and
  `mcp/dispatch.rs:21` imports nine hook-classification functions from `cli::hook_logic` — one
  adapter importing application logic from another.
- **11 of 94 config knobs are dead**, several validated and round-trip-tested so they look alive.
  The worst is `[code] index_path`: the path is hardcoded at `core/code.rs:22`, and
  `config.rs:1467` *asserts it parses correctly* — proving only deserialization. A user who sets it
  believes the index moved.
- **`is_corruption` is dead on the path-scoped route.** `core/code.rs:99` does
  `.map_err(|e| anyhow!("Indexing '{p}' failed: {e}"))` — building from a `String` gives chain
  length 1, so the downcast in `code/indexing/mod.rs:992` can never fire. Both callers are live.
  The fix is `.context()` at five lines, ~1 hour.

**What is genuinely well-built:** `tests/dup_surface_parity.rs` runs the real binary *and* the real
MCP dispatch over one repo and asserts `assert_eq!(mcp, cli)` **byte for byte**, across four
scenarios including real git history. **It is the exact tool that would have caught the memory-write
divergence, the two renderers and the two routers** — it was built for `dup` and never applied
elsewhere. Alongside it, an executable tool-count budget test with the rationale in the failure
message. `Error::is_validation_refusal()` is the best-reasoned code in the repo: it explains why a
typed predicate beats string matching, names the only two qualifying variants, proves it by walking
every raise site, and drives a real decision about whether a write may be retried.

### F. Agent ergonomics (Claude Code / Codex)

Measured against the real binary over a real MCP handshake:
`initialize` 652 B; `tools/list` **17,326 B ≈ 4,331 tokens**; server instructions ≈ 495 tokens.
Descriptions are 1,250 B (7%); **JSON Schemas are 15,346 B (89%)**. `BASE_INSTRUCTIONS` has an
enforced 600-token budget test (`server.rs:2539`) — the block nine times larger has none. 43% of the
schema cost is `MemoryWriteBatchEntry` restating every field of `MemoryWriteParams`.

- **Every CLI error prints a Rust `Debug` struct.** `fn main() -> Result<()>` (`main.rs:53`) means
  first contact reads
  `Error { kind: DatabaseNotFound { path: "…/.mdkb/index.sqlite" }, backtrace: <disabled> }`.
  All 30 carefully written `#[error(...)]` Display strings in `src/error.rs` are discarded at the top
  level. Sharper: **the MCP path auto-initialises the DB while the CLI hard-fails** — same product,
  opposite policy, and the CLI is the one dumping a struct.
- **`memory_write`'s description contradicts its own schema.** `server.rs:671` says *"Types: problem,
  decision, topic"* (3); the schema and runtime accept 6 — including `handoff`, the type the entire
  session-handoff feature runs on. Same class: `search`'s description omits the `duplicates` scope
  entirely, so duplication detection is reachable over MCP only by an agent that reverse-engineers a
  scope value nothing documents.
- **In daemon mode the repo comes exclusively from MCP `roots/list`**, no cwd fallback, and the
  error — *"No repos registered. Waiting for MCP roots from client."* — tells the agent to wait for
  something it cannot influence while hiding the one thing that works (`root="/abs/path"`).
- **`scope` means two different things in two tools** (`search`: `docs|memory|code|symbols|duplicates`;
  `graph`: `doc|memory`), and the "what to look up" parameter is spelled four ways across the surface
  (`query` / `id` / `name` / `entity`) — CLI and MCP disagree on the noun for the same concept.
- **`mdkb cheatsheet`: 35% of its 9,142 bytes is one absolute path repeated 57 times**, and 16 of 32
  top-level commands appear nowhere in it — including `init` and `setup`, exactly what a first-contact
  agent needs. `tests/surface_parity.rs:134` tests only that there are no *phantom* commands; nothing
  asserts real ones are present, so this drift is invisible to CI by construction.
- **Codex parity is more real than the code's own hedging claims.** Verified on this machine
  (Codex CLI 0.154.0): `~/.codex/config.toml` has recorded trust state for all four mdkb hooks;
  Codex read the file, normalised mdkb's PascalCase to snake_case and accepted them. The blind spot
  is `trusted_hash` — `grep -rn "trusted_hash\|hooks.state" src/` returns **nothing**, and the
  installed `hooks.json` still carries the dead shell fallback removed in story 021-0636, so
  re-running setup changes every hook command and invalidates every pinned hash with no warning.
  **The unverified question that decides Codex MCP support entirely: does Codex reply to
  `roots/list`?** If not, every mdkb MCP tool is non-functional under Codex while
  `mdkb setup mcp codex` reports success. One manual session answers it.

**What is genuinely well-designed for agents** — these are worth copying:
symbol search annotates each hit with the file's token cost (`(file: ~41884tok)`), pricing the
agent's *next* action before it takes it; search results teach the batching pattern
(`get("621,436,7")`); enum rejections state the fact and the full valid set in 104 bytes;
`format_warmup_line` drops the `[type]` label when the id already starts with it, with *"every
warmup token is charged on every turn"* as the stated reason; SessionStart injection measures
**~504 tokens once per session** with a ~0-token steady state (`user_prompt_submit` fires on 0.29% of
1,368 invocations); and hooks degrade correctly — with the daemon pointed at a dead socket,
`mdkb hook session-start` still returns full context in-process.

### G. Dependencies, security, ops

- **`cargo audit`: 4 vulnerabilities.** `rmcp 0.14.0` is a **direct** dependency with an unpatched
  **HIGH (8.8)** DNS-rebinding vuln in the streamable-HTTP transport (RUSTSEC-2026-0189; fix needs
  ≥1.4.0, current crates.io is 3.3.0 — a breaking migration). Plus `quinn-proto` 7.5,
  `crossbeam-epoch`, `h2` — all reachable through fastembed's tree. `anyhow` itself is flagged
  unsound (RUSTSEC-2026-0190).
- **fastembed's cost is disproportionate**: ONNX Runtime, an **AV1 image codec**, and a full HTTP/3
  QUIC stack, to embed text locally. 506 locked crates, 36 with duplicate versions, 53 MB binary.
  Model download from HuggingFace into a machine-wide `~/.cache/fastembed/` with no integrity check
  in mdkb's own code.
- **CI is ubuntu-only** for a tool that releases 5 targets, and `CHANGES.md` admits Windows
  *"compiles"* but *"nothing here has been run on Windows"* — including the Unix-only `libc` daemon
  code. `cargo clippy` runs without `-D warnings` and without `--all-targets`.
- **Hygiene is genuinely strong**, and should be said plainly: `cargo clippy --all-targets` with
  `pedantic` enabled produces **5 warnings total**, all trivial. Zero SQL injection across a large
  dynamic-SQL layer — every `format!` near `execute`/`query` builds a placeholder list, a static
  column list or a hardcoded table name; the one place user text reaches FTS5 `MATCH` binds it and
  escapes the query syntax. Path traversal is guarded by a reusable canonicalize+`starts_with`
  pattern at the boundary. HTTP auth is constant-time (`subtle`), deny-by-default, loopback-bound,
  with a hard CLI error unless `--allow-no-auth` is explicit, and the TLS key is `0600`. All 29
  `unsafe` blocks are narrow and justified. **No telemetry, no phone-home.** The release pipeline
  blocks a mismatched or off-main tag, builds 5 targets, checksums every artifact.
- **The test suite is better than its headline number.** The claimed "~1150 tests" is stale: actual
  count is 2,332. **Zero mock-testing anywhere** — no `mockito`, no `MockServer`, no `assert!(true)`.
  All 13 language parsers have real inline tests. The ONNX-dependent tests are all `#[ignore]`d with
  explicit reasons, so a clean CI run does not hit the network for them. Real gaps: the
  self-admitted `just verify it returns Ok` test at `cli/hook_client.rs:857`, 19 `thread::sleep`
  assertions including a genuinely racy `sleep(150ms) + try_wait().is_none()` at
  `tests/sole_writer.rs:253`, no coverage tooling, and no integration test exercising 12 of the 13
  parsers through the CLI/MCP boundary.

---

## What the field says (external research, both agents)

**Keep the index. Do not "just give the agent ripgrep."** Agent Retrieval Bench (427 samples, 25
repos, 392k files) found logged agent trajectories **miss every gold file on 27–35% of samples** —
evidence that a pre-built index can help recover relevant files.

**Measure token efficiency alongside accuracy.** Evaluate answer quality, tokens consumed, and
tool calls together; retrieval accuracy alone does not measure the cost of finding an answer.

**Prioritize ranking under a token budget.** Rank tree-sitter symbols with personalized PageRank
and fit the result to the available context. mdkb extracts a call graph but does not use it for
ranking.

**Do not chase precise resolution formats.** `stack-graphs` was archived on 2025-09-09.
*Producing* SCIP requires bundling compiler-backed indexers and their toolchains — precisely the
cost tree-sitter avoids. *Importing* SCIP would provide a ground-truth oracle for measuring the
heuristics.

**Keep sqlite-vec.** Alternatives mean two storage engines in one binary and a split transaction
boundary, for a ceiling mdkb is nowhere near. (It shipped experimental ANN in a `0.1.10` alpha with
IVF disabled and unfinished docs — do not take it.)

**RRF is weaker than the folklore.** In the one head-to-head code benchmark found, Hybrid-RRF
(.487 MRR@10) **underperformed embedding-alone** (.518); reranking won (.528).

**Distractors are the dominant harm.** Topically related but non-answering content can harm
answer quality. Keep recall focused on evidence that answers the query. An
unqueried semantic warmup at session start is structurally a distractor generator. The one study on
memory injection into *coding* agents measured static hybrid retrieval at a **75% false-positive
rate on hard negatives**.

**MCP Tool Search changes mdkb's own cost model.** Claude Code auto-enables it once tool definitions
exceed ~10% of context; under deferred loading only tool *names* and server *instructions* enter
context — schemas do not. The project's own "every token is charged every turn" rule is now false for
parameter docs and doubly true for instructions.

**Tool annotations are free money.** `readOnlyHint` / `destructiveHint` / `idempotentHint` /
`openWorldHint` are unset on all 12 tools, so hosts must assume write/destructive/unsafe-to-retry —
and **Codex CLI drives approval friction off exactly these hints.** Hours of work; the best ratio in
either research report.

**The strategic risk is named and concrete.** Claude Code now ships **native auto memory**: four
typed note kinds, per-git-repo markdown, a `MEMORY.md` index loaded at every session start under an
**enforced 200-line / 25 KB cap**, topic files lazy-loaded, zero MCP overhead. What it does *not* do,
per Anthropic's own docs: **search, cross-machine, cross-repo, cross-tool (nothing for Codex), TTL,
confirm, graph, revisions.** That list is mdkb's entire remaining defensible surface, and the first
three are one release away. `autoMemoryDirectory` is configurable from any settings scope.

---

## Fable's verdicts (second opinion, independently grounded in the source)

**Q1 — delete the belief model, do not spend a month measuring it.** The "it never really ran"
counter-argument covers only automatic recall. Confirm/refute, reminders, evolution and telemetry
were wired and reachable in every session for six months and produced 3/0/0/0/0. Those are not broken
triggers; they are signals nobody produces. The structural reason: the model needs post-hoc
confirmation that a memory was useful, a coding agent does not produce that unprompted, and nothing
produces it on the agent's behalf. Without the loop, every Bayesian term collapses to "recency ×
source label" — the flat model, minus the age cliff. **What would change the verdict:** a month with
auto-recall on, the gate on hybrid score, **and** an automatic confirm path (e.g. a hook marking a
recalled entry confirmed when the agent's next action touches the file it cites). That third
condition does not exist today, so the month is not worth spending. *Keep* TTL for lifecycle entries
and `supersedes`/`refutes` edges; judge `condense` separately as corpus hygiene.

**Q2 — cut what has no user, consolidate what has one.** Delete: the belief model (~3,000 lines); the
HTTP/HTTPS transport (**which is also where the unpatched HIGH rmcp CVE lives** — deleting the
transport deletes the exposure *and* the breaking-migration pressure together); `src/eval` as it
stands (a benchmark scoring 1.000 on queries copied from the answers is worse than none, because it
will be cited); the 11 dead knobs, `[code] index_path` first; and the CLI's diverged memory-write
twin — not "merge", but pick the dispatch implementation and make the CLI call it through `core`.
**Do not cut code intel to rescue memory or vice versa** — the defensible shape is one shared corpus
served cheaply to two hosts, and both halves feed it. The 45k inline test LOC is not scope; it is
what makes a solo consolidation possible.

**Q3 — yes, serve Anthropic's `autoMemoryDirectory`. Not surrender; the only position that survives
the next release.** SessionStart injection is simultaneously where mdkb is structurally weakest and
where Anthropic owns the host. What Anthropic does not do — search, cross-repo, cross-machine, Codex
— is what mdkb's projection/sync layer already does, and that layer was *built* for "someone else
edits the markdown". Cross-machine is the gap Anthropic has no incentive to close. **The honest
cost:** after this, mdkb is an index and serving layer for Claude Code, not the writer of record. It
stays writer of record for Codex and for anything needing TTL/supersession/graph. Smaller product,
truer one. **Condition:** keep mdkb's own injection only when *queried*; the 29-line warmup the
auditor liked is corpus and survives, the unqueried semantic pass is the distractor and goes.

**Q4 — ceiling, and the bug list gets you there faster. Do both, then change the promise.** Bare-name
resolution across 13 languages without types cannot be made precise, and receiver typing cost months
for one language. Prioritize **ranking useful context under a token budget** and measure
resolution accuracy separately. New promise: `get_called_functions` returns
`(symbol, tier, is_unique)` and the tool description says tier ≤2 is resolved, tier ≥3 is a candidate
list to confirm with grep. **Do not narrow the language list** — the parser layer is well factored
and bare-name is a useful candidate generator everywhere. Narrow the promise, not the surface.

**Q5 — sequencing.** The deciding risk: the storage findings can lose data that cannot be
re-derived, and they are invisible because the daemon logs to `/dev/null`.

1. **Daemon observability** — stderr to `daemon.log`; hooks never exit 1 on a bad `daemon.toml`.
   ~½ day. *Nothing in steps 2–3 can be confirmed fixed in production without this.*
2. **Transaction lifetime** — `RELEASE` after every `ROLLBACK TO` (4 sites); close `BEGIN IMMEDIATE`
   on every `handle_session_index` error path; `is_structurally_sound` matches
   `DatabaseCorrupt | NotADatabase` only, like its code-index sibling. Add the "failed vec insert
   leaves no open txn" test. ~1 day.
3. **Unify memory-write** (CLI → `core` → the dispatch impl) and generalise `dup_surface_parity` over
   `memory_write` and `search`. **Do the belief-model deletion inside this step** — you are rewriting
   the write path anyway, and removing fields is cheaper than porting them. ~2–3 days.
4. **Deletions**: HTTP transport, dead knobs, `src/eval`; recall gate becomes hybrid-score-based.
   ~1–2 days, mostly tests. Archive with `wiz:yagni`.
5. **Code-intel bug list** (kind filter, surface the tier, route coupling through the cascade,
   `has_error()`, `.h` disambiguation), then **MCP tool annotations on all 12 tools** and the
   description fixes (6 types not 3, the `duplicates` scope, the `root=` hint in the roots error).
   Hours for the annotations — best ratio in the report.

**Deliberately left alone:** the `rmcp` upgrade for the HIGH CVE — a breaking migration across the
whole MCP layer to patch a transport Boss does not use and step 4 deletes. Delete first; upgrade when
there is a reason. Second: watcher overflow, the LRU double-watcher, and the missing timeouts — real,
but recoverable (the index rebuilds); after step 5.

**Immediately after the five:** a real retrieval eval — held-out queries written independently of the
documents, through the production path, with and without the embedding pass. Both researchers, the
memory auditor and Fable converge on this independently. Until it exists, every claim about whether
the vector pass helps is opinion.

---

## Cross-cutting root causes

| root cause | findings it produces | single fix |
|---|---|---|
| **Surfaces multiplied without a shared implementation** — 2 write paths, 2 renderers, 2 routers, 4 surfaces | diverged prior TTL (+ migration v22), unbounded `memory_list`, untested routed renderer, cheatsheet drift | one implementation per operation in `core`; generalise `dup_surface_parity` into the gate |
| **A documented guarantee the code does not deliver** | F1 (score vs confidence), `is_corruption` dead by `anyhow!`, `model` column never read, `[code] index_path`, HTTPS expiry comment, `memory_write` description | most of this audit was found *by* a doc comment stating a guarantee — make the comment executable, or delete it |
| **A quality signal nobody produces** | 0 refutations, 0 confirmations at scale, `access_count` = 11, 0 `query_events`, prior injections = 0 | either produce the signal automatically, or delete every mechanism that consumes it |
| **Silent truncation and swallowed errors at the boundary** | 8 code-intel truncations, the discarded `store_memory_embedding` error, `.ok()` turning corruption into "not found", quarantine-on-`BUSY` | a truncation that is not reported is a precision lie; report the count or do not truncate |
| **Heuristics promoted to the highest-confidence tier** | `X::y()` returns `X`, last-`let`-before-use, last-symbol-in-file caller attribution | a guess may generate candidates; it must never enter tier 1–2 |

---

## What is genuinely excellent (not a courtesy section)

1. **`tests/dup_surface_parity.rs`** — byte-for-byte equality between the real binary and real MCP
   dispatch. The answer to three of this review's HIGH findings, already written, applied once.
2. **`src/core/memory_sync.rs`** — content-hash change detection with the git-restamps-mtimes reason
   stated, whole-directory classification before mutation, a bulk-archive circuit breaker that
   git-proven deletions bypass, conflict-marker quarantine, `gitignore_shadow`.
3. **`Error::is_validation_refusal()` and the `Unstarted`/`Undetermined` split** — classifying
   failures by whether a write can have started, not by where the call broke, and proving the
   predicate by walking every raise site.
4. **`store/identity.rs`** — every cross-process guarantee is keyed on a *path string*, macOS
   firmlinks defeat `canonicalize`, therefore record-and-refuse rather than assume.
5. **`bind_socket_0600`** — pid-stamped staging name, chmod, atomic rename, tested against 200
   racing rebinds. The path is never observable wider than 0600.
6. **Query-time call resolution** — the graph is always consistent with the current symbol table.
7. **The parser layer** — genuinely shared machinery across 13 languages; the audit went looking for
   copy-paste and did not find it.
8. **Token-cost annotation on symbol search** (`file: ~41884tok`) — pricing the agent's next action
   before it takes it. The most agent-aware thing in the codebase.
9. **Honest negative results in comments** — the duplication report stating its bottom bucket is
   "about 2 in 6"; `db_contention_multiprocess.rs` stating the thread test "has always passed while
   production stores kept going corrupt"; `CHANGES.md` disclosing that Windows compiles but has never
   been run. A maintainer who records inconvenient numbers is why this audit could be quantitative.
10. **Executable budgets** — the 600-token instruction test and the 12-tool count test, each with its
    rationale in the failure message. A stated principle turned into a gate. Extend the pattern; do
    not abandon it.

---

## The one-line summary

mdkb is a well-built machine wrapped around a hypothesis that was never tested: that a Bayesian
belief model over engineering memory would beat "keep it until it's wrong, rank it by relevance."
Six months of the author's own data says it did not get the chance, and the mechanism that would have
given it the chance filters on age. Delete the hypothesis, keep the machine, narrow every promise to
what the code delivers, and measure retrieval for the first time.
