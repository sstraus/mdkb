# mdkb

Local markdown knowledge base CLI with semantic search.

## Features

- **Per-repo storage** - `.mdkb/` directory in your project
- **Full-text search** - BM25 ranking via SQLite FTS5
- **Semantic search** - Vector embeddings with local LLM inference (optional)
- **Hybrid search** - RRF fusion combining keyword and semantic results
- **MCP server mode** - Expose as tools for AI assistants
- **File watching** - Auto-reindex on changes in MCP mode
- **Differential indexing** - Only reindex changed files
- **Smart exclusions** - Ignores `.git/`, `node_modules/`, skill files, etc.

## Installation

```bash
# Basic build
cargo build --release

# With LLM support for semantic search
cargo build --release --features llm
```

## Quick Start

```bash
# Initialize in your project
cd your-project
mdkb init

# Add a collection
mdkb collection add docs ./docs

# Index the files
mdkb update

# Search
mdkb search "authentication"

# Semantic search (requires --features llm)
mdkb vsearch "how to handle user login"

# Hybrid search
mdkb query "error handling patterns"
```

## CLI Commands

```bash
mdkb init                      # Initialize .mdkb/ directory
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection list
mdkb collection rename <old> <new>

mdkb search <query> [-l limit] [-c collection]   # BM25 full-text
mdkb vsearch <query> [-l limit] [-c collection]  # Vector semantic
mdkb query <query> [-l limit] [-c collection]    # Hybrid with RRF

mdkb get <id|path> [--lines 10:50]               # Retrieve document
mdkb mget <pattern> [-c collection]              # Batch retrieve

mdkb status                    # Index status
mdkb update                    # Differential reindex
mdkb embed                     # Generate embeddings (requires --features llm)
mdkb stats [-s N] [-a]         # Usage statistics

mdkb serve                     # Start MCP server
```

## Output Formats

All commands support `--format`:
- `text` (default)
- `json`
- `csv`
- `markdown`

```bash
mdkb search "query" --format json
```

## MCP Server

Run as an MCP server for AI assistants:

```bash
mdkb serve
```

### Available Tools

| Tool | Description |
|------|-------------|
| `mdkb_search` | BM25 full-text search |
| `mdkb_vsearch` | Semantic vector search |
| `mdkb_query` | Hybrid search with RRF fusion |
| `mdkb_get` | Retrieve document by ID/path (supports line ranges) |
| `mdkb_multi_get` | Batch retrieve by glob pattern |
| `mdkb_list_collections` | List indexed collections |
| `mdkb_status` | Index status |
| `mdkb_update` | Trigger reindex |
| `mdkb_metrics` | Token usage statistics |

### Configuration

Create `.mdkb/config.toml`:

```toml
[mcp]
max_response_tokens = 4000
truncate_with_ellipsis = true
```

## Storage

- **Database**: `.mdkb/index.sqlite`
- **Config**: `.mdkb/config.toml`
- **Models**: `.mdkb/models/` (GGUF files for LLM)

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

## License

MIT
