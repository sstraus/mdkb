<p align="center">
  <img src="https://raw.githubusercontent.com/sstraus/mdkb/main/assets/mdkb.png" alt="mdkb logo" width="420">
</p>

# mdkb

**Local memory, search, and code intelligence — integrated with Claude Code and Codex via CLI, lifecycle hooks, and MCP.**

mdkb indexes your project's docs, source code, and persistent knowledge into a local hybrid search engine — then exposes it to Claude Code, Codex, or any MCP client so the AI finds what it needs instead of guessing.

No cloud APIs. No token-heavy context dumps. Just fast, local, relevant retrieval.

## Why mdkb

- **Persistent across sessions, not a snapshot** — decisions, problems, and behavioral `prior`s carry over between conversations, with Bayesian confidence that decays over time and is reinforced on access. The knowledge graph captures *what your project is now*; mdkb also remembers *what you learned and decided*.
- **Proactive — no tool call required** — relevant context is injected automatically at session start, on each prompt, and before code searches via lifecycle hooks. Value doesn't depend on the AI remembering to query a tool.
- **Fully local, nothing leaves the machine** — no cloud APIs, no query logging, SQLite on disk, relative paths only. Safe for private and regulated repos.
- **Code intelligence is a graph, not just text** — tree-sitter call graphs answer callers / callees / impact across 13 languages, deterministically and offline.
- **One Rust binary, zero config** — auto-indexes on startup, watches for changes, self-repairs the index. No runtime, no daemon to babysit.

## What it does

- **Hybrid search** — BM25 + semantic vectors over your markdown docs
- **Code intelligence** — tree-sitter parsing for 13 languages, call graphs, symbol search
- **Persistent memory** — AI-created knowledge entries that survive across sessions, including time-bound `reminder` entries with due-date surfacing and `prior` entries for behavioral patterns (30-day TTL default)
- **Lifecycle hooks** — proactive context injection and reindex enqueue via Claude Code / Codex CLI hooks (no tool call required)
- **Markdown-native memory** — export/import memory entries as a folder of `.md` files for review, git tracking, or bulk edit
- **Unified diagnostics** — `mdkb stats` renders a static ASCII dashboard (index health, collections, memory, code, sessions, hooks)
- **Zero config serving** — auto-indexes on startup, watches for file changes, auto-`VACUUM`s on drift

### Recent highlights (3.7.x / 3.1.0 / 3.0.0)

Full details in [CHANGES.md](CHANGES.md).

- **3.7.2** — Fixed `index.sqlite` pointer-map corruption (dropped `mmap`+`auto_vacuum`); autoheal rebuilds a corrupted code index on open instead of failing the session.
- **3.7.1** — Full-codebase audit remediation: default-deny daemon whitelist in global mode, `source_file` confinement + HTTP auth on the MCP boundary, query embeddings off the context lock, `idx_files_rel_path` (kills O(n²) reindex), incremental re-embed of only changed symbols, honest code-index errors (no silent wipe), and recursion-depth guards across all parser walks.
- **3.7.0** — UserPromptSubmit recall is opt-in by default (`*` sigil); non-aggressive auto-indexing + embedding backfill; mdkb×wiz synergy audit (schema v16/v17) revives the self-learning loop with embeddings on every write path.
- **3.1.0** — Automatic `code.sqlite` repair on open — idempotent integrity checks fix NULL kinds, orphaned rows, and desynced FTS without user intervention.
- **3.0.0 (breaking)** — Hook dispatch via daemon IPC (Unix socket JSON-RPC instead of in-process execution); `reindex-queue.jsonl` removed (PostToolUse sends paths directly to daemon watcher channel); hook event logging to `hook-events.jsonl`; per-event configurable latency thresholds; `spawn_blocking` for CPU-bound hook work.
- **2.2.0** — `prior` entry type for behavioral patterns (30d TTL default, excluded from searches); `mdkb cheatsheet` AI-friendly command reference; `--entry-type` filter on `mdkb search`; PreToolUse Grep hook suggests CLI commands (works without MCP); optimized injected text (~185 fewer tokens per turn).
- **2.0.0 (breaking)** — `mdkb status` removed (use `mdkb stats`); `mdkb memory export`/`import` round-trip entries as `.md` files with YAML frontmatter; unified ASCII stats dashboard with `--format json` and `--no-color`.
- **1.4.0** — `reminder` entry type with `due_in` (surfaced in session warmup once due); schema migration v9 → v10; input hardening (reject control chars in titles/tags).

## Installation

### Homebrew (macOS/Linux)

