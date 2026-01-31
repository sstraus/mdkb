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
- **Semantic vector search** - Understands meaning. "how to authenticate users" finds docs about "login", "OAuth", "JWT".

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

1. **Session start**: AI calls `mdkb_memory_index` to get top 50 entries (~1.5K tokens)
2. **On demand**: AI calls `mdkb_memory_get` for full entry content
3. **Learning**: AI calls `mdkb_memory_write` to persist new knowledge
4. **Finding**: AI calls `mdkb_memory_search` to find related entries

Access counts track which entries are most useful, ensuring the warmup index prioritizes frequently-used knowledge.

## MCP Server

mdkb implements the [Model Context Protocol](https://modelcontextprotocol.io/) to integrate with Claude Code.

### Setup

Add to `~/.claude/mcp_servers.json`:

```json
{
  "mdkb": {
    "command": "mdkb",
    "args": ["serve"],
    "cwd": "/path/to/your/project"
  }
}
```

### Available Tools

| Tool | Description |
|------|-------------|
| `mdkb_search` | Hybrid search (keyword + semantic) |
| `mdkb_get` | Retrieve document by ID (supports line ranges) |
| `mdkb_multi_get` | Batch retrieve by glob pattern |
| `mdkb_list_collections` | List available collections |
| `mdkb_status` | Check index health |
| `mdkb_update` | Trigger reindex |
| `mdkb_metrics` | View token usage statistics |
| `mdkb_memory_index` | Get warmup index for session start |
| `mdkb_memory_get` | Retrieve full memory entry |
| `mdkb_memory_write` | Create or update memory entry |
| `mdkb_memory_search` | Search memory entries |

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
├── index.sqlite
└── models/
    └── bge-small-en-v1.5-q8_0.gguf
```

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
