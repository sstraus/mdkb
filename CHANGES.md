# Changelog

## 3.7.5 (2026-07-22)

### Fixed

- **Lifecycle hooks no longer leak context across repositories.** Prefer the
  host-provided event working directory over the hook subprocess directory, so
  a SessionStart in one repository cannot surface another repository's warmup
  or quarantine banner.
- **Corrupt code indexes recover automatically.** Validate `code.sqlite` before
  opening it, retain malformed databases and WAL sidecars under
  `.mdkb/quarantine/`, and rebuild the reproducible code index from source.
- **MCP stdio survives daemon restarts.** Keep the client transport open after
  a daemon socket disconnect, fail only requests that were in flight, and
  reconnect by replaying the initialization handshake before the next request.
  The proxy no longer leaves detached stdin tasks and zombie processes behind.
- **Hook memory writes accept documented comma-separated tags.** Normalize the
  CLI string into the JSON array required by the daemon instead of returning an
  `invalid type: string, expected a sequence` protocol error.

## 3.7.4 (2026-07-21)

### Fixed

- **Recurring SQLite corruption under concurrent daemon/CLI writes.** Upgrade
  bundled SQLite from 3.46.0 to 3.51.3, which contains the upstream fix for the
  WAL-reset corruption race affecting concurrent writers and checkpointers.
- **Codex `PreToolUse` context injection no longer fails validation.** Context-only
  hook responses now omit `permissionDecision`; Codex reserves `"allow"` for
  responses that also rewrite the tool call through `updatedInput`.

## 3.7.3 (2026-07-11)

Graph navigation & DX (stories 075–082) plus a P1 autoheal data-safety fix (083).

### Fixed

- **Autoheal no longer silently loses memory.** `memory_entries`/`memory_edges`
  live only in `index.sqlite`; on quarantine they are now salvaged into the fresh
  database via `ATTACH ... immutable=1` (a table that cannot be read logs the row
  count lost). The event is surfaced loudly and never silently: an enriched stderr
  warning at heal time, a persistent banner in `mdkb stats` while a `*.corrupt-*`
  file remains, and a SessionStart warmup line (even when the rebuilt store is
  empty). Post-heal now triggers a full docs + sessions + code rebuild, not just
  code. `search`/`get` on an empty store append an actionable "run `mdkb update`"
  hint so a blank result no longer reads as "nothing matched".
- **Graph output no longer leaks numeric doc ids.** `links`/`backlinks` render the
  source document's path (`people/x --owner--> repos/mdkb`) across CLI
  (text/json/csv/markdown) and MCP, resolved in one batched query.
- **`mdkb update` reports honest doc counts.** Output leads with
  `Docs: N indexed (X new, Y changed, Z removed)` so an unchanged re-run reads as
  `N indexed`, not the misleading code-index `Files discovered: 0`.

### Added

- **`neighbors` carries relation labels.** Each neighbor is annotated with the
  `via` relation(s) it was reached through — you see WHY nodes connect, not just
  THAT they do. No extra queries (labels come from the adjacency rows).
- **Collection-prefixed graph refs.** `graph links map/people/x.md` resolves like
  `people/x`; an unresolved ref enumerates the forms it tried.
- **`mdkb collection list`** — name, path, pattern, and document count per
  collection (`--format json` stable).
- **`mdkb graph dangling`** — references that resolve to no indexed document
  (with source doc + relation). Full-table scan, explicit command only.
- **`mdkb graph hubs [--relation R] [--limit N]`** — entities ranked by degree
  centrality with a per-relation breakdown. Full-table scan, explicit command only.

### Changed

- **Recall expansion caps are configurable.** `[graph] expand_seeds`,
  `expand_neighbors`, and `doc_neighbor_cap` move from hardcoded constants into
  `GraphConfig`, with defaults (2/3/3) that keep existing behavior byte-identical.

## 3.7.2 (2026-07-07)

### Fixed

- **`index.sqlite` pointer-map corruption.** Dropped the `mmap` + `auto_vacuum`
  combination that could corrupt the SQLite pointer map on the code index, and
  added an autoheal path that detects and rebuilds a corrupted index on open
  instead of failing the session.

## 3.7.1 (2026-07-07)

Full-codebase audit remediation (stories 055–070) plus warmup/handoff and parser
hardening. No schema break; existing DBs gain the new index on next open.

### Security

