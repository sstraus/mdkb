# mdkb

Local markdown knowledge base with hybrid search for AI assistants.

**mdkb** indexes your markdown files and exposes them to Claude Code (or any MCP-compatible AI) through a local search API. It combines keyword matching with semantic understanding to find the most relevant documents.

## Why mdkb?

AI assistants are limited by context windows. When your codebase has extensive documentation, the AI can't read it all. mdkb solves this by:

1. **Indexing** your markdown files locally (no cloud, no API keys)
2. **Searching** with hybrid retrieval (keyword + semantic)
3. **Exposing** results through MCP so Claude can query your docs on demand

## Installation

```bash
cargo install --path .
```

Or download a pre-built binary from [Releases](https://github.com/user/mdkb/releases).

## Quick Start

```bash
# Initialize in your project
cd your-project
mdkb init

# Add collections (groups of documents)
mdkb collection add docs ./docs
mdkb collection add wiki ./wiki --pattern "**/*.md"

# Index files
mdkb update

# Generate semantic embeddings (downloads ~100MB model on first run)
mdkb embed

# Search
mdkb search "authentication flow"
mdkb search "how to handle errors" -c docs
```

## Search

mdkb uses hybrid search combining two strategies:

- **BM25 keyword search** - Traditional full-text search. "OAuth2 callback" finds documents with those exact words.
- **Semantic vector search** - Understands meaning using fastembed (AllMiniLML6V2, 384-dim). "how to authenticate users" finds docs about "login", "OAuth", "JWT".

Results are merged using Reciprocal Rank Fusion (RRF), so documents matching both keyword and meaning rank highest.

```bash
mdkb search <query> [-l LIMIT] [-c COLLECTION]
```

Output format: `[id] collection:path - title (score)`

```
[42] docs:api/auth.md - Authentication Guide (score: 0.85)
[17] notes:security.md - Security Notes (score: 0.72)
```

Use `mdkb get 42` to retrieve full document content.

## Collections

Collections group documents by source directory:

```bash
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection list
mdkb collection remove <name>
mdkb collection rename <old> <new>
```

The pattern defaults to `**/*.md`. Use `-c <name>` on search/get commands to filter by collection.

## Document Retrieval

```bash
mdkb get <id|path>              # Get document by ID or path
mdkb get 42 --lines 10:50       # Get specific line range
mdkb mget "*.md" [-c NAME]      # Batch retrieve by glob pattern
```

## Memory

Memory entries persist AI knowledge across sessions. Store decisions, learned patterns, and project context:

```bash
# Add a memory entry
mdkb memory add auth-patterns -t "OAuth2 PKCE Flow" -T topic --tags auth,security \
  -c "Always use PKCE for public clients. Store refresh tokens securely..."

# Show entry
mdkb memory show auth-patterns

# List entries (ranked by access count)
mdkb memory list

# Search memories
mdkb memory search "authentication"

# Get warmup index (compact list for session start)
mdkb memory warmup
```

Entry types: `topic` (concepts), `problem` (solutions), `decision` (architectural choices).

### How AIs Use Memory

1. **Session start**: Server instructions include top 50 memory entries (~1.5K tokens)
2. **On demand**: AI calls `get(slug)` for full entry content
3. **Learning**: AI calls `memory_write` to persist new knowledge
4. **Finding**: AI calls `search(query, scope="memory")` to find related entries

Access counts track which entries are most useful, ensuring the warmup index prioritizes frequently-used knowledge.

## MCP Server

mdkb implements the [Model Context Protocol](https://modelcontextprotocol.io/) to integrate with Claude Code.

### Setup

**Option 1: Automatic Setup (Recommended)**

Run the setup command from your project directory (where `.mdkb/` exists):

```bash
# Project-scoped (recommended for project-specific documentation)
mdkb setup mcp claude --scope local

# User-scoped (global installation)
mdkb setup mcp claude --scope user
```

This registers mdkb with Claude Code using `claude mcp add`. Restart Claude Code after setup.

**Option 2: Manual Setup**

If you prefer manual configuration, add to `~/.claude/mcp.json`:

```json
{
  "mcpServers": {
    "mdkb": {
      "type": "stdio",
      "command": "/path/to/mdkb",
      "args": ["serve"],
      "cwd": "/path/to/your/project",
      "env": {},
      "alwaysAllow": ["*"]
    }
  }
}
```

**Important**: The `cwd` field specifies where the MCP server runs. This must be a directory with `.mdkb/` initialized. Without `cwd`, the server runs from wherever Claude Code starts it.

For project-scoped setups (Option 1 with `--scope local`), the server runs from your current project directory. For user-scoped setups (`--scope user`), ensure `.mdkb/` exists in your home directory or launch Claude Code from a directory with `.mdkb/` initialized.

### Available Tools (10)

| Tool | Description |
|------|-------------|
| `search` | Unified search with scope: `docs` (default), `memory`, `all`, `code`, `symbols` |
| `get` | Retrieve by ID, path, or memory slug (supports line ranges) |
| `multi_get` | Batch retrieve by glob pattern |
| `code_graph` | Call graph queries: `calls`, `callers`, or `impact` (requires `code-intel` feature) |
| `status` | Index health, collections, and code index stats |
| `update` | Trigger reindex |
| `memory_write` | Create or update memory entry |
| `memory_delete` | Delete a memory entry |
| `collection_add` | Add a document collection |
| `collection_remove` | Remove a collection |

### File Watching

In MCP mode, mdkb watches collection paths and automatically reindexes when files change.

## Configuration

Configuration lives in `.mdkb/config.toml`:

```toml
[search]
default_limit = 10

[indexing]
debounce_ms = 100

[mcp]
max_response_tokens = 4000
max_document_tokens = 2000
```

Environment overrides: `MDKB_SEARCH_LIMIT=20`, `MDKB_MCP_MAX_TOKENS=8000`

## Storage

All data stays local in `.mdkb/`:

```
.mdkb/
├── config.toml
└── index.sqlite
```

The embedding model (AllMiniLML6V2, ~30MB ONNX) is downloaded automatically on first use and cached locally.

Add `.mdkb/` to `.gitignore` - it can be regenerated with `mdkb update && mdkb embed`.

## Default Exclusions

These paths are never indexed:

```
**/.git/**
**/.mdkb/**
**/.claude/**
**/node_modules/**
**/target/**
**/SKILL.md
**/skills/**
```

## License

MIT
