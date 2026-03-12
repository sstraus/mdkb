# mdkb

Local knowledge base with hybrid search for AI coding assistants.

**mdkb** gives Claude Code (or any MCP client) searchable access to your project's documentation, source code symbols, and persistent memory — all running locally, no cloud API needed.

## Why mdkb?

AI assistants can't read your entire codebase. mdkb solves this by indexing your markdown docs and source code locally, then exposing hybrid search (BM25 + semantic vectors) through MCP. The AI queries what it needs, when it needs it.

## Installation

```bash
cargo install --path .
```

Or download a pre-built binary from [Releases](https://github.com/sstraus/mdkb/releases).

## Quick Start

```bash
# Initialize in your project
cd your-project
mdkb init

# Add documentation collections
mdkb collection add docs ./docs
mdkb collection add wiki ./wiki --pattern "**/*.md"

# Index everything and start serving
mdkb update
```

### Connect to Claude Code

```bash
# Project-scoped (recommended)
mdkb setup mcp claude --scope local

# Or user-scoped (global)
mdkb setup mcp claude --scope user
```

Restart Claude Code after setup. The MCP server auto-indexes on startup and watches for file changes — no manual `update` needed after initial setup.

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

## MCP Tools (8)

| Tool | Description |
|------|-------------|
| `search` | Hybrid search across docs+memory (default), or scoped to `docs`, `memory`, `code`, `symbols` |
| `get` | Retrieve by ID, path, memory slug, glob pattern, or comma-separated list |
| `code_graph` | Call graph queries: `calls`, `callers`, or `impact` (transitive) |
| `status` | Index health, collections, and code index stats |
| `update` | Differential reindex of all collections and source code |
| `memory_write` | Create or update a memory entry |
| `memory_delete` | Delete a memory entry |
| `memory_list` | List memory entries sorted by recency, popularity, or creation date |

### Search Scopes

| Scope | What it searches |
|-------|-----------------|
| _(omit)_ | Docs + memory combined (default) |
| `docs` | Hybrid BM25 + semantic over markdown documents |
| `memory` | Full-text over memory entries |
| `symbols` | Exact symbol lookup by name, filterable by `kind` and `file` |
| `code` | Semantic code search across indexed symbols |

### Memory

Memory entries persist AI knowledge across sessions — decisions, patterns, solved problems:

- **Session start**: Top 50 entries loaded in server instructions (~1.5K tokens)
- **On demand**: AI calls `get(slug)` for full content
- **Learning**: AI calls `memory_write` to persist new knowledge
- **Search**: AI calls `search(query, scope="memory")` for related entries

Entry types: `topic` (concepts), `problem` (solutions), `decision` (architectural choices).

## Code Intelligence

mdkb indexes source code with tree-sitter parsers for **13 languages**: Rust, Go, TypeScript, JavaScript, Python, Java, Kotlin, C, C++, C#, PHP, Swift, Lua, and GDScript.

The code index supports:
- **Symbol search** — find functions, structs, methods by name
- **Semantic code search** — find conceptually similar code using embeddings
- **Call graph** — trace what a function calls, what calls it, or impact radius

Generate semantic embeddings (downloads ~30MB ONNX model on first run):

```bash
mdkb embed
```

## Search (CLI)

```bash
# Document search (hybrid BM25 + semantic)
mdkb search "authentication flow"
mdkb search "error handling" -c docs

# Symbol search
mdkb search "handler" --scope symbols --kind function
mdkb search "Config" --scope symbols --kind struct --file main.rs

# Semantic code search
mdkb search "auth handler" --scope code
```

Output: `[id] collection:path - title (score: 0.85)`

Use `mdkb get <id>` to retrieve full document content.

## Collections

```bash
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection rename <old> <new>
```

Pattern defaults to `**/*.md`. Use `-c <name>` on search to filter by collection.

## Document Retrieval

```bash
mdkb get <id|path|slug>          # By ID, path, or memory slug
mdkb get 42 --lines 10:50        # Specific line range
mdkb get "docs/*.md"             # By glob pattern
mdkb get 42,43,44                # Comma-separated IDs
```

## Code Commands (CLI)

```bash
mdkb code index                         # Build/rebuild code index
mdkb code search "handler" --kind fn    # Fuzzy symbol search
mdkb code find "Config" --kind struct   # Exact symbol lookup
mdkb code calls main                    # What does main() call?
mdkb code callers handle_get            # What calls handle_get()?
mdkb code impact init --depth 5         # Transitive dependency graph
mdkb code info                          # Index statistics
```

## Memory (CLI)

```bash
mdkb memory add auth-patterns -t "OAuth2 PKCE Flow" -T topic --tags auth,security \
  -c "Always use PKCE for public clients..."
mdkb memory show auth-patterns
mdkb memory list
mdkb memory search "authentication"
mdkb memory warmup                # Compact index for session start
```

## Configuration

Configuration lives in `.mdkb/config.toml`:

```toml
[search]
default_limit = 10

[indexing]
debounce_ms = 100

[mcp]
max_response_tokens = 50000
max_document_tokens = 10000
```

Environment overrides: `MDKB_SEARCH_DEFAULT_LIMIT=20`, `MDKB_INDEXING_DEBOUNCE_MS=200`.

## Storage

All data stays local in `.mdkb/`:

```
.mdkb/
├── config.toml
├── index.sqlite      # FTS5 + document metadata
├── code-index/       # Tantivy index for source code
└── memory/           # Memory entries (markdown files)
```

The embedding model (AllMiniLML6V2, ~30MB ONNX) is downloaded on first use and cached locally.

Add `.mdkb/` to `.gitignore` — it can be regenerated with `mdkb update && mdkb embed`.

### Default Code Exclusions

These paths are excluded from code indexing:

```
**/target/**        **/.git/**         **/dist/**
**/node_modules/**  **/vendor/**       **/build/**
**/__pycache__/**   **/.venv/**
```

Configurable via `[code.indexing] ignore_patterns` in `config.toml`.

### Incremental Indexing

The MCP server watches your project for file changes and reindexes automatically:

- **Documents** — on change, the server reconciles each collection against the filesystem. New files are added, modified files re-parsed, and files deleted from disk are removed from the index.
- **Code** — changed files are batched (30s idle window) and reindexed incrementally. Content hashes skip unchanged files; deleted files are purged from both Tantivy and the vector store.
- **Startup** — if an index already exists, the server performs an incremental reindex (hash-based diff). A fresh index triggers a full reindex.

The file watcher uses OS-native notifications (`notify` crate) with 100ms debounce. Code exclusion patterns (e.g. `node_modules`) apply to both full and incremental reindexing.

## License

MIT