- **Daemon root whitelist is now default-deny in global mode.** An empty
  `whitelist_dirs` in `~/.mdkb/daemon.toml` no longer means allow-all; it now
  confines the daemon to the user's home directory. A client can no longer point
  the `--global` daemon at an arbitrary path to force `.mdkb/` creation, indexing,
  or a file watcher. Set `whitelist_dirs` to widen or narrow the allowed roots.
  Single-repo (non-global) local usage is unaffected — it never consults the
  whitelist.
- **MCP `source_file` confined to the repo root** and **HTTP transport now
  enforces authentication**, closing a path-traversal / unauthenticated-read gap
  on the MCP boundary.

### Performance

- **Query embeddings computed off the context lock.** The per-turn semantic
  search no longer holds the context mutex while running ONNX — the single
  highest-impact per-turn latency fix.
- **`idx_files_rel_path` kills O(n²) indexing.** `insert_file` runs a legacy
  cleanup `DELETE ... WHERE rel_path = ?` per file; with `rel_path` unindexed
  each was a full table scan, making a full reindex O(n²). The new index makes it
  a lookup. Added via `CREATE INDEX IF NOT EXISTS`, so existing DBs gain it on
  open. Also speeds `run_repairs`.
- **Incremental reindex re-embeds only changed symbols** instead of the whole
  file's symbol set.
- **Frontmatter regexes cached** (compiled once) and single-chunk doc embedding
  batched.

### Fixed

- **Honest code-index errors.** A failed update/reindex no longer silently wipes
  the index; worker threads are joined, and parser failures are logged instead of
  swallowed.
- **`busy_timeout` + WAL set in the production `Context::open` path**, removing
  the most common `SQLITE_BUSY` that previously triggered the silent wipe.
- **RAII reindex guard** so a panic mid-reindex can no longer wedge the MCP
  handle.
- **Bounded daemon memory** — stale `hook_dedup` sessions are evicted (TTL + LRU).
- **Watcher backpressure visibility + graceful shutdown drain** on the daemon.
- **`get_document_status` errors surfaced**, and the recall stale-dependency check
  batched.
- **CLI `get`** returns a correct exit code, scans the collection once, and runs
  on a one-shot current-thread runtime.
- **Warmup handoff injection.** mdkb now owns handoff injection: the newest
  handoff body is injected and handoffs are excluded from the compact list, with
  a cap on warmup handoffs and noise tags filtered from warmup lines.

### Changed

- **Recursion-depth guards threaded through all recursive parser walks** (31
  walks across the tree-sitter language backends) via shared helpers
  (`node_range`, visibility extraction, doc-comment strip), removing the last
  unbounded-recursion paths in parsing. Deleted the dead `domain/traits.rs`.



## 3.7.0 (2026-07-06)

### Changed

- **UserPromptSubmit recall is now opt-in by default.**
  `[hooks] user_prompt_submit_require_sigil` now defaults to `true`: mdkb injects
  context (recall, related docs, priors, call-graph hint) only for prompts that
  begin with `*`. The `*` is stripped before recall and stopwords are already
  dropped from the FTS query, so suggestions key off the meaningful prompt terms.
  Set `user_prompt_submit_require_sigil = false` to restore the always-on behavior.

### Added

- **Non-aggressive auto-indexing & embedding backfill.** mdkb now self-heals its
  memory embeddings and stops umbrella stores from re-scanning sub-repos, without
  the user running `mdkb update` by hand:
  - **Automatic embedding backfill.** Pending memory embeddings left by a
    cold-model `memory_write` now drain in the background on the next
    session-start and stop hooks (`spawn_embedding_backfill`) — single-flight per
    repo, gated on a cheap count, ONNX off the async runtime. The "N pending
    embeddings — run `mdkb update`" warning clears on its own.
  - **Nested-`.mdkb` boundary.** The index walk (both code and doc/collection
    scanning) no longer descends into a subdirectory that owns its own `.mdkb`
    store — a sub-repo indexes its own files, so an umbrella/parent store stops
    re-walking every child. An explicitly configured collection rooted in a
    sub-repo is still scanned (the walk root is exempt).
  - **Config-driven watcher tunables.** `[code.indexing] debounce_ms` (default
    raised 100→300) and `batch_idle_ms` (default 30000, unchanged — each flush
    re-embeds changed code, so it stays coalesced) are now settable in
    `.mdkb/config.toml`; the hardcoded literals are gone.

