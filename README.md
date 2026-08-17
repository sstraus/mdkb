<p align="center">
  <img src="https://raw.githubusercontent.com/sstraus/mdkb/main/assets/mdkb.png" alt="mdkb logo" width="420">
</p>

# mdkb

**Repository memory for coding agents.**

mdkb gives Claude Code, Codex, and other MCP clients one local retrieval layer
for the repository: durable project memory, Markdown documentation, source
symbols, and call relationships.

It combines hybrid search, knowledge graphs, code intelligence, and lifecycle
hooks so an agent can recover what the team decided, find what the docs say, and
trace what the code does without loading the repository into every prompt.

Storage and inference stay local. mdkb uses SQLite, FTS5, tree-sitter, and a
local ONNX embedding model; no memory API, hosted vector database, or LLM
extraction service is required. The embedding model is downloaded on its first
use and then runs on-device.

## Why mdkb

- **Repository-first, not conversation-first** — docs, decisions, solved
  problems, symbols, and dependencies are searchable as one project context.
- **Memory designed to age well** — typed entries carry provenance, confidence
  decay, confirmation/refutation signals, TTL, reminders, revisions,
  supersession, and explicit relations. Stale knowledge is surfaced instead of
  silently becoming permanent truth.
- **Recall is not dependent on a lucky tool call** — hooks inject a compact
  session warmup, provide opt-in prompt recall with a leading `*` by default,
  redirect code searches to indexed symbols, and reindex after edits. Always-on
  prompt recall is configurable.
- **Code intelligence is structural** — tree-sitter indexes 14 languages and
  persists symbols and call relationships, so callers, callees, and transitive
  impact do not require repeated multi-file grep.
- **Local and inspectable** — queries, indexes, and embeddings stay on the
  machine. Durable authored memory is projected to reviewable Markdown for Git,
  while machine-local counters and SQLite state stay untracked.
- **Low operational weight** — one Rust binary owns CLI, MCP, hooks, watching,
  and repair. On Unix, an auto-started local daemon shares indexes and serializes
  writes; there is no separate LLM, vector, or graph service to provision.

## How it differs from other memory systems

"AI memory" covers products with very different jobs. mdkb deliberately
optimizes for software repositories rather than trying to be a general-purpose
personalization or conversation-memory platform.

| Memory approach | Usually optimized for | mdkb's difference |
|---|---|---|
| Conversation-memory SDKs | Extracting user facts and preferences for an application | Works as an installed repository tool; no application integration, extraction LLM, or hosted service is required |
| Markdown knowledge bases | Portable notes and human-editable knowledge graphs | Adds typed engineering-memory lifecycle, source indexing, symbol search, and a persistent call graph |
| Session-recording plugins | Capturing tool activity and AI-compressing past conversations | Prioritizes curated project truth and retrieves it alongside docs and code; it does not require a second AI process to summarize memory |
| Temporal knowledge graphs | Evolving entities, events, and point-in-time facts | Uses a lighter local stack and deterministic project relations; no graph database or ingestion LLM is required |

Choose mdkb when the repository is the memory boundary and source-code impact is
part of recall. A conversation-memory SDK is a better fit for end-user
personalization; a temporal graph is a better fit for bi-temporal entity facts;
and a session recorder is a better fit when automatic transcript compression is
the primary requirement.

## What it does

- **Hybrid retrieval** — BM25 + local semantic vectors over documents and
  memory, with result fusion and filters.
- **Two knowledge graphs** — frontmatter/wikilink relations for project docs,
  plus typed relations between memories and documents.
- **Code intelligence** — tree-sitter parsing for 14 languages, symbol and
  semantic code search, callers, callees, and transitive impact.
- **Persistent memory** — `topic`, `problem`, `decision`, `reminder`, `prior`,
  and `handoff` entries with duplicate detection and revision history.
- **Lifecycle hooks** — session warmup, controlled prompt recall, code-search
  guidance, and post-edit reindexing for Claude Code and Codex.
- **Git-reviewable memory** — bidirectional synchronization between the local
  store and `.mdkb/memory/entries/*.md` without projecting local usage churn.
- **Unified diagnostics** — `mdkb stats` reports index health, collections,
  memory, code, sessions, and hooks; `mdkb surface` maps MCP tools to CLI
  commands.
- **Self-maintaining indexes** — automatic watching, differential reindexing,
  integrity checks, repair, and database maintenance.

See [CHANGES.md](CHANGES.md) for release history.

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

Register the lifecycle dispatcher so Claude gets a memory warmup at session
start, prompt recall when requested, and Grep-to-mdkb suggestions — without
having to call `search` first:

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

Per-prompt recall is quiet by default: prefix a prompt with `*` to inject
matching memory, documents, and graph hints. To make it always-on, set
`user_prompt_submit_require_sigil = false` under `[hooks]` in
`.mdkb/config.toml`. Session warmup and the other enabled hooks do not require
the sigil.

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

Tree-sitter parsing for **14 languages**: Rust, Go, TypeScript, JavaScript, Python, Java, Kotlin, C, C++, C#, PHP, Swift, Lua, and GDScript.

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

The report is stacked: header (repo, version, db size, last update) → index health → collections → memory (by entry type, reminders DUE / upcoming 7d) → code (by language, top files by tokens) → sessions (totals, top tools) → hooks (invocations, hit rate, latency, prior mining, registration drift). Output auto-detects whether stdout is a TTY; the JSON format is stable for scripting.

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

Project state stays local in `.mdkb/`:

```
.mdkb/
├── config.toml
├── index.sqlite      # FTS5 + document metadata
├── code.sqlite       # Source code symbols + call graph
└── memory/           # Memory entries (markdown mirror + index.json cache)
```

The embedding model (AllMiniLML6V2, ~30MB ONNX) is downloaded on first use and cached in the platform's user cache directory.

Keep `.mdkb/*` ignored at the repository root, then re-include
`.mdkb/.gitignore` and `.mdkb/memory/`. The generated store-level ignore file
allows only `memory/entries/*.md` to be tracked; indexes and machine-local state
remain ignored. See [Team sync (git)](#team-sync-git).

## License

MIT
