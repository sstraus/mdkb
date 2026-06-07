# Changelog

## Unreleased

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