- **mdkb×wiz synergy audit — self-learning loop revived, token economy, retention
  (schema v16/v17).** Fixes the audit findings where the self-learning loop was
  effectively dead and search silently degraded to BM25:
  - **Embeddings on every write path.** CLI/bridge `memory add` and both import
    paths now embed like the MCP path; `mdkb update` backfills any entry missing
    an embedding. `mdkb update` also auto-embeds changed documents (`[search]
    auto_embed_docs`, default on; `claude_sessions` excluded unless
    `auto_embed_sessions`). `mdkb embed --collection <name>` embeds one collection
    explicitly. Pending-embedding counts surface in `mdkb stats`.
  - **`memory add --source-type`** (`official_docs|user_statement|inference|
    auto_extracted`, default `user_statement`, preserved on re-write) so
    synthesized entries stop being over-trusted. `update_entry` now persists
    `source_type`.
  - **Daemon-less `mdkb memory confirm <id> --outcome confirmed|refuted`** — the
    confirm loop is reachable on every transport; the UPS recall nudge points at
    this command.
  - **Warmup token economy.** SessionStart warmup strips YAML frontmatter from
    recall snippets, suppresses empty auto-handoffs (keeps the newest), applies a
    confidence floor (`warmup_min_confidence` 0.25) and a ~300-token budget
    (`warmup_token_budget`); `warmup_limit` 50→10.
  - **`claude_sessions` retention.** `mdkb update` archives transcripts whose
    source jsonl is gone (still searchable via `--collection claude_sessions`);
    `mdkb compact --prune-sessions --older-than <dur> [--export <dir>]`
    hard-deletes only archived transcripts, exporting markdown first.
  - **Hook-call telemetry.** Hook invocations are counted under a reserved
    `hooks` pseudo-session (schema v16 `sessions.agent`); opt-in `[telemetry]
    query_events` records per-recall metrics and NEVER the query text.
  - **Memory storage reconciliation (schema v17 `projected_at`).** `mdkb update`
    projects every DB entry to a markdown file (DB is the source of truth); a
    manually deleted, previously-projected file archives its entry.
  - **Setup drift detection & prior-mining visibility in `mdkb stats`** — warns on
    duplicated / missing (Stop) hook registrations; shows mining enabled/disabled
    with reason using the effective merged (daemon.toml < repo) priors.
  - **Housekeeping & log rotation.** `mdkb update` removes vestigial artifacts
    (0-byte `mdkb.sqlite`, legacy `code-index/`, writer-less `reindex-queue.jsonl`)
    and warns on dead `[models]` embedding keys (now removed); `hook-events.jsonl`
    / `hook-slow.jsonl` are halved (newest kept) past 1 MiB.

- **Memory graph — typed edges between memory entries (schema v14).** A new
  `memory_edges` table records typed relations (`supports`, `contradicts`,
  `supersedes`, `derived_from`, `relates_to`) from a memory entry to another
  memory or a document. Targets are dangling-tolerant and resolved at query time,
  mirroring the document graph.
  - `memory_write` accepts `relates=[{relation, target, target_kind}]` (max 10) —
    entry and edges are written in one transaction. A `supersedes` memory edge
    keeps the `superseded_by` scalar and `superseded` status in lockstep (single
    write path).
  - `graph(entity, direction="links"|"backlinks", scope="memory")` traverses the
    memory graph. CLI: `mdkb memory link <id> <relation> <target> [--doc]
    [--agent <name>]`; invalid relations are rejected listing the closed set.
  - `memory_write(on_conflict="contradicts")` records a near-duplicate conflict as
    a `contradicts` edge to the similar entry instead of rejecting the write
    (default behavior unchanged when omitted).
  - **Authorship provenance** — `memory_write` records the authoring session and
    optional `agent`; both surface in `get(id)`.
- **Post-recall 1-hop expansion.** A recalled entry's active memory neighbors are
  surfaced (≤2 seeds, ≤3 neighbors), annotated `(via <relation>)`;
  superseded/expired/dangling neighbors are excluded.
- **`[STALE-DEP]` marker.** At injection time (warmup + recall), an entry whose
  `derived_from`/`supports` dependency is superseded or net-refuted is prefixed
  `[STALE-DEP]`. Read-only — it never mutates stored confidence.