```bash
brew install sstraus/tap/mdkb
```

### From source

```bash
cargo install --path .
```

### Pre-built binaries

Download from [Releases](https://github.com/sstraus/mdkb/releases) — macOS (arm64/x64), Linux (arm64/x64), Windows (x64).

## Quick Start

```bash
cd your-project
mdkb init
mdkb collection add docs ./docs
mdkb update
```

### Connect to Claude Code

```bash
# Project-scoped (recommended)
mdkb setup mcp claude --scope local

# Or user-scoped (global)
mdkb setup mcp claude --scope user
```

Restart Claude Code after setup. The MCP server auto-indexes on startup and watches for file changes.

### Hooks (optional, recommended)

MCP gives the assistant tools; hooks make it use them. Hooks also work standalone without MCP — the `PreToolUse` Grep interceptor suggests CLI commands via `current_exe()`, and `SessionStart` points to `mdkb cheatsheet` for the full command reference.

Register the lifecycle dispatcher so Claude gets a memory warmup at session start, relevant context on every prompt, and Grep-to-mdkb suggestions — without having to call `search` first:

```bash
# Claude Code, project-scoped (writes .claude/settings.local.json)
mdkb setup hooks claude --scope local

# Claude Code, user-scoped / global (writes ~/.claude/settings.json)
mdkb setup hooks claude --scope user

# Codex CLI (writes ~/.codex/hooks.json)
mdkb setup hooks codex

# Preview the merged settings JSON without writing
mdkb setup hooks claude --scope local --dry-run

# Disable specific events at install time
mdkb setup hooks claude --disable post-tool-use
mdkb setup hooks claude --disable user-prompt-submit,post-tool-use
```

Restart the host CLI after setup. Re-running is idempotent: existing hook entries are replaced, unrelated settings preserved. Events: `session-start`, `user-prompt-submit`, `pre-tool-use` (Grep interceptor), `post-tool-use`. Full contract, config, and opt-out in [docs/hooks.md](docs/hooks.md).

### Binary path caveat

`mdkb setup mcp …` and `mdkb setup hooks …` hard-code the absolute path of the binary that ran the setup. If you later move or rebuild the binary, the recorded command breaks. For stable global installs, first run `cargo install --path .` (binary lands in `~/.cargo/bin/mdkb`), then run setup from that binary.

For local development builds that back active MCP/hooks, prefer:

```bash
scripts/local-release.sh
```

It builds `target/release/mdkb`, stops stale `mdkb mcp` processes, restarts the daemon, and reports which process holds the rebuilt binary.

### Uninstalling

```bash
# Remove all Claude Code registrations (MCP + hooks)
mdkb setup remove claude --scope local   # per-project
mdkb setup remove claude --scope user    # global

# Remove individually
mdkb setup remove mcp claude --scope local
mdkb setup remove mcp codex
mdkb setup remove hooks claude --scope local
mdkb setup remove hooks codex
```

Soft alternatives before uninstalling: create an empty `.mdkbignore-hooks` marker at the repo root to silence hooks for that working tree, or toggle `session_start_enabled` / `user_prompt_submit_enabled` / `post_tool_use_enabled` in `.mdkb/config.toml`.

### Manual MCP Setup

Add to your Claude Code MCP config (`.claude/mcp.json` or `~/.claude/mcp.json`):

```json
{
  "mcpServers": {
    "mdkb": {
      "type": "stdio",
      "command": "/path/to/mdkb",
      "args": ["mcp"]
    }
  }
}
```

The `mcp` subcommand connects to the daemon via unix socket (auto-spawning it
if needed). Each Claude Code session runs a lightweight proxy instead of a
full in-process server, sharing one daemon for file watching and indexing.

## MCP Tools (12)

| Tool | Description |
|------|-------------|
| `search` | Hybrid search across docs+memory (default), or scoped to `docs`, `memory`, `code`, `symbols`. `scope="memory"` accepts `min_confidence` to filter decayed entries |
| `get` | Retrieve by ID, path, memory slug, glob pattern, or comma-separated list |
| `code_graph` | Call graph queries: `calls`, `callers`, or `impact` (transitive) |
| `graph` | Knowledge-graph queries over frontmatter + wikilink edges: `links` (outgoing), `backlinks` (incoming), `neighbors` (adjacent, each annotated with the `via` relation), or `path` (shortest path to `to`). Edge endpoints render as document paths, never numeric ids |
| `status` | Index health, collections, and code index stats |
| `update` | Differential reindex of all collections and source code |
| `memory_write` | Create or update a memory entry (supports `ttl`, `due_in` for reminders, near-duplicate rejection) |
| `memory_write_batch` | Create or update multiple memory entries at once (max 20) |
| `memory_confirm` | Atomic Bayesian signal — `outcome="confirmed"` / `"refuted"` bumps `confirmations` and `last_confirmed_at` without rewriting content |
| `memory_delete` | Delete a memory entry |
| `memory_list` | List memory entries sorted by recency, popularity, or creation date |
| `usage` | Session and lifetime token ledger (per-tool call counts, token totals, truncation stats) |

### Search Scopes

| Scope | What it searches |
|-------|-----------------|
| _(omit)_ | Docs + memory combined (default) |
| `docs` | Hybrid BM25 + semantic over markdown documents |
| `memory` | Full-text over memory entries |
| `symbols` | Exact symbol lookup by name, filterable by `kind` and `file` |
| `code` | Semantic code search across indexed symbols |

### Memory

Persistent AI knowledge that survives across sessions — decisions, patterns, solved problems:

- **Confidence scoring** — entries decay over time unless re-confirmed (0-1 score based on age, access count, source type)
- **Duplicate detection** — near-duplicate entries are rejected before writing
- **Revision tracking** — manual entries track up to 3 revision diffs
- **TTL (time-to-live)** — pass `ttl` (seconds) to `memory_write` for auto-expiring entries. Expired entries are filtered from searches and listings but remain accessible via `get(id)` with an `[EXPIRED]` marker, so they can be inspected or renewed. Omit `ttl` for permanent entries.
- **Provenance** — `memory_write` records the authoring session and (optional) `agent`; both surface in `get(id)` and via `mdkb memory link ... --agent <name>`.

Entry types: `topic` (concepts), `problem` (solutions), `decision` (architectural choices), `reminder` (time-bound — see below), `prior` (behavioral patterns — 30-day TTL default, excluded from default searches), `handoff` (session handover — no default TTL).

#### Memory graph (typed edges)

Memory entries are graph nodes: a `memory_edges` table (schema v14) records typed relations between memories, or from a memory to a document. Relations are a closed set — `supports`, `contradicts`, `supersedes`, `derived_from`, `relates_to` — and unknown values are rejected with the valid set listed.

- **Create edges at write time** — pass `relates` to `memory_write`: `relates=[{relation, target, target_kind}]` (up to 10, `target_kind` is `memory` (default) or `doc`). The entry and its edges are written in one transaction. Or link an existing entry from the CLI: `mdkb memory link <id> <relation> <target> [--doc] [--agent <name>]`.
- **`supersedes`** keeps the scalar `superseded_by` and `superseded` status in lockstep with the edge (single write path).
- **Traverse** — `graph(entity, direction="links"|"backlinks", scope="memory")` (MCP) walks the memory graph; targets are dangling-tolerant and resolved at query time, mirroring the document graph.
- **`on_conflict="contradicts"`** — when a `memory_write` hits the near-duplicate gate, instead of rejecting it writes the new entry and links it to the similar one with a `contradicts` edge (returning both ids). Omitting `on_conflict` keeps the default rejection.
- **Recall expansion** — a recalled entry's active 1-hop neighbors are surfaced (capped), annotated `(via <relation>)`.
- **`[STALE-DEP]` marker** — at injection time, an entry whose `derived_from`/`supports` target is superseded or net-refuted is prefixed `[STALE-DEP]` in warmup and recall. This is a read-only flag — it never mutates stored confidence.

#### Graph introspection & gardening (CLI)

- **`mdkb graph dangling`** — lists references (with source doc + relation) that resolve to no indexed document. Full-table scan, explicit command only (never runs in hooks).
- **`mdkb graph hubs [--relation R] [--limit N]`** — entities ranked by degree centrality (in/out-degree) with a per-relation breakdown. Full-table scan, explicit command only.
- **`mdkb collection list`** — name, path, pattern, and document count per collection (`--format json` for stable output).
- Graph refs accept collection-prefixed paths (`map/people/x.md` resolves like `people/x`); an unresolved ref lists the forms it tried.

#### Reminders

Create with `memory_write(id, title, content, entry_type="reminder", due_in=<seconds>)` (or `mdkb memory add --entry-type reminder --due-in N`). While `due_at > now` the reminder is hidden from searches and listings. Once due, it appears in the session warmup index prefixed `[reminder:DUE] {id}: {title}` so the MCP client sees it on the next turn. The AI is instructed to ask for confirmation before deleting and to snooze via `memory_write` with a new `due_in` (same `id` updates the record).

#### Priors

Behavioral pattern entries written by external analyzers (e.g., HUD stop hooks). Create with `memory_write(id, title, content, entry_type="prior")` or `mdkb memory add <id> --entry-type prior`. Priors default to 30-day TTL and are excluded from all default searches — query them explicitly with `mdkb search --scope memory --entry-type prior "query"` or `search(query, scope="memory", entry_type="prior")` via MCP.

#### Handoffs

Session context transfer entries. Create with `memory_write(id, title, content, entry_type="handoff")` or `mdkb memory add <id> --entry-type handoff`. Use `--file <path>` (CLI) or `source_file` (MCP) to read content from a file — saves tokens when agents write handoffs to the filesystem. The file path is persisted as `source_path` metadata. Handoffs have no default TTL; confidence decay handles relevance naturally.

Source types control confidence weighting:

| Source Type | Multiplier | Use Case |
|-------------|-----------|----------|
| `official_docs` | 1.0 | Verified documentation |
| `user_statement` | 0.85 | Human-stated facts (default) |
| `auto_extracted` | 0.70 | Automated knowledge capture |
| `inference` | 0.65 | AI-inferred knowledge |

## Sessions

mdkb indexes Claude Code session JSONL files from `~/.claude/projects` to track token usage and tool call statistics per session.

### CLI

```bash
# Index sessions for the current project
mdkb session index

# Custom sessions directory or project root
mdkb session index --sessions-path /path/to/sessions --project-root /path/to/project
```

Session data feeds the `mdkb stats` dashboard (session totals, top tools by call count and tokens) and the `usage` MCP tool.

### MCP: `usage` tool

Returns per-tool call counts, total tokens, and averages:

```
usage(session_only=true)   # current session (default)
usage(session_only=false)  # lifetime aggregates across all sessions
```

## Code Intelligence

Tree-sitter parsing for **13 languages**: Rust, Go, TypeScript, JavaScript, Python, Java, Kotlin, C, C++, C#, PHP, Swift, Lua, and GDScript.

- **Substring search** — find symbols by partial name (FTS5 trigram, works from 3 characters)
- **Semantic code search** — find conceptually similar code using embeddings
- **Persistent call graph** — function calls, callers, and transitive impact radius survive restarts

Hidden directories (`.git/`, `.vscode/`, etc.) are excluded by default.
To force-index files inside a hidden directory, annotate your `.gitignore`:

```gitignore
# mdkb:index
.generated/**/*.rs
```

Generate semantic embeddings (downloads ~30MB ONNX model on first run):

```bash
mdkb embed
```

## CLI Reference

### Search

```bash
mdkb search "authentication flow"
mdkb search "handler" --scope symbols --kind function
mdkb search "auth handler" --scope code
```

### Collections

```bash
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection rename <old> <new>
```

### Document Retrieval

```bash
mdkb get <id|path|slug>
mdkb get 42 --lines 10:50
mdkb get "docs/*.md"
```

### Code Commands

```bash
mdkb code index
mdkb code search "handler" --kind fn
mdkb code calls main
mdkb code callers handle_get
mdkb code impact init --depth 5
```

### Knowledge Graph

Typed edges are extracted during indexing from allowlisted frontmatter keys
(strong) and body `[[wikilinks]]` (soft). Configure via the `[graph]` section.

```bash
mdkb graph links project.md                 # outgoing edges (owner, themes, links_to, ...)
mdkb graph links project.md --relation owner # filter by relation
mdkb graph backlinks alice                   # who points at this entity (works on dangling slugs)
mdkb graph neighbors project.md --depth 2    # adjacent entities, undirected
mdkb graph path project.md guide.md          # shortest path between two entities
```

### Memory

```bash
mdkb memory add auth-patterns -t "OAuth2 PKCE Flow" -T topic --tags auth,security \
  -c "Always use PKCE for public clients..."
mdkb memory add pay-bill -t "Pay electricity bill" -T reminder --due-in 86400 \
  -c "Monthly utility payment"
mdkb memory list
mdkb memory search "authentication"
mdkb memory history auth-patterns

# Export all entries to .mdkb/memory/entries/ (one .md file per entry)
mdkb memory export
mdkb memory export --dir ./memories --include-expired --overwrite

# Import from a markdown folder (auto-detected) or legacy JSON file
mdkb memory import .mdkb/memory/entries --skip-duplicates
mdkb memory import entries.json --dry-run --skip-duplicates
```

#### Team sync (git)

`mdkb init` keeps the SQLite indexes and machine-local state ignored while
allowing `.mdkb/memory/entries/*.md` into Git. Memory writes update that durable
projection, and `mdkb memory sync` reconciles changes arriving from a pull:

```bash
# Reconcile and commit the tracked projection
mdkb memory sync
git add .mdkb/.gitignore .mdkb/memory/entries/
git commit -m "chore(memory): sync team knowledge"

# Teammate, after pulling:
mdkb memory sync
```

Automate both ends with git hooks so nobody has to remember the manual steps:

```bash
# .git/hooks/pre-commit — reconcile before every commit
#!/bin/sh
mdkb memory sync
git add .mdkb/memory/entries/

# .git/hooks/post-merge and post-checkout — pick up teammates' entries after a pull
#!/bin/sh
mdkb memory sync
```

Only `entries/*.md` is meant for version control. `index.json` and `archive/`
under `.mdkb/memory/` are regenerable caches — never commit those. Derived
counters (`access_count`, `last_accessed`, `confirmations`) reset to zero on
import; they track local usage, not authored knowledge, so they don't need
to round-trip.

### Stats

`mdkb stats` is the unified diagnostic dashboard introduced in 2.0.0 (replaces the former `mdkb status` — not aliased, it was removed).

```bash
# Unified ASCII diagnostic dashboard
mdkb stats

# Machine-readable JSON output (safe for pipes and scripts)
mdkb stats --format json

# Plain text (no ANSI color, no Unicode box-drawing)
mdkb stats --no-color
```

The report is stacked: header (repo, version, db size, last update) → index health → collections → memory (by entry type, reminders DUE / upcoming 7d) → code (by language, top files by tokens) → sessions (totals, top tools) → hooks (slow events last 7d, reindex queue pending). Output auto-detects whether stdout is a TTY; the JSON format is stable for scripting.

## Configuration

Configuration lives in `.mdkb/config.toml`:

```toml
[search]
default_limit = 10

[indexing]
debounce_ms = 100
# When true, the doc/collection walker honors .gitignore.
# When false (default), it reads .mdkbignore instead.
respect_gitignore = false

[code.indexing]
# When true (default), the code walker honors .gitignore.
# When false, it reads .mdkbignore instead.
respect_gitignore = true

[mcp]
max_response_tokens = 50000
max_document_tokens = 10000
```

Environment overrides: `MDKB_SEARCH_DEFAULT_LIMIT=20`, `MDKB_INDEXING_DEBOUNCE_MS=200`.

### Controlling what gets indexed

Both the document walker (`mdkb update`) and the code walker (`mdkb code index`) share a unified ignore system:

| Mode                       | Files honored                                  | Use when                                                               |
| -------------------------- | ----------------------------------------------- | ---------------------------------------------------------------------- |
| `respect_gitignore = true` | `.gitignore` (+ `# mdkb:index` force-include)  | Your ignore rules are already correct for indexing.                    |
| `respect_gitignore = false`| `.mdkbignore` only                              | You want to index content that `.gitignore` excludes (e.g. `stories/`, generated sources), or you need a different ignore scope from git. |

**Defaults:**
- Code indexing: `respect_gitignore = true` — source trees usually want `.gitignore` honored (skip `target/`, `node_modules/`, etc.).
- Document indexing: `respect_gitignore = false` — project knowledge often lives in gitignored folders (plans, stories, drafts).

**`# mdkb:index` annotation** (only active when `respect_gitignore = true`):

Force-include a gitignored path by prefixing it with a `# mdkb:index` comment line in `.gitignore`:

```gitignore
# mdkb:index
generated/
# mdkb:index
docs/api/*.md
```

Blank lines between the annotation and the pattern are tolerated. The annotation is case-insensitive.

**`.mdkbignore`** (only active when `respect_gitignore = false`):

Uses the same syntax as `.gitignore`, including `!pattern` for re-inclusion. Place one at the repo root.

## Storage

All data stays local in `.mdkb/`:

```
.mdkb/
├── config.toml
├── index.sqlite      # FTS5 + document metadata
├── code.sqlite       # Source code symbols + call graph
└── memory/           # Memory entries (markdown mirror + index.json cache)
```

The embedding model (AllMiniLML6V2, ~30MB ONNX) is downloaded on first use and cached locally.

Keep `.mdkb/*` ignored at the repository root, then re-include
`.mdkb/.gitignore` and `.mdkb/memory/`. The generated store-level ignore file
allows only `memory/entries/*.md` to be tracked; indexes and machine-local state
remain ignored. See [Team sync (git)](#team-sync-git).

## License

MIT
