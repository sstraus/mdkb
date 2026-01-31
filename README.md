# mdkb

Local markdown knowledge base with hybrid search for AI assistants.

**mdkb** indexes your markdown files and exposes them to Claude Code (or any MCP-compatible AI) through a local search API. It combines keyword matching with semantic understanding to find the most relevant documents.

## Why mdkb?

AI assistants are limited by context windows. When your codebase has extensive documentation, the AI can't read it all. mdkb solves this by:

1. **Indexing** your markdown files locally (no cloud, no API keys)
2. **Searching** with hybrid retrieval (keyword + semantic)
3. **Exposing** results through MCP so Claude can query your docs on demand

## Features

- **Per-repo storage** - `.mdkb/` directory in your project, gitignore-friendly
- **Hybrid search** - Combines BM25 keyword matching with semantic vector search using RRF fusion
- **Local LLM** - Embeddings generated locally with a small GGUF model (~100MB)
- **MCP server** - Integrates directly with Claude Code
- **File watching** - Auto-reindex when files change (in MCP mode)
- **Differential indexing** - Only reindex modified files
- **Smart exclusions** - Ignores `.git/`, `node_modules/`, skill files, etc.

## Installation

```bash
# Clone and build
git clone <repo-url>
cd mdkb
cargo build --release

# Add to PATH or copy binary
cp target/release/mdkb ~/.local/bin/
```

## Quick Start

```bash
# Initialize in your project
cd your-project
mdkb init

# Add collections (groups of documents)
mdkb collection add docs ./docs
mdkb collection add wiki ./wiki --pattern "**/*.md"

# Index files and generate embeddings
mdkb update    # Index text content
mdkb embed     # Generate semantic embeddings (first run downloads ~100MB model)

# Search
mdkb search "authentication flow"
mdkb search "how to handle errors" -c docs  # Filter to docs collection
```

## How Hybrid Search Works

mdkb combines two search strategies for better results:

### 1. BM25 Keyword Search
Traditional full-text search using SQLite FTS5. Good for exact terms:
- "OAuth2 callback" finds documents with those exact words
- Fast, no embeddings required

### 2. Semantic Vector Search
Uses a local embedding model to understand meaning:
- "how to authenticate users" finds docs about "login", "OAuth", "JWT"
- Requires running `mdkb embed` to generate vectors

### 3. RRF Fusion
Results from both methods are merged using Reciprocal Rank Fusion:
- Documents appearing in both lists rank higher
- Balances precision (keywords) with recall (semantics)

## CLI Commands

```bash
# Initialization
mdkb init                      # Create .mdkb/ directory

# Collections
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection list
mdkb collection rename <old> <new>

# Search & Retrieval
mdkb search <query> [-l N] [-c NAME]   # Hybrid search
mdkb get <id|path> [--lines 10:50]     # Retrieve document by ID
mdkb mget <pattern> [-c NAME]          # Batch retrieve by glob

# Indexing
mdkb update                    # Differential text reindex
mdkb embed                     # Generate/update embeddings
mdkb status                    # Show index statistics

# Server
mdkb serve                     # Start MCP server
mdkb stats [-s N] [-a]         # Usage statistics
```

### Options

| Option | Description |
|--------|-------------|
| `-l, --limit N` | Maximum results to return (default: 10) |
| `-c, --collection NAME` | Filter to a specific collection |
| `--format FORMAT` | Output: `text`, `json`, `csv`, `markdown` |
| `-v, -vv, -vvv` | Increase verbosity (info, debug, trace) |

### Search Output

Results show `[id] collection:path - title (score)`:

```
[42] docs:api/auth.md - Authentication Guide (score: 0.85)
[17] notes:security.md - Security Notes (score: 0.72)
```

Use the ID to retrieve full content: `mdkb get 42`

## MCP Server Integration

mdkb implements the [Model Context Protocol](https://modelcontextprotocol.io/) to integrate with Claude Code and other MCP-compatible AI assistants.

### Setup with Claude Code

Add to your Claude Code MCP configuration (`~/.claude/mcp_servers.json`):

```json
{
  "mdkb": {
    "command": "mdkb",
    "args": ["serve"],
    "cwd": "/path/to/your/project"
  }
}
```

Or for multiple projects, use the global installation:

```json
{
  "mdkb-myproject": {
    "command": "/home/user/.local/bin/mdkb",
    "args": ["serve"],
    "cwd": "/home/user/projects/myproject"
  }
}
```

### What Claude Sees

When configured, Claude Code has access to these tools:

| Tool | Description |
|------|-------------|
| `mdkb_search` | Search documents with hybrid retrieval |
| `mdkb_get` | Retrieve full document content by ID |
| `mdkb_multi_get` | Batch retrieve documents by glob pattern |
| `mdkb_list_collections` | List available collections |
| `mdkb_status` | Check index health |
| `mdkb_update` | Trigger reindex (after file changes) |
| `mdkb_metrics` | View token usage statistics |

### Example Interaction

```
User: How does authentication work in this project?

Claude: Let me search the documentation.
[Calls mdkb_search with query "authentication"]

Claude: I found relevant docs. Let me read the authentication guide.
[Calls mdkb_get with id 42]

Claude: Based on the documentation, authentication uses JWT tokens...
```

### File Watching

In MCP mode, mdkb watches collection paths for changes and automatically reindexes:
- New files are indexed within 100ms
- Modified files are re-indexed
- Deleted files are removed from the index

## Configuration

Configuration lives in `.mdkb/config.toml`:

```toml
[search]
default_limit = 10

[indexing]
debounce_ms = 100

[mcp]
max_response_tokens = 4000      # Truncate long responses
max_document_tokens = 2000      # Per-document limit in multi_get
truncate_with_ellipsis = true   # Add "..." when truncating
```

### Environment Overrides

```bash
MDKB_SEARCH_LIMIT=20 mdkb search "query"
MDKB_MCP_MAX_TOKENS=8000 mdkb serve
```

## Storage

All data stays in your project:

```
.mdkb/
├── config.toml      # Configuration
├── index.sqlite     # SQLite database (FTS5 + vectors)
└── models/          # Downloaded GGUF models
    └── bge-small-en-v1.5-q8_0.gguf
```

Add `.mdkb/` to your `.gitignore` - the index can be regenerated.

## Default Exclusions

These paths are never indexed:

```
**/SKILL.md
**/skills/**
**/.claude/**
**/.git/**
**/node_modules/**
**/target/**
**/.mdkb/**
```

## Architecture

```
┌─────────────────┐     ┌─────────────────┐
│   Claude Code   │────▶│   MCP Server    │
│  (or other AI)  │◀────│  (mdkb serve)   │
└─────────────────┘     └────────┬────────┘
                                 │
                    ┌────────────┴────────────┐
                    │                         │
              ┌─────▼─────┐           ┌───────▼───────┐
              │  SQLite   │           │  File Watcher │
              │  FTS5 +   │           │   (notify)    │
              │  Vectors  │           └───────────────┘
              └───────────┘
```

**Components:**
- **CLI** (`clap`) - Command-line interface
- **MCP Server** (`rmcp`) - JSON-RPC over stdio
- **Storage** (`rusqlite` + `sqlite-vec`) - FTS5 for keywords, sqlite-vec for embeddings
- **LLM** (`llama-cpp-rs`) - Local embedding generation
- **Watcher** (`notify`) - File system monitoring

## Performance

- **Indexing**: ~1000 docs/second (text only)
- **Embeddings**: ~10 docs/second (depends on hardware)
- **Search**: <10ms for most queries
- **Memory**: ~50MB base + model size (~100MB when generating embeddings)

## License

MIT