- **AI-distilled behavioral priors (schema v13).** Replaces the mechanical
  tool-chain "prior" miner with a recurrence-gated, trigger-matched subsystem
  owned by mdkb. New `prior_candidates`/`prior_clusters` tables; a write-time gate
  rejects mechanical tool-chain priors.
  - **Mining (opt-in, kill-switched).** A new `Stop` hook feeds the end-of-episode
    transcript to a cheap no-LLM candidate detector (error→fix→clean, or explicit
    user correction). Only flagged episodes are distilled — by an external agent
    CLI (`[priors].distiller_program`, prompt piped on stdin, run off the hook
    budget in a detached task) into strict JSON (falsifiable ≤160-char lesson,
    machine-matchable trigger, scope, evidence). Untrusted transcript evidence is
    secret-redacted before it leaves the process. Off by default
    (`[priors].mining_enabled=false`, and inert without a configured distiller).
  - **Recurrence gate + promotion.** A distilled prior is clustered by canonical
    trigger key; a cluster promotes to a `memory_entries` prior only after
    recurring across ≥2 distinct sessions. Injection scoring
    (`recurrence × freshness × belief`) is decoupled from per-entry source
    authority, so an honestly-tagged AI prior can finally surface.
  - **Trigger-matched injection.** Promoted priors surface at PreToolUse
    (tool / path-glob / command match) and UserPromptSubmit (prompt match) — never
    unconditionally at SessionStart. `[priors].injection_enabled` (on) and
    `max_injected_per_hook` (1) bound the per-turn cost; the PreToolUse path reads
    only an already-warm context so it never opens a DB on the hot path.

### Fixed

- **Data-safety guards on auto-run paths** (from the 2026-07-06 multi-agent
  review + GPT-5.5 triage — none of these had shipped):
  - **Bulk-archive circuit breaker.** `mdkb update`'s memory→file sync refuses to
    archive when more than 10 previously-projected entry files vanish in one pass
    (a `git checkout`/`stash`/`clean` or backup restore, not deliberate deletion),
    warning loudly instead of silently retiring the corpus. `mdkb update` now also
    prints archived / archive-skipped counts in its default output.
  - **Nested-store validation.** The `.mdkb` walker boundary requires an
    *initialized* store (`.mdkb/index.sqlite`); a bare or half-created `.mdkb`
    directory no longer makes the parent hard-delete every previously-indexed doc
    under it.
  - **`compact --prune-sessions --export` never loses the only copy.** A transcript
    whose content body is missing is skipped (not deleted), and export filenames
    are collision-proof (`{stem}-{id}-{hash8}.md`) so two sessions can't overwrite
    each other's export.
  - **Overflow-checked retention.** `--older-than` parsing and the prune cutoff use
    checked arithmetic, so an oversized value is rejected rather than wrapping to a
    cutoff that over-deletes.
  - **Backfill no longer stalls on a poison row.** A single un-embeddable memory
    entry is skipped; only a cold model pauses the batch (previously one bad row
    starved every later entry).
- **`[search] auto_embed_memory`** (default on) — kill switch for embed-on-write on
  `memory add` / `memory import`; off leaves entries pending for `mdkb update`.
- **Performance.** Auto-embed / memory backfill / session indexing run off the
  async runtime via `spawn_blocking` (no longer holding the repo lock across ONNX
  work); the doc-embed pass replaces a per-document `has_embedding` query with one
  set lookup; new partial index `idx_sessions_agent` for the per-hook session
  lookup.

## 3.4.0 (2026-06-09)

### Added

- **Knowledge graph — typed edges from frontmatter + wikilinks.** A new `edges`
  table (schema v11) records typed relations from a document to entity slugs,
  derived during indexing from allowlisted frontmatter keys (strong) and body
  `[[wikilinks]]` (soft). Targets are stored verbatim and resolved to documents
  at query time, so cross-document links survive regardless of indexing order
  (dangling edges resolve once their target is indexed). Re-indexing replaces a
  document's outgoing edges idempotently.
  - CLI: `mdkb graph links <entity> [--relation T]` (outgoing),
    `mdkb graph backlinks <entity> [--relation T]` (incoming),
    `mdkb graph neighbors <entity> [--relation T] [--depth N]` (adjacent,
    undirected), and `mdkb graph path <a> <b> [--max-hops N]` (shortest path) —
    all honoring `--format json|text|csv|markdown`.
  - MCP: a single consolidated `graph` tool with
    `direction=links|backlinks|neighbors|path` (mirrors `code_graph`), keeping
    the always-on tool surface minimal.
  - Config: a `[graph]` section (`enabled`, `frontmatter_relations`,
    `include_wikilinks`) written into the default template by `init`.
