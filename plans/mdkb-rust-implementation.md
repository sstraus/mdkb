# Implementation Plan: mdkb - Rust Markdown Knowledge Base

**Created:** 2026-01-31
**Status:** Draft
**Estimated Effort:** L (Large - multi-week project)

## Summary

Rust CLI tool and MCP server for local markdown knowledge base search. A Rust port of [qmd](https://github.com/tobi/qmd) with full parity and beyond:
- Improved platform compatibility (single cross-platform binary)
- Per-repo storage (`.mdkb/` directory)
- File watching in MCP mode for auto-updates
- Differential indexing based on file modification times
- Local LLM inference only (no external APIs)
- Smart exclusions (no skill files indexed)

## Design Decisions

### Storage Location
- **Per-repository**: `.mdkb/` directory in project root
- Contains: `index.sqlite`, `models/` (cached GGUF models), `config.toml`
- Gitignore: add `.mdkb/` to `.gitignore` template

### Database Choice: rusqlite + FTS5 (Confirmed)
| Requirement | Solution |
|-------------|----------|
| Full-text search | FTS5 with BM25 ranking |
| Metadata/tags | Structured SQL tables |
| Document relationships | Foreign keys, joins |
| Vector embeddings | sqlite-vec extension |
| Single file | SQLite database |
| Cross-platform | Bundled SQLite, no runtime deps |

**Why not alternatives:**
- redb + Tantivy: More complexity, manual index sync
- sled: Still beta, unstable format
- RocksDB: Overkill, larger binary

### File Watching (MCP Mode)
- Use `notify` crate for cross-platform file system events
- Watch all collection paths for changes
- Debounce rapid changes (100ms window)
- Auto-reindex on: create, modify, delete, rename
- Skip indexing for patterns: `**/SKILL.md`, `**/.claude/**`, `**/skills/**`

### Differential Indexing
- Store `modified_at` timestamp in documents table
- On startup/update: compare file mtime vs stored mtime
- Only reindex files where mtime differs
- Handle deletions: remove documents for missing files
- Handle renames: detect via content hash

### LLM Integration (Local Only)
- Use `llama-cpp-rs` for GGUF model inference
- Models stored in `.mdkb/models/`
- Auto-download on first use (with progress)
- Models:
  - Embeddings: `nomic-embed-text` or `bge-small-en`
  - Reranking: `bge-reranker-base`
- Lazy loading: only load when needed
- Unload after inactivity timeout (2 min)

### Exclusion Patterns
Default exclusions (not indexed):
```
**/SKILL.md
**/skills/**
**/.claude/**
**/.git/**
**/node_modules/**
**/target/**
**/.mdkb/**
```
Configurable via `.mdkb/config.toml`

## Research Findings

### qmd Architecture (Source: tobi/qmd)
- **Storage**: SQLite with FTS5 + sqlite-vec for vectors
- **Search modes**: BM25 (fast), vector (semantic), hybrid (query expansion + reranking)
- **LLM**: node-llama-cpp with GGUF models (EmbeddingGemma, Qwen3-Reranker)
- **MCP**: 6 tools via @modelcontextprotocol/sdk
- **Output**: JSON, CSV, MD, XML, files, terminal

### TypeScript Limitations Addressed
| Issue | qmd (TS) | mdkb (Rust) |
|-------|----------|-------------|
| Platform compat | Windows broken, macOS crashes | Single binary, cross-compile |
| Runtime deps | Bun-only, sqlite-vec alpha | No runtime, statically linked |
| Cold start | ~2min LLM load | Lazy loading, faster startup |
| Embedding batch | 53/53 failures | Robust error handling, retry logic |
| BM25 scoring | Returns 100% always | Native SQLite FTS5 |
| No file watching | Manual `update` command | Auto-watch in MCP mode |
| Global database | `~/.cache/qmd/` | Per-repo `.mdkb/` |

### Improvements Beyond qmd
| Feature | qmd | mdkb |
|---------|-----|------|
| File watching | No | Yes (MCP mode) |
| Differential indexing | Hash-based | mtime + hash |
| Skill exclusion | No | Yes (pattern-based) |
| Per-repo storage | No | Yes (`.mdkb/`) |
| Config format | YAML | TOML (Rust convention) |
| Chunking | Fixed 800 tokens | Configurable |
| Model selection | Hardcoded | Configurable |

### Rust Stack Decision
| Component | Crate | Rationale |
|-----------|-------|-----------|
| Storage | rusqlite | Sync API, FTS5 built-in, battle-tested |
| Vector storage | sqlite-vec | Same DB, proven by qmd |
| CLI | clap | Derive macros, subcommands |
| MCP | rmcp | Official Anthropic SDK |
| Async | tokio | Required for MCP server |
| File watching | notify | Cross-platform, mature |
| Serialization | serde | Industry standard |
| Errors | anyhow + thiserror | Ergonomic error handling |
| LLM | llama-cpp-rs | GGUF inference, battle-tested |
| Logging | tracing | Structured, async-compatible |

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        CLI (clap)                               │
│                       src/cli/mod.rs                            │
├─────────────────────────────────────────────────────────────────┤
│                     MCP Server (rmcp)                           │
│         + File Watcher (notify) for auto-updates                │
│                      src/mcp/mod.rs                             │
├─────────────┬───────────────────────┬───────────────────────────┤
│  Domain     │     Output            │      LLM Integration      │
│  Logic      │   Formatting          │   (local inference)       │
│ src/domain/ │  src/formatter.rs     │      src/llm/             │
├─────────────┴───────────────────────┴───────────────────────────┤
│                      Storage Layer                               │
│                      src/store/                                  │
├─────────────────────────────────────────────────────────────────┤
│              rusqlite + FTS5 + sqlite-vec                        │
│                      .mdkb/index.sqlite                          │
└─────────────────────────────────────────────────────────────────┘
```

## Implementation Phases

### Phase 1: Core Foundation
**Goal**: CLI with collections, BM25 search, differential indexing

#### Step 1.1: Project Structure
- [ ] Set up Cargo.toml with initial dependencies
- [ ] Create module structure (lib.rs, cli/, store/, domain/)
- [ ] Configure error handling with anyhow/thiserror
- [ ] Set up tracing for logging
- [ ] Create `.mdkb/` directory structure

**Files:**
- `Cargo.toml` - Dependencies and metadata
- `src/lib.rs` - Library root, re-exports
- `src/main.rs` - CLI entry point
- `src/error.rs` - Error types
- `src/config.rs` - Configuration management

#### Step 1.2: Storage Layer
- [ ] Implement rusqlite connection management
- [ ] Create schema with migrations
- [ ] Implement content-addressable storage (SHA256 hashing)
- [ ] Add FTS5 virtual table for full-text search
- [ ] Store file modification times for differential indexing

**Schema:**
```sql
-- Schema version for migrations
CREATE TABLE schema_version (
    version INTEGER PRIMARY KEY
);

-- Collections configuration
CREATE TABLE collections (
    name TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    pattern TEXT DEFAULT '**/*.md',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Content-addressable storage (deduplication)
CREATE TABLE content (
    hash TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Documents (file system mapping)
CREATE TABLE documents (
    id INTEGER PRIMARY KEY,
    collection TEXT NOT NULL,
    relative_path TEXT NOT NULL,  -- relative to collection path
    hash TEXT NOT NULL,
    title TEXT,
    file_modified_at INTEGER NOT NULL,  -- mtime from filesystem
    indexed_at INTEGER NOT NULL,
    FOREIGN KEY(collection) REFERENCES collections(name) ON DELETE CASCADE,
    FOREIGN KEY(hash) REFERENCES content(hash),
    UNIQUE(collection, relative_path)
);

-- Full-text search index
CREATE VIRTUAL TABLE documents_fts USING fts5(
    title,
    body,
    content='',  -- external content table
    content_rowid='id'
);

-- Triggers to keep FTS in sync
CREATE TRIGGER documents_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, title, body)
    SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
END;

CREATE TRIGGER documents_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, title, body)
    VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
END;

-- Vector embeddings (Phase 2)
-- CREATE VIRTUAL TABLE document_vectors USING vec0(...);

-- Context metadata for paths
CREATE TABLE contexts (
    id INTEGER PRIMARY KEY,
    path_pattern TEXT NOT NULL UNIQUE,
    context TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- LLM response cache
CREATE TABLE llm_cache (
    cache_key TEXT PRIMARY KEY,
    response TEXT NOT NULL,
    model TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Exclusion patterns
CREATE TABLE exclusions (
    pattern TEXT PRIMARY KEY
);

-- Default exclusions
INSERT INTO exclusions VALUES ('**/SKILL.md');
INSERT INTO exclusions VALUES ('**/skills/**');
INSERT INTO exclusions VALUES ('**/.claude/**');
INSERT INTO exclusions VALUES ('**/.git/**');
INSERT INTO exclusions VALUES ('**/node_modules/**');
INSERT INTO exclusions VALUES ('**/target/**');
INSERT INTO exclusions VALUES ('**/.mdkb/**');
```

**Files:**
- `src/store/mod.rs` - Store interface
- `src/store/schema.rs` - Migrations
- `src/store/documents.rs` - Document CRUD
- `src/store/search.rs` - FTS5 search
- `src/store/collections.rs` - Collection management

#### Step 1.3: Domain Logic
- [ ] Implement document indexing with exclusion patterns
- [ ] Implement differential indexing (mtime comparison)
- [ ] Extract titles from markdown (first H1 or filename)
- [ ] Implement BM25 search with snippets
- [ ] Implement document retrieval (get, mget)
- [ ] Add collection management (add, remove, list)

**Files:**
- `src/domain/mod.rs` - Domain interface
- `src/domain/indexer.rs` - File walking, differential logic
- `src/domain/search.rs` - Search orchestration
- `src/domain/collections.rs` - Collection operations
- `src/domain/exclusions.rs` - Pattern matching for exclusions

#### Step 1.4: CLI Interface
- [ ] Implement `init` command (create .mdkb/ in current dir)
- [ ] Implement collection subcommands (add, remove, list, rename)
- [ ] Implement search command with options
- [ ] Implement get/mget commands
- [ ] Implement status command (show index stats, stale files)
- [ ] Implement update command (differential reindex)
- [ ] Add output format flags (--json, --csv, --md, --xml)

**Commands:**
```
mdkb init                      # Initialize .mdkb/ in current directory
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection list
mdkb collection rename <old> <new>
mdkb search <query> [--limit N] [--collection NAME] [--json|--csv|--md|--xml]
mdkb get <docid|path> [--lines START:END]
mdkb mget <pattern>
mdkb status                    # Show index health, stale files count
mdkb update                    # Differential reindex
mdkb exclusion add <pattern>
mdkb exclusion remove <pattern>
mdkb exclusion list
```

**Files:**
- `src/cli/mod.rs` - Clap definitions
- `src/cli/init.rs` - Init command
- `src/cli/collection.rs` - Collection commands
- `src/cli/search.rs` - Search command
- `src/cli/get.rs` - Retrieval commands
- `src/cli/exclusion.rs` - Exclusion management
- `src/formatter.rs` - Output formatting

### Phase 2: MCP Server + File Watching

#### Step 2.1: MCP Server Setup
- [ ] Add rmcp dependency with server features
- [ ] Implement MCP tool definitions
- [ ] Add `serve` subcommand for MCP mode
- [ ] Configure stdio transport
- [ ] Logging to stderr only (critical for JSON-RPC)

**MCP Tools:**
```
mdkb_search      - BM25 full-text search
mdkb_vsearch     - Vector semantic search (Phase 3)
mdkb_query       - Hybrid search with reranking (Phase 4)
mdkb_get         - Single document retrieval (supports line ranges)
mdkb_multi_get   - Batch retrieval (glob patterns)
mdkb_status      - Index information and health
mdkb_update      - Trigger differential reindex
```

**Files:**
- `src/mcp/mod.rs` - MCP server entry
- `src/mcp/tools.rs` - Tool implementations
- `src/mcp/transport.rs` - Stdio transport

#### Step 2.2: File Watching
- [ ] Add notify crate for file system events
- [ ] Implement debounced event handling (100ms)
- [ ] Filter events based on exclusion patterns
- [ ] Auto-reindex on file changes
- [ ] Handle edge cases (rapid saves, large batches)

**Files:**
- `src/watcher/mod.rs` - File watcher implementation
- `src/watcher/debounce.rs` - Event debouncing

#### Step 2.3: Dual-Mode Binary
- [ ] Detect invocation mode (CLI vs MCP)
- [ ] Configure logging for each mode (stderr for MCP)
- [ ] Add graceful shutdown handling
- [ ] Start file watcher in MCP mode

### Phase 3: Semantic Search (Local LLM)

#### Step 3.1: LLM Infrastructure
- [ ] Add llama-cpp-rs dependency
- [ ] Implement model manager (download, cache, load)
- [ ] Add sqlite-vec for vector storage
- [ ] Implement lazy loading with timeout

**Model Configuration:**
```toml
# .mdkb/config.toml
[models]
embedding = "nomic-embed-text-v1.5.Q4_K_M.gguf"
reranker = "bge-reranker-base.Q4_K_M.gguf"
inactivity_timeout_secs = 120

[chunking]
max_tokens = 512
overlap_tokens = 64
```

**Files:**
- `src/llm/mod.rs` - LLM interface
- `src/llm/manager.rs` - Model lifecycle
- `src/llm/embeddings.rs` - Embedding generation
- `src/llm/download.rs` - Model downloading with progress

#### Step 3.2: Vector Embeddings
- [ ] Implement document chunking (configurable)
- [ ] Generate embeddings for all chunks
- [ ] Store in sqlite-vec
- [ ] Implement `embed` command for manual generation
- [ ] Add progress reporting

#### Step 3.3: Vector Search
- [ ] Implement vsearch command
- [ ] Add cosine similarity search via sqlite-vec
- [ ] Integrate with MCP tools
- [ ] Add `--threshold` option for minimum similarity

### Phase 4: Hybrid Search

#### Step 4.1: Query Expansion
- [ ] Implement query expansion prompts
- [ ] Generate lexical and semantic variations
- [ ] Cache expansions in llm_cache table

#### Step 4.2: Hybrid Pipeline
- [ ] Implement Reciprocal Rank Fusion (RRF)
- [ ] Add position bonuses for high-confidence matches
- [ ] Implement LLM reranking with reranker model
- [ ] Create `query` command

### Phase 5: Context System

#### Step 5.1: Context Metadata
- [ ] Implement `context add <path> <text>` command
- [ ] Implement `context remove <path>` command
- [ ] Implement `context list` command
- [ ] Implement `context check <path>` command
- [ ] Apply context to search results

## Acceptance Criteria

### Phase 1 (Core)
- [ ] `mdkb init` creates `.mdkb/` directory
- [ ] Single binary works on Linux, macOS, Windows
- [ ] Can add/remove collections of markdown files
- [ ] Skill files and exclusion patterns are skipped
- [ ] Differential indexing based on mtime works
- [ ] BM25 search returns ranked results with snippets
- [ ] Document retrieval works by path and docid
- [ ] Output formats: terminal, JSON, CSV, MD, XML
- [ ] All tests passing

### Phase 2 (MCP + Watching)
- [ ] `mdkb serve` runs as MCP server over stdio
- [ ] All MCP tools functional
- [ ] File watcher auto-reindexes on changes
- [ ] Watcher respects exclusion patterns
- [ ] Works with Claude Desktop and Claude Code
- [ ] Graceful error handling

### Phase 3 (Semantic)
- [ ] Models auto-download on first use
- [ ] Embedding generation works reliably
- [ ] vsearch returns semantically similar documents
- [ ] Models unload after inactivity

### Phase 4 (Hybrid)
- [ ] Query expansion generates useful variations
- [ ] Hybrid search combines BM25 + vector results
- [ ] Reranking improves result quality

## Security Considerations

- Path traversal: Validate all paths stay within collection roots
- No network calls except model downloads
- Models downloaded from trusted sources (Hugging Face)
- No external API keys required

## Performance Considerations

- Lazy loading: Don't open DB until needed
- Differential indexing: Only re-index changed files (mtime check)
- Streaming output: Don't load all results into memory
- FTS5 already optimized for BM25
- Model lazy loading: Only load when semantic features used
- Debounced file watching: Don't thrash on rapid saves

## File Structure

```
project/
├── .mdkb/                    # Per-repo data directory
│   ├── index.sqlite          # Database
│   ├── config.toml           # Configuration
│   └── models/               # Cached GGUF models
│       ├── nomic-embed-text-v1.5.Q4_K_M.gguf
│       └── bge-reranker-base.Q4_K_M.gguf
└── ...project files...
```

```
src/
├── main.rs              # Entry point - mode detection
├── lib.rs               # Library root
├── error.rs             # Error types
├── config.rs            # Configuration management
├── cli/
│   ├── mod.rs           # Clap definitions
│   ├── init.rs          # Init command
│   ├── collection.rs    # Collection commands
│   ├── search.rs        # Search command
│   ├── get.rs           # Retrieval commands
│   └── exclusion.rs     # Exclusion commands
├── store/
│   ├── mod.rs           # Store interface
│   ├── schema.rs        # Migrations
│   ├── documents.rs     # Document CRUD
│   ├── search.rs        # FTS5 search
│   ├── vectors.rs       # sqlite-vec operations
│   └── collections.rs   # Collection management
├── domain/
│   ├── mod.rs           # Domain interface
│   ├── indexer.rs       # File walking, differential
│   ├── search.rs        # Search orchestration
│   ├── collections.rs   # Collection operations
│   └── exclusions.rs    # Pattern matching
├── mcp/
│   ├── mod.rs           # MCP server
│   └── tools.rs         # Tool implementations
├── watcher/
│   ├── mod.rs           # File watcher
│   └── debounce.rs      # Event debouncing
├── llm/
│   ├── mod.rs           # LLM interface
│   ├── manager.rs       # Model lifecycle
│   ├── embeddings.rs    # Embedding generation
│   └── download.rs      # Model downloading
└── formatter.rs         # Output formatting
```

## Dependencies (Cargo.toml)

```toml
[package]
name = "mdkb"
version = "0.1.0"
edition = "2024"
description = "Local markdown knowledge base with semantic search"
license = "MIT"

[dependencies]
# CLI
clap = { version = "4", features = ["derive", "env"] }

# Storage
rusqlite = { version = "0.32", features = ["bundled", "fts5"] }
sqlite-vec = "0.1"

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"

# Async (for MCP)
tokio = { version = "1", features = ["full"] }

# MCP
rmcp = { version = "0.13", features = ["server", "transport-io"] }

# File watching
notify = "6"
notify-debouncer-mini = "0.4"

# Errors
anyhow = "1"
thiserror = "1"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Utilities
sha2 = "0.10"
walkdir = "2"
glob = "0.3"
globset = "0.4"
chrono = { version = "0.4", features = ["serde"] }
directories = "5"       # XDG paths for model cache

# LLM (Phase 3)
llama-cpp-rs = { version = "0.4", optional = true }
indicatif = "0.17"     # Progress bars

[features]
default = []
llm = ["llama-cpp-rs"]

[dev-dependencies]
tempfile = "3"
assert_cmd = "2"
predicates = "3"
```

## Configuration File (.mdkb/config.toml)

```toml
# mdkb configuration

[indexing]
# Default glob pattern for markdown files
default_pattern = "**/*.md"

# Debounce timeout for file watcher (milliseconds)
debounce_ms = 100

[models]
# Embedding model (GGUF format)
embedding = "nomic-embed-text-v1.5.Q4_K_M.gguf"

# Reranker model (GGUF format)
reranker = "bge-reranker-base.Q4_K_M.gguf"

# Unload models after this many seconds of inactivity
inactivity_timeout_secs = 120

[chunking]
# Maximum tokens per chunk
max_tokens = 512

# Overlap between chunks (tokens)
overlap_tokens = 64

[search]
# Default result limit
default_limit = 10

# Minimum relevance score (0.0 - 1.0)
min_score = 0.0
```

---

## Next Steps

When ready to implement, run:
- `/wiz:work plans/mdkb-rust-implementation.md` - Execute Phase 1
- Iterate through phases as needed

---

*Generated with Claude Code via Happy*
