# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

mdkb is a Rust CLI tool and MCP server for local markdown knowledge base search. It's a Rust port of [qmd](https://github.com/tobi/qmd) with improvements:
- Per-repo storage (`.mdkb/` directory)
- File watching in MCP mode for auto-updates
- Differential indexing based on file modification times
- Local LLM inference only (no external APIs)
- Smart exclusions (skill files not indexed)

## Build & Development Commands

```bash
# Build
cargo build                    # Debug build
cargo build --release          # Release build
cargo build --features llm     # With LLM support

# Run
cargo run -- <args>            # Run with arguments
cargo run -- init              # Initialize .mdkb/ in current dir
cargo run -- collection add docs ./docs
cargo run -- search "query"
cargo run -- serve             # MCP server mode

# Test
cargo test                     # Run all tests
cargo test <test_name>         # Run specific test
cargo test -- --nocapture      # Show println! output

# Lint & Format
cargo fmt                      # Format code
cargo clippy                   # Lint
cargo check                    # Fast compile check
```

## Architecture

```
src/
├── main.rs          # Entry point - CLI/MCP mode detection
├── lib.rs           # Library root
├── error.rs         # Error types (thiserror)
├── config.rs        # TOML configuration management
├── cli/             # CLI layer (clap)
├── store/           # Storage layer (rusqlite + FTS5 + sqlite-vec)
├── domain/          # Business logic (hexagonal core)
├── mcp/             # MCP server (rmcp) + file watcher
├── watcher/         # File system watcher (notify)
├── llm/             # Local LLM inference (llama-cpp-rs)
└── formatter.rs     # Output formatting (JSON, CSV, MD, XML)
```

**Key patterns:**
- Hexagonal architecture: domain logic independent of CLI/MCP/storage
- Content-addressable storage: SHA256 hashes for deduplication
- FTS5 for full-text search with BM25 ranking
- Differential indexing: mtime comparison to skip unchanged files

## Storage

- **Location**: `.mdkb/` in project root (per-repo)
- **Database**: `.mdkb/index.sqlite`
- **Config**: `.mdkb/config.toml`
- **Models**: `.mdkb/models/` (GGUF files)

## Default Exclusions (not indexed)

```
**/SKILL.md
**/skills/**
**/.claude/**
**/.git/**
**/node_modules/**
**/target/**
**/.mdkb/**
```

## CLI Commands

```bash
mdkb init                      # Initialize .mdkb/ in current directory
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection list
mdkb search <query> [--limit N] [--collection NAME] [--json|--csv|--md|--xml]
mdkb vsearch <query>           # Vector semantic search
mdkb query <query>             # Hybrid search with reranking
mdkb get <docid|path>
mdkb mget <pattern>
mdkb status
mdkb update                    # Differential reindex
mdkb serve                     # MCP server mode (with file watching)
```

## MCP Server

Run as MCP server: `mdkb serve`

**Tools exposed:**
- `mdkb_search` - BM25 full-text search
- `mdkb_vsearch` - Vector semantic search
- `mdkb_query` - Hybrid search with reranking
- `mdkb_get` - Document retrieval (supports line ranges)
- `mdkb_multi_get` - Batch retrieval
- `mdkb_status` - Index status
- `mdkb_update` - Trigger reindex

**File watching:** In MCP mode, automatically watches collection paths and reindexes on changes (debounced 100ms).

**Critical:** All logs MUST go to stderr, never stdout (corrupts JSON-RPC).

## Dependencies

- **clap**: CLI argument parsing
- **rusqlite**: SQLite with FTS5
- **sqlite-vec**: Vector storage
- **rmcp**: MCP protocol (official SDK)
- **notify**: File system watching
- **tokio**: Async runtime for MCP
- **llama-cpp-rs**: Local LLM inference (feature-gated)
- **serde**: Serialization
- **tracing**: Structured logging

## Testing

Tests use `tempfile` for isolated database instances.

```rust
#[test]
fn test_search() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join(".mdkb/index.sqlite")).unwrap();
    // ...
}
```