- **`mdkb update --force`** reindexes every file regardless of modification time.
  Without it, `update` is mtime-incremental, so config changes (e.g.
  `graph.frontmatter_relations` or `include_wikilinks`) only reach documents
  that are subsequently edited; `--force` reapplies them to the whole index.

### Fixed

- **CLI memory-write upserts instead of failing** — `mdkb memory add` (and the
  bridge `memory-write` path) now updates an existing entry in place — saving a
  revision — rather than crashing with `UNIQUE constraint failed:
  memory_entries.id`. Matches the MCP `memory_write` behavior.
- **`setup hooks` replaces legacy untagged entries** — re-running hook setup
  removes prior `mdkb hook <event>` entries that predate the `_managedBy: mdkb`
  tag, instead of leaving a duplicate that fires mdkb twice.
- **`setup mcp claude` heals stale registrations** — it now removes an existing
  registration at the target scope before adding, so a legacy `mdkb serve`
  command is replaced by the `mdkb mcp` proxy instead of being reported as
  "already registered" and left untouched.

## 3.3.0 (2026-06-07)

### Added

- **PreToolUse redirects Bash `grep`/`rg` to mdkb** — the hook now intercepts
  `Bash` commands, not just the rarely-used `Grep` tool. Agents search code
  through `Bash` far more than the `Grep` tool, so this is where the redirect
  actually reaches them. The shell command is parsed quote-aware; only the
  source stage of a pipeline is considered (a `… | grep x` stdout filter is
  left alone), and bare `grep PATTERN` (stdin), single-file greps, and
  regex/alternation patterns are left to grep. `sh|bash|zsh -lc "…"` wrappers
  (used by Codex) are unwrapped first.
- **Redirect conversion telemetry** — a new `mdkb_invocation` hook outcome
  records when a `Bash` command actually runs mdkb. `mdkb stats` shows a `Conv`
  column per hook event so the PreToolUse redirect's hit rate is measurable.

### Changed

- **Slimmed MCP server instructions** — dropped the code-search syntax table
  that duplicated the tool JSON Schema. Kept the semantic-vs-literal routing,
  memory guidance, and reminder protocol. Fewer always-injected tokens per
  session.

## 3.2.0 (2026-06-03)

### Added

- **Automatic incremental `auto_vacuum` reclaim** — the maintenance pass now
  runs incremental `auto_vacuum` so `index.sqlite` releases freed pages instead
  of growing unbounded after deletes/reindexes.
- **Git worktrees share the main repo's `.mdkb/`** — secondary worktrees no
  longer get an isolated database; memory and index written in one worktree are
  visible from the others.
- **`symbols_in_file` and `symbol_at_position` MCP tools** — list the symbols
  defined in a file, or resolve the symbol at a `line:col` position.

### Changed

- **MCP registration routes through the daemon proxy** — `mdkb setup` now
  registers the server via the daemon proxy command instead of a direct binary
  invocation.
- **Instructions clarify mdkb is semantic search, not literal matching** — the
  server instructions and tool text state that exact strings, substrings, and
  regex belong to Grep, not mdkb.

### Fixed

- **Watcher bootstraps code index on startup** — in daemon/global mode, the
  file watcher now runs a full `index_directory` when `code.sqlite` is empty
  (file_count == 0). Previously, repos opened via the daemon had 0 symbols
  until a file change triggered the incremental watcher. Mirrors the standalone
  startup task behavior.
- **Standalone startup respects `code.enabled`** — the background code reindex
  task now checks `code.enabled` before indexing. Previously it always ran,
  ignoring the config flag that the CLI `init` path honored.
- **Watcher receives `respect_gitignore` config** — the file watcher now
  creates its `PipelineConfig` with the correct `respect_gitignore` setting
  from `code.indexing.respect_gitignore`, instead of relying on the default.
- **Hidden directories excluded from code index** — directories starting with
  `.` (`.git/`, `.vscode/`, `.idea/`, etc.) are now skipped by the file walker.
  Previously `hidden(false)` let the walker enter hidden directories, relying
  on `.gitignore` to filter them — which failed when `respect_gitignore` was
  false. Use `# mdkb:index` in `.gitignore` to force-include files inside
  hidden directories.
