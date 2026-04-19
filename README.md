# mdkb

**Give your AI coding assistant a memory and a search engine.**

mdkb indexes your project's docs, source code, and persistent knowledge into a local hybrid search engine — then exposes it to Claude Code (or any MCP client) so the AI finds what it needs instead of guessing.

No cloud APIs. No token-heavy context dumps. Just fast, local, relevant retrieval.

## What it does

- **Hybrid search** — BM25 + semantic vectors over your markdown docs
- **Code intelligence** — tree-sitter parsing for 13 languages, call graphs, symbol search
- **Persistent memory** — AI-created knowledge entries that survive across sessions, including time-bound `reminder` entries with due-date surfacing
- **Lifecycle hooks** — proactive context injection and reindex enqueue via Claude Code / Codex CLI hooks (no tool call required)
- **Markdown-native memory** — export/import memory entries as a folder of `.md` files for review, git tracking, or bulk edit
- **Unified diagnostics** — `mdkb stats` renders a static ASCII dashboard (index health, collections, memory, code, sessions, hooks)
- **Zero config serving** — auto-indexes on startup, watches for file changes, auto-`VACUUM`s on drift

### Recent highlights (2.0.0 / 1.5.0 / 1.4.0)

Full details in [CHANGES.md](CHANGES.md).

- **2.0.0 (breaking)** — `mdkb status` removed (use `mdkb stats`); `mdkb memory export`/`import` round-trip entries as `.md` files with YAML frontmatter; unified ASCII stats dashboard with `--format json` and `--no-color`.
- **1.5.0** — lifecycle hooks (`SessionStart`, `UserPromptSubmit`, `PostToolUse`) via `mdkb setup hooks claude|codex`; `usage` MCP tool (session+lifetime token ledger); access-count × recency as a third RRF signal in `search`; `memory_confirm` atomic Bayesian signal; auto-`VACUUM` + `PRAGMA optimize` on drift; `mdkb setup mcp codex`.
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

MCP gives the assistant tools; hooks make it use them. Register the lifecycle dispatcher so Claude gets a memory warmup at session start and relevant context on every prompt — without having to call `search` first:

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

Restart the host CLI after setup. Re-running is idempotent: existing hook entries are replaced, unrelated settings preserved. Events: `session-start`, `user-prompt-submit`, `post-tool-use`. Full contract, config, and opt-out in [docs/hooks.md](docs/hooks.md).

### Binary path caveat

`mdkb setup mcp …` and `mdkb setup hooks …` hard-code the absolute path of the binary that ran the setup. If you later move or rebuild the binary, the recorded command breaks. For stable global installs, first run `cargo install --path .` (binary lands in `~/.cargo/bin/mdkb`), then run setup from that binary.

### Uninstalling

There is no dedicated `mdkb uninstall` subcommand — removal is a few targeted edits:

**MCP (Claude Code):**

```bash
claude mcp remove mdkb -s local   # per-project
claude mcp remove mdkb -s user    # global
```

Note: `claude mcp remove` operates on `~/.claude-private/.claude.json`, while `mdkb setup mcp claude` writes to `~/.claude.json`. If you registered via `mdkb setup`, also manually delete the `mdkb` block from the `mcpServers` object in `~/.claude.json` (user scope) or from the per-project entry (local scope).

**Hooks:**

No CLI removal exists. Delete the three `_managedBy: "mdkb"` entries (`SessionStart`, `UserPromptSubmit`, `PostToolUse`) from:

- `.claude/settings.local.json` — if installed with `--scope local`
- `~/.claude/settings.json` — if installed with `--scope user`
- `~/.codex/hooks.json` — for Codex CLI

Soft alternatives before uninstalling: create an empty `.mdkbignore-hooks` marker at the repo root to silence hooks for that working tree, or toggle `session_start_enabled` / `user_prompt_submit_enabled` / `post_tool_use_enabled` in `.mdkb/config.toml`.

### Manual MCP Setup

Add to your Claude Code MCP config (`.claude/mcp.json` or `~/.claude/mcp.json`):

```json
{
  "mcpServers": {
    "mdkb": {
      "type": "stdio",
      "command": "/path/to/mdkb",
      "args": ["serve"],
      "cwd": "/path/to/your/project"
    }
  }
}
```

The `cwd` must point to a directory with `.mdkb/` initialized.

## MCP Tools (11)

| Tool | Description |
|------|-------------|
| `search` | Hybrid search across docs+memory (default), or scoped to `docs`, `memory`, `code`, `symbols`. `scope="memory"` accepts `min_confidence` to filter decayed entries |
| `get` | Retrieve by ID, path, memory slug, glob pattern, or comma-separated list |
| `code_graph` | Call graph queries: `calls`, `callers`, or `impact` (transitive) |
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

Entry types: `topic` (concepts), `problem` (solutions), `decision` (architectural choices), `reminder` (time-bound — see below).

#### Reminders

Create with `memory_write(id, title, content, entry_type="reminder", due_in=<seconds>)` (or `mdkb memory add --entry-type reminder --due-in N`). While `due_at > now` the reminder is hidden from searches and listings. Once due, it appears in the session warmup index prefixed `[reminder:DUE] {id}: {title}` so the MCP client sees it on the next turn. The AI is instructed to ask for confirmation before deleting and to snooze via `memory_write` with a new `due_in` (same `id` updates the record).

Source types control confidence weighting:

| Source Type | Multiplier | Use Case |
|-------------|-----------|----------|
| `official_docs` | 1.0 | Verified documentation |
| `user_statement` | 0.85 | Human-stated facts (default) |
| `auto_extracted` | 0.70 | Automated knowledge capture |
| `inference` | 0.65 | AI-inferred knowledge |

## Code Intelligence

Tree-sitter parsing for **13 languages**: Rust, Go, TypeScript, JavaScript, Python, Java, Kotlin, C, C++, C#, PHP, Swift, Lua, and GDScript.

- **Substring search** — find symbols by partial name (FTS5 trigram, works from 3 characters)
- **Semantic code search** — find conceptually similar code using embeddings
- **Persistent call graph** — function calls, callers, and transitive impact radius survive restarts

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
└── memory/           # Memory entries (markdown files)
```

The embedding model (AllMiniLML6V2, ~30MB ONNX) is downloaded on first use and cached locally.

Add `.mdkb/` to `.gitignore` — it can be regenerated with `mdkb update && mdkb embed`.

## License

MIT
