# Changelog

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