- **`_root` collection no longer recursively duplicates docs** — indexing the
  repo root stopped re-adding the same documents on each pass.
- **Duplicate `rel_path` entries prevented in the code index** — plus
  previously-silent repair failures are now surfaced.
- **Race between `ensure_context` and the `doc_reindex_active` flag eliminated.**

## 3.1.0 (2026-05-01)

### Added

- **Automatic code.sqlite repair on open** — idempotent integrity checks run
  every time the code index is opened. Detects and fixes: NULL kind rows,
  orphaned symbols (missing file), orphaned relationships (missing file or
  symbol), and desynced FTS5 index. Fixes are reported to stderr; clean
  databases have zero overhead beyond the integrity check queries.
  New module: `code::storage::repair`.

### Changed

- **Stats report opens code.sqlite read-write** — enables autofix on
  `mdkb stats` instead of silently logging a WARN nobody reads. Falls back
  to read-only if write access is unavailable.

## 3.0.3 (2026-04-26)

### Added

- **`handoff` entry type** — session handover entries for agent context
  transfer. No default TTL (use `--ttl` to set one). Handoffs are project
  history — confidence decay handles relevance naturally.
- **`--file <path>` on `memory add`** — reads content from a file instead
  of `--content` or stdin. Saves token overhead when agents write handoffs
  to the filesystem and want to register them in mdkb. Mutually exclusive
  with `--content`.
- **`source_file` on MCP `memory_write` / `memory_write_batch`** — server-side
  file read. The model passes only the path; mdkb reads the content. Mutually
  exclusive with `content`.
- **Source path metadata** — the file path is persisted in `source_path` and
  displayed in `memory show` (text and markdown formats).
- **Memory subcommand aliases** — hidden aliases for commands models commonly
  guess: `write`/`create` → `add`, `get` → `show`, `delete` → `rm`.

## 3.0.2 (2026-04-26)

### Fixed

- **`setup mcp claude/codex` registers `mdkb mcp` instead of `mdkb serve`** —
  the old registration spawned standalone server processes per Claude session,
  bypassing the singleton daemon. Now correctly proxies through the daemon.

## 3.0.0 (2026-04-25)

### Breaking Changes

- **Hook dispatch via daemon IPC** — all hook events (`session-start`,
  `user-prompt-submit`, `pre-tool-use`, `post-tool-use`) now dispatch
  through the daemon's Unix socket instead of running in-process. The CLI
  `mdkb hook <event>` connects to the daemon, auto-spawning it if needed,
  with exponential backoff. Falls back to in-process (`MDKB_NO_DAEMON=1`)
  if the daemon is unreachable.
- **`reindex-queue.jsonl` removed** — `PostToolUse` no longer appends to a
  file. Edited paths are sent directly to the daemon's watcher channel via
  `reindex_tx`, triggering immediate reindex. Any tooling that read or
  monitored `reindex-queue.jsonl` must be updated.
- **`hooks.rs` deleted** — the monolithic hook handler is replaced by
  `hook_logic.rs` (pure functions) + `hook_client.rs` (IPC client) +
  `dispatch.rs` (4 hook methods in the daemon dispatch layer).

### Added

- **Hook event logging** — every hook invocation is logged to
  `.mdkb/hook-events.jsonl` with event name, outcome (ok/empty/error),
  elapsed time, and latency budget. Replaces the old `hook-slow.jsonl`
  which only logged overruns.
- **Per-event configurable thresholds** — `latency_budget_ms` can now be
  set per event type in `[hooks]` config.
- **`mdkb hook` one-shot IPC client** — `mdkb hook <event>` reads stdin,
  sends a JSON-RPC call to the daemon socket, and prints the response. No
  in-process DB access on the primary path.
- **Agent DX CLI Scale** — imported evaluation rubric at
  `.agents/skills/agent-dx-cli-scale/SKILL.md` for scoring CLI
  agent-friendliness.

### Changed

- **`spawn_blocking` for CPU-bound hook work** — FTS tokenization and
  pattern classification moved to `tokio::task::spawn_blocking` to avoid
  blocking the async runtime.
- **Safe JSON serialization** — hook responses use checked serialization
  with fallback to `{}` on failure, preventing malformed output.

## 2.2.1 (2026-04-21)

### Changed

- **Silent hooks** — hooks that have nothing to report now produce no stdout
  output instead of an empty JSON object. Reduces noise for the host CLI.
- **`emit_response` graceful error handling** — serialization failures are
  logged to stderr instead of emitting a fallback `{}`.
- **File watcher ready signal** — `run_file_watcher_inner` accepts an optional
  `Notify` to signal readiness, replacing sleep-based synchronization in tests.
- **Watcher test determinism** — `e2e_daemon_watcher` uses `Notify`-based
  readiness instead of `sleep(500ms)`, eliminating flaky timing.

## 2.2.0 (2026-04-20)

### Added

- **`prior` entry type** — behavioral pattern entries for external analyzers
  (e.g., HUD stop hooks). 30-day default TTL. Excluded from all default
  searches; query with `--entry-type prior` or `search(scope="memory",
  entry_type="prior")` via MCP.
- **`mdkb cheatsheet`** — AI-friendly compact command reference with full
  binary paths via `current_exe()`. Eliminates trial-and-error CLI discovery.
- **`--entry-type` filter on `mdkb search`** — filter memory searches by
  entry type (topic, problem, decision, reminder, prior).
- **PreToolUse Grep interceptor suggests CLI commands** — works without MCP.
  Classifies Grep patterns (pure identifiers, definition searches, callsite
  patterns) and suggests `mdkb search`/`mdkb code` via Bash.
- **`mdkb setup remove`** — CLI removal of MCP and hook registrations.
  `setup remove mcp claude|codex`, `setup remove hooks claude|codex`,
  `setup remove claude --scope local|user` (MCP + hooks in one shot).

### Changed

- **Hook suggestions use CLI instead of MCP tool names** — `current_exe()`
  resolves the binary path dynamically. No daemon socket check required.
- **Optimized injected text** — ~185 fewer tokens per turn across
  BASE_INSTRUCTIONS, PreToolUse messages, and SessionStart tip.
- **SessionStart tip points to `mdkb cheatsheet`** instead of inline syntax.
- **Removed duplicated entry_type/ttl docs from BASE_INSTRUCTIONS** — already
  in JSON Schema `/// doc` comments.

## 2.0.0 (2026-04-18)

### Breaking Changes

- **`mdkb status` removed** — use `mdkb stats` instead. The old command
  prints an "unknown command" error from clap. No alias is provided.
- **`mdkb stats` signature changed** — `--sessions` / `--aggregate` flags
  removed. The command now accepts `--no-color` and `--format json|text`.

### Added

- **`mdkb memory export`** — dumps all memory entries to a folder of
  per-entry `.md` files with YAML frontmatter. Options: `--dir`,
  `--include-expired`, `--overwrite`, `--dry-run`. Default directory:
  `.mdkb/memory/entries/`.
- **`mdkb memory import` (directory mode)** — auto-detects whether the
  path argument is a directory; if so, scans `*.md` files and imports
  via the new `memory_file` YAML parser. JSON file path unchanged.
- **`mdkb stats` unified ASCII dashboard** — replaces both `mdkb status`
  and the old session-only `mdkb stats`. Sections: index health
  (document/memory counts, free-page ratio), collections table, memory
  bar-by-type with reminder due/upcoming counts, code symbols per language
  (when code.sqlite is present), session totals with top-tools bar chart,
  hooks slow events and reindex-queue pending count. Uses box-drawing
  characters and block-element bar charts. `--format json` serializes
  the full `StatsReport` struct.

### Internals

- `src/cli/memory_file.rs` — hand-written YAML frontmatter serializer
  and `gray_matter`-based parser for `MemoryEntry`. Round-trip preserves
  all authored fields; derived counters (`access_count`, `last_accessed`,
  `confirmations`) are reset on import.
- `src/cli/stats_render.rs` — `bar`, `sparkline`, `frame` ASCII primitives
  and a hand-rolled ANSI `style` module (no `owo-colors` dependency).
- `src/cli/stats_report.rs` — `collect_report` aggregator.
- `src/cli/stats_render_report.rs` — ASCII renderer for `StatsReport`.
- `src/store/memory.rs` — added `list_entries_all` (no expiry filter,
  used by export to include expired entries when requested).

## 1.5.0 (2026-04-18)

### Added

- **Lifecycle hook dispatcher** — `mdkb hook <event>` handles `session-start`, `user-prompt-submit`, and `post-tool-use`. SessionStart injects a `## mdkb memory warmup` block; UserPromptSubmit injects `## mdkb: relevant context` via an FTS5 OR query over the prompt tokens; PostToolUse appends edited paths to `.mdkb/reindex-queue.jsonl` so the next `mdkb update` pass picks them up. See `docs/hooks.md`.
- **Hook registration commands** — `mdkb setup hooks claude --scope local|user [--disable …] [--dry-run]` writes `.claude/settings.local.json` or `~/.claude/settings.json`; `mdkb setup hooks codex` writes `~/.codex/hooks.json`. Idempotent re-runs; preserves unrelated settings.
- **`mdkb setup mcp codex`** — registers mdkb in `~/.codex/config.toml` under `[mcp_servers.mdkb]` using `toml_edit` to preserve comments and formatting. Dry-run prints the merged config without writing. (#023)
- **`.mdkbignore-hooks` opt-out marker** — empty file at repo root suppresses all three hooks; ancestor lookup stops at `$HOME`.
- **`[hooks]` config section** — per-event enable toggles, `recall_limit`, `latency_budget_ms`, `min_recall_score`. Slow hooks log to `.mdkb/hook-slow.jsonl`.
- **`usage` MCP tool** — reports per-tool call counts and recent activity for the current session. (#019)
- **Memory confidence & access counters** — search ranks memories by `access_count × recency` as a third RRF signal (weight configurable via `[search.memory] access_recency_weight`); `get` is the only writer of `access_count` so `search` stays SELECT-idempotent. (#025–#027)
- **File token estimates in code index** — `files.token_count` populated from `cl100k_base`; surfaced via `search(scope="symbols")` and `get`. (#020)
- **Auto-optimize on drift** — startup VACUUM when free-page ratio > threshold, runtime `PRAGMA optimize` every `db.optimize_interval_calls` tool calls. (#028)

### Changed

- **E2E hook contract covered** — `tests/e2e_hooks.rs` spawns the real binary and verifies SessionStart warmup, UserPromptSubmit recall, PostToolUse queue, and `.mdkbignore-hooks` suppression. (#021-0ad9)

## 1.4.0 (2026-04-17)

### Added

- **Reminder entry type** — `memory_write(entry_type="reminder", due_in=<seconds>)` creates a time-bound memory entry. Future reminders (`due_at > now`) are hidden from `memory_list`, `search(scope="memory")`, and active-count stats. Once `due_at <= now`, the reminder surfaces in the warmup index with a `[reminder:DUE] {id}: {title}` prefix so the MCP client sees it on the next turn.
- **Reminder confirmation protocol in BASE_INSTRUCTIONS** — CC is instructed to ask the user before deleting a due reminder and to re-ask on ambiguous replies, preventing accidental deletion from incidental topic mentions.
- **Schema migration v9 → v10** — adds `due_at INTEGER NULL` column; non-destructive on existing DBs.
- **CLI support** — `mdkb memory add <id> --entry-type reminder --due-in <seconds> --title "..." --content "..."`.
- **Input hardening** — memory titles and tags now reject newlines and control characters to prevent prompt-injection via instruction-surface fields.

### Changed

- **BASE_INSTRUCTIONS rewritten** — tighter wording (token budget < 600 for empty-index), English-only affirmatives section, documented `memory_write` signature inline, `code_graph` direction values listed, Reminders protocol added as a numbered 4-step flow.

## 1.2.0 (2026-04-08)

### Fixed

- **Code index: duplicate symbols crash** — `UNIQUE constraint failed` on JS/TS files with same-line redeclarations (e.g., minified code, `var` re-declarations). Changed `INSERT` to `INSERT OR REPLACE` in symbol storage.
- **Startup reindex silent failure** — the above crash was logged but silently ignored, leaving the code index stale after server restart.

### Added

- **Shebang language detection** — extensionless scripts with shebangs (`#!/usr/bin/env node`, `#!/usr/bin/python3`, etc.) are now detected and indexed as their respective languages.
- **Semantic code search enabled by default** — `code.semantic_search.enabled` defaults to `true`. Embedding-based code search (`scope="code"`) works out of the box.

### Changed

- **MCP instructions rewritten** — removed "always use mdkb search before Grep" rule. New instructions clarify when to use mdkb (semantic queries, code_graph, memory) vs Grep (exact pattern matching). `code_graph` promoted to a primary workflow step.
