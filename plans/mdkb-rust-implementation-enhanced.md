# Implementation Plan: mdkb - Rust Markdown Knowledge Base (Enhanced)

**Created:** 2026-01-31
**Enhanced:** 2026-01-31
**Status:** Draft (Research-Enhanced)
**Estimated Effort:** L (Large - multi-week project)

---

## Enhancement Summary

**Research Performed:** 2026-01-31
**Agents Deployed:** 7
**Technologies Researched:** SQLite FTS5, sqlite-vec, vectorlite, LanceDB, RRF, chunking strategies, MCP specification, Serena MCP, journaling systems, knowledge evolution (RFC supersedes pattern), search quality metrics (BEIR, MTEB, nDCG)

### Key Discoveries

1. **Markdown-aware chunking** significantly improves retrieval quality vs fixed-size chunks
2. **FTS5 column weighting** (title 10x, body 1x) improves ranking with zero effort
3. **Serena's warmup pattern** (auto-init on serve, project root discovery) improves UX
4. **MCP tool naming** should be clearer: `vsearch` → `semantic_search`, `query` → `hybrid_search`
5. **Frontmatter/YAML parsing** and **wiki-link backlinks** are table-stakes for KB tools
6. **Weighted RRF** is more tunable than plain RRF
7. **RFC supersedes pattern** is the gold standard for knowledge evolution tracking
8. **wiz:journal pattern** (daily + topics + problems dirs, status tracking) works well for hybrid access
9. **nDCG@10 and latency percentiles** are key metrics for search self-evaluation

### New Risks Identified

| Risk | Severity | Mitigation |
|------|----------|------------|
| sqlite-vec O(n) scaling | Medium | Consider LanceDB (pure Rust, ANN) for >10k docs |
| No fuzzy matching | Medium | Add trigram tokenizer or strsim preprocessing |
| Missing frontmatter support | High | Add YAML parsing with `serde_yaml` |
| Journal entry sprawl | Medium | Archive strategy + reflection consolidation |
| Supersession complexity | Low | Start with manual, add auto-detect later |

---

## Summary

Rust CLI tool and MCP server for local markdown knowledge base search. A Rust port of [qmd](https://github.com/tobi/qmd) with full parity and beyond:
- Improved platform compatibility (single cross-platform binary)
- Per-repo storage (`.mdkb/` directory)
- File watching in MCP mode for auto-updates
- Differential indexing based on file modification times
- Local LLM inference only (no external APIs)
- Smart exclusions (no skill files indexed)
- **NEW: Frontmatter/tag extraction**
- **NEW: Wiki-link and backlink tracking**
- **NEW: Markdown-aware chunking**
- **NEW: Auto-init warmup on serve**
- **NEW: Journaling system** - Hybrid markdown/indexed problem-solution tracking
- **NEW: Knowledge evolution** - Supersedes pattern for structured knowledge versioning
- **NEW: Self-evaluation metrics** - Latency, quality, and efficiency tracking

---

## Design Decisions

### Storage Location
- **Per-repository**: `.mdkb/` directory in project root
- Contains: `index.sqlite`, `models/` (cached GGUF models), `config.toml`
- Gitignore: add `.mdkb/` to `.gitignore` template

### Database Choice: rusqlite + FTS5 (Confirmed)
| Requirement | Solution |
|-------------|----------|
| Full-text search | FTS5 with BM25 ranking + **porter stemmer** |
| Metadata/tags | Structured SQL tables + **frontmatter JSON** |
| Document relationships | Foreign keys, joins + **links table for backlinks** |
| Vector embeddings | sqlite-vec extension |
| Single file | SQLite database |
| Cross-platform | Bundled SQLite, no runtime deps |

### Enhancement: FTS5 Optimization

**Research Finding:** FTS5 supports column weighting and tokenizer selection.

```sql
-- Use porter stemmer for better English recall
CREATE VIRTUAL TABLE documents_fts USING fts5(
    title,
    body,
    tokenize = 'porter unicode61',
    content='',
    content_rowid='id'
);

-- Set column weights: title 10x more important than body
INSERT INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');
```

**Performance PRAGMAs to add:**
```sql
PRAGMA mmap_size = 1073741824;  -- 1GB mmap for large DBs
PRAGMA journal_mode = WAL;
PRAGMA temp_store = memory;
```

### File Watching (MCP Mode)
- Use `notify` crate for cross-platform file system events
- Watch all collection paths for changes
- Debounce rapid changes (100ms window)
- Auto-reindex on: create, modify, delete, rename
- Skip indexing for patterns: `**/SKILL.md`, `**/.claude/**`, `**/skills/**`

### Enhancement: Warmup Pattern (from Serena MCP research)

**Research Finding:** Serena uses auto-init and project root discovery for seamless warmup.

On `mdkb serve`:
1. Walk up from CWD looking for `.mdkb/config.toml` or `.git`
2. If no `.mdkb/` exists, auto-initialize (like `mdkb init`)
3. Pre-load FTS index into memory
4. Verify all collections are indexed, trigger incremental update if stale
5. Start file watcher

**New startup behavior:**
```rust
async fn serve() -> Result<()> {
    // 1. Auto-discover project root
    let root = find_project_root()?;  // Walk up looking for .mdkb or .git

    // 2. Auto-init if needed
    if !root.join(".mdkb").exists() {
        init_mdkb(&root)?;
        // Auto-add current directory as default collection
        add_collection("default", ".", "**/*.md")?;
    }

    // 3. Warmup: verify index freshness
    let status = get_status()?;
    if status.stale_files > 0 {
        tracing::info!("Warming up: indexing {} stale files", status.stale_files);
        update_index()?;
    }

    // 4. Start MCP server with watcher
    start_mcp_with_watcher().await
}
```

### Differential Indexing
- Store `modified_at` timestamp in documents table
- On startup/update: compare file mtime vs stored mtime
- Only reindex files where mtime differs
- Handle deletions: remove documents for missing files
- Handle renames: detect via content hash
- **NEW:** Track chunk hashes separately for incremental embedding updates

### LLM Integration (Local Only)
- Use `llama-cpp-rs` for GGUF model inference
- Use `hf-hub` crate for runtime model download from HuggingFace
- Models cached in global HuggingFace cache (`~/.cache/huggingface/hub/`)
- Auto-download on first use (with progress bar via `indicatif`)
- Models:
  - **Embeddings:** `nomic-ai/nomic-embed-text-v1.5-GGUF` Q4_K_M (~250MB)
  - **Reranking:** `BAAI/bge-reranker-base` Q4_K_M (~150MB)
  - **Condensation:** `bartowski/Llama-3.2-3B-Instruct-GGUF` Q4_K_M (~2GB)
- Lazy loading: only load when needed
- Unload after inactivity timeout (2 min)
- Feature-gated: `--features llm` enables all LLM functionality

**Model download pattern:**
```rust
use hf_hub::api::sync::Api;

fn download_model(repo: &str, filename: &str) -> Result<PathBuf> {
    let api = Api::new()?;
    let model_path = api
        .model(repo.to_string())
        .get(filename)?;
    Ok(model_path)
}

// Example: download embedding model
let embed_path = download_model(
    "nomic-ai/nomic-embed-text-v1.5-GGUF",
    "nomic-embed-text-v1.5.Q4_K_M.gguf"
)?;
```

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

---

## Enhancement: Missing Features (Gap Analysis)

### Must-Have for Parity

#### 1. Frontmatter/YAML Parsing
**Gap:** Original plan doesn't extract YAML frontmatter.

**Solution:** Parse frontmatter with `serde_yaml` and store as JSON in `metadata` column.

```sql
ALTER TABLE documents ADD COLUMN metadata TEXT;  -- JSON blob
```

```rust
use gray_matter::Matter;

fn parse_frontmatter(content: &str) -> (Option<serde_json::Value>, &str) {
    let matter = Matter::<YAML>::new();
    let result = matter.parse(content);
    (result.data, result.content)
}
```

**New dependency:** Add `gray-matter = "0.2"` or use `serde_yaml` directly.

#### 2. Wiki-Links and Backlinks
**Gap:** No link extraction or backlink tracking.

**Solution:** Add `links` table and parse `[[wiki-links]]` during indexing.

```sql
CREATE TABLE links (
    id INTEGER PRIMARY KEY,
    source_doc_id INTEGER NOT NULL,
    target_path TEXT NOT NULL,  -- The [[link target]]
    link_text TEXT,              -- Display text if different
    line_number INTEGER,
    FOREIGN KEY(source_doc_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX idx_links_target ON links(target_path);
```

**New queries enabled:**
- `mdkb backlinks <path>` - Find all documents linking to this file
- Search results can show "linked by N documents"

#### 3. Cleanup Command
**Gap:** No command to remove orphaned content entries.

**Solution:** Add `mdkb cleanup` command.

```sql
DELETE FROM content WHERE hash NOT IN (SELECT hash FROM documents);
```

#### 4. Collection Rename
**Gap:** Listed in CLI but not in implementation steps.

**Solution:** Ensure `mdkb collection rename <old> <new>` is implemented.

### Should-Have for Competitive Advantage

#### 5. Search Filters (tag:, path:)
**Gap:** No query syntax for filtering.

**Solution:** Parse query for operators before FTS5 search.

```rust
fn parse_query(query: &str) -> ParsedQuery {
    // Extract: tag:rust path:docs/* "actual search"
    ParsedQuery {
        tags: extract_operator(query, "tag:"),
        paths: extract_operator(query, "path:"),
        collections: extract_operator(query, "collection:"),
        text: remaining_text,
    }
}
```

#### 6. Heading Structure Indexing
**Gap:** Only extracts first H1 as title.

**Solution:** Store heading hierarchy for section-level search.

```sql
CREATE TABLE headings (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL,
    level INTEGER NOT NULL,  -- 1-6
    text TEXT NOT NULL,
    line_number INTEGER NOT NULL,
    FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE
);
```

**Enables:** "Find the ## Testing section in any doc"

---

## Enhancement: Chunking Strategy

### Research Finding: Markdown-Aware Chunking

**Current plan:** Fixed 512 tokens with 64 token overlap.

**Problem:** Splits mid-section, loses context.

**Recommendation:** Implement markdown-aware chunking as default.

```rust
enum ChunkStrategy {
    Fixed { max_tokens: usize, overlap: usize },
    Markdown { max_tokens: usize, respect_headers: bool },
    Semantic { max_tokens: usize },  // Future: requires embedding model
}

fn chunk_markdown(content: &str, max_tokens: usize) -> Vec<Chunk> {
    // 1. Split on ## headers first
    // 2. If section > max_tokens, split on paragraphs
    // 3. If paragraph > max_tokens, fall back to token-based with overlap
    // 4. Store section_path in chunk metadata: "## Architecture > ### Storage"
}
```

**Updated schema:**
```sql
CREATE TABLE chunks (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL,
    section_path TEXT,  -- "## Architecture > ### Storage"
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    hash TEXT NOT NULL,  -- For incremental embedding updates
    FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE,
    UNIQUE(document_id, chunk_index)
);

-- Vector embeddings reference chunks, not documents
CREATE VIRTUAL TABLE chunk_vectors USING vec0(
    chunk_id INTEGER PRIMARY KEY,
    embedding FLOAT[384]
);
```

**Configuration:**
```toml
[chunking]
strategy = "markdown"  # fixed, markdown, semantic
max_tokens = 512
overlap_tokens = 64
respect_headers = true
include_header_path = true
```

---

## Enhancement: Hybrid Search (Weighted RRF)

### Research Finding: Weighted RRF is More Tunable

**Current plan:** Plain RRF with k=60.

**Improvement:** Add configurable weights per ranking source.

```rust
fn weighted_rrf(
    bm25_results: &[SearchResult],
    vec_results: &[SearchResult],
    config: &SearchConfig,
) -> Vec<SearchResult> {
    // score = w_bm25 * (1 / (k + rank_bm25)) + w_vec * (1 / (k + rank_vec))
    let mut scores: HashMap<DocId, f64> = HashMap::new();

    for (rank, result) in bm25_results.iter().enumerate() {
        let score = config.bm25_weight * (1.0 / (config.rrf_k as f64 + rank as f64));
        *scores.entry(result.id).or_default() += score;
    }

    for (rank, result) in vec_results.iter().enumerate() {
        let score = config.vector_weight * (1.0 / (config.rrf_k as f64 + rank as f64));
        *scores.entry(result.id).or_default() += score;
    }

    // Sort by combined score
    ...
}
```

**Configuration:**
```toml
[search]
rrf_k = 60
bm25_weight = 1.0
vector_weight = 0.7
rerank_top_k = 50
```

---

## Enhancement: MCP Tool Design

### Research Finding: Tool Naming and Descriptions Matter

**Rename for clarity:**

| Current | New | Rationale |
|---------|-----|-----------|
| `mdkb_vsearch` | `mdkb_semantic_search` | "vsearch" is cryptic |
| `mdkb_query` | `mdkb_hybrid_search` | "query" is too generic |
| `mdkb_multi_get` | `mdkb_batch_get` | "batch" is more standard |
| `mdkb_update` | `mdkb_reindex` | "update" is ambiguous |

**Improved descriptions (include when/when-not guidance):**

```rust
Tool {
    name: "mdkb_search",
    title: "Knowledge Base Keyword Search",
    description: "Performs keyword-based full-text search using BM25 ranking. \
        Use for exact term matching, technical queries, or when you know the \
        specific words that should appear in documents. Returns results ranked \
        by term frequency. Prefer this over semantic search for code, exact \
        phrases, or technical terminology.",
    ...
}

Tool {
    name: "mdkb_semantic_search",
    title: "Knowledge Base Semantic Search",
    description: "Performs vector similarity search to find conceptually related \
        documents regardless of exact wording. Use when the user's question is \
        conceptual or uses different terminology than the documents. Requires \
        embedding model. Slower than keyword search but finds semantically \
        similar content.",
    ...
}

Tool {
    name: "mdkb_hybrid_search",
    title: "Knowledge Base Hybrid Search",
    description: "Combines keyword (BM25) and semantic search with LLM reranking \
        for highest quality results. Use as the default for complex questions. \
        Most comprehensive but slowest. Falls back gracefully if embedding \
        model unavailable.",
    ...
}
```

**Add MCP annotations (Nov 2025 spec):**
```rust
Tool {
    name: "mdkb_reindex",
    annotations: ToolAnnotations {
        read_only: false,
        destructive: false,
        idempotent: true,
    },
    ...
}
```

---

## Enhancement: SKILL.md for Claude Code Integration

**File:** `.claude/skills/mdkb/SKILL.md`

```yaml
---
name: mdkb
description: Search and retrieve documents from the local markdown knowledge base. Use when the user asks about project documentation, needs to find specific files, or wants to understand how something works in the codebase.
---

# mdkb - Markdown Knowledge Base

Search and retrieve documents from the locally indexed markdown knowledge base.

## When to Use

- User asks "how does X work?" or "where is Y documented?"
- Need to find specific markdown files by content
- Looking for documentation, READMEs, or design docs
- User references docs, notes, or markdown content

## Available Tools

### Searching

1. **mdkb_search** - Keyword search (BM25)
   - Best for: exact terms, code references, technical queries
   - Example: `mdkb_search("authentication middleware")`

2. **mdkb_semantic_search** - Conceptual similarity
   - Best for: "how do I...", conceptual questions
   - Example: `mdkb_semantic_search("handle user login flow")`

3. **mdkb_hybrid_search** - Combined search with reranking
   - Best for: complex questions, highest quality results
   - Example: `mdkb_hybrid_search("database migration strategy")`

### Retrieval

4. **mdkb_get** - Single document by path or ID
   - Supports line ranges: `mdkb_get("docs/api.md:50-100")`

5. **mdkb_batch_get** - Multiple documents by glob pattern
   - Example: `mdkb_batch_get("docs/**/*.md")`

### Maintenance

6. **mdkb_status** - Check index health and stale file count
7. **mdkb_reindex** - Trigger incremental update

## Search Strategy

1. Start with `mdkb_search` for specific terms
2. If results are poor, try `mdkb_semantic_search`
3. For important queries, use `mdkb_hybrid_search`
4. Always check `mdkb_status` if searches return unexpected results

## Query Syntax

Supports FTS5 operators:
- Boolean: `rust AND async`, `rust OR go`, `NOT deprecated`
- Phrases: `"error handling"`
- Prefix: `auth*`
- Filters: `tag:rust path:docs/*` (when supported)
```

---

## Updated Schema (with enhancements)

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
    relative_path TEXT NOT NULL,
    hash TEXT NOT NULL,
    title TEXT,
    metadata TEXT,  -- NEW: JSON frontmatter
    file_modified_at INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL,
    FOREIGN KEY(collection) REFERENCES collections(name) ON DELETE CASCADE,
    FOREIGN KEY(hash) REFERENCES content(hash),
    UNIQUE(collection, relative_path)
);

-- NEW: Document tags (extracted from frontmatter)
CREATE TABLE document_tags (
    document_id INTEGER NOT NULL,
    tag TEXT NOT NULL,
    FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE,
    PRIMARY KEY(document_id, tag)
);
CREATE INDEX idx_tags ON document_tags(tag);

-- NEW: Wiki-links between documents
CREATE TABLE links (
    id INTEGER PRIMARY KEY,
    source_doc_id INTEGER NOT NULL,
    target_path TEXT NOT NULL,
    link_text TEXT,
    line_number INTEGER,
    FOREIGN KEY(source_doc_id) REFERENCES documents(id) ON DELETE CASCADE
);
CREATE INDEX idx_links_target ON links(target_path);
CREATE INDEX idx_links_source ON links(source_doc_id);

-- NEW: Heading structure
CREATE TABLE headings (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL,
    level INTEGER NOT NULL,
    text TEXT NOT NULL,
    line_number INTEGER NOT NULL,
    FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE
);
CREATE INDEX idx_headings_doc ON headings(document_id);

-- Full-text search index (ENHANCED: porter stemmer + column weights)
CREATE VIRTUAL TABLE documents_fts USING fts5(
    title,
    body,
    tokenize = 'porter unicode61',
    content='',
    content_rowid='id'
);

-- Set column weights: title 10x, body 1x
INSERT INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');

-- Triggers to keep FTS in sync
CREATE TRIGGER documents_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, title, body)
    SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
END;

CREATE TRIGGER documents_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, title, body)
    VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
END;

-- NEW: Chunks table for Phase 3 (better than document-level vectors)
CREATE TABLE chunks (
    id INTEGER PRIMARY KEY,
    document_id INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL,
    section_path TEXT,
    start_line INTEGER NOT NULL,
    end_line INTEGER NOT NULL,
    hash TEXT NOT NULL,
    FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE,
    UNIQUE(document_id, chunk_index)
);

-- Vector embeddings reference chunks
-- CREATE VIRTUAL TABLE chunk_vectors USING vec0(
--     chunk_id INTEGER PRIMARY KEY,
--     embedding FLOAT[384]
-- );

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

---

## Updated Configuration (.mdkb/config.toml)

```toml
# mdkb configuration

[indexing]
default_pattern = "**/*.md"
debounce_ms = 100
# NEW: Extract frontmatter YAML
parse_frontmatter = true
# NEW: Extract wiki-links [[target]]
parse_wikilinks = true
# NEW: Index heading structure
index_headings = true

[chunking]
# NEW: Chunking strategy
strategy = "markdown"  # fixed, markdown, semantic
max_tokens = 512
overlap_tokens = 64
# NEW: Include header hierarchy in chunk context
include_header_path = true

[models]
# HuggingFace repos - auto-downloaded on first use
embedding_repo = "nomic-ai/nomic-embed-text-v1.5-GGUF"
embedding_file = "nomic-embed-text-v1.5.Q4_K_M.gguf"

reranker_repo = "BAAI/bge-reranker-base-GGUF"
reranker_file = "bge-reranker-base.Q4_K_M.gguf"

condense_repo = "bartowski/Llama-3.2-3B-Instruct-GGUF"
condense_file = "Llama-3.2-3B-Instruct-Q4_K_M.gguf"

inactivity_timeout_secs = 120

[search]
default_limit = 10
min_score = 0.0
# NEW: Weighted RRF configuration
rrf_k = 60
bm25_weight = 1.0
vector_weight = 0.7
rerank_top_k = 50
```

---

## Updated CLI Commands

```
mdkb init                      # Initialize .mdkb/ in current directory
mdkb collection add <name> <path> [--pattern <glob>]
mdkb collection remove <name>
mdkb collection list
mdkb collection rename <old> <new>

mdkb search <query> [--limit N] [--collection NAME] [--tag TAG] [--json|--csv|--md|--xml]
mdkb get <docid|path> [--lines START:END]
mdkb mget <pattern>

# NEW: Backlinks
mdkb backlinks <path>          # Show documents linking to this file

# NEW: Tags
mdkb tags [--collection NAME]  # List all tags
mdkb tagged <tag>              # Find documents with tag

mdkb status                    # Show index health, stale files count
mdkb update                    # Differential reindex
mdkb cleanup                   # NEW: Remove orphaned content

mdkb exclusion add <pattern>
mdkb exclusion remove <pattern>
mdkb exclusion list

mdkb serve                     # MCP server mode (auto-init, auto-warmup)
```

---

## Updated Dependencies (Cargo.toml)

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
serde_yaml = "0.9"  # NEW: Frontmatter parsing
toml = "0.8"

# NEW: Frontmatter extraction
gray-matter = "0.2"

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
directories = "5"

# NEW: Markdown parsing for chunking and link extraction
pulldown-cmark = "0.10"

# NEW: Regex for wiki-link extraction
regex = "1"

# LLM (Phase 3+)
llama-cpp-rs = { version = "0.4", optional = true }
hf-hub = { version = "0.3", optional = true }  # HuggingFace model download
indicatif = "0.17"  # Progress bars for model download

[features]
default = []
llm = ["llama-cpp-rs", "hf-hub"]

[dev-dependencies]
tempfile = "3"
assert_cmd = "2"
predicates = "3"
```

---

## Implementation Priority (Updated)

### Phase 1: Core Foundation (with enhancements)

**Additional tasks:**
- [ ] Add porter stemmer to FTS5 configuration
- [ ] Add column weighting for BM25
- [ ] Implement frontmatter parsing with `gray-matter`
- [ ] Add `metadata` column and `document_tags` table
- [ ] Implement wiki-link extraction with regex
- [ ] Add `links` table for backlinks
- [ ] Implement `mdkb cleanup` command
- [ ] Implement `mdkb collection rename`

**New files:**
- `src/domain/frontmatter.rs` - YAML parsing
- `src/domain/links.rs` - Wiki-link extraction
- `src/domain/tags.rs` - Tag operations

### Phase 2: MCP Server (with enhancements)

**Additional tasks:**
- [ ] Implement auto-init on `mdkb serve` (Serena pattern)
- [ ] Implement project root discovery
- [ ] Add warmup: verify index freshness on startup
- [ ] Rename tools for clarity
- [ ] Improve tool descriptions with when/when-not guidance
- [ ] Create `.claude/skills/mdkb/SKILL.md`

### Phase 3: Semantic Search (with enhancements)

**Additional tasks:**
- [ ] Implement markdown-aware chunking
- [ ] Store chunks with `section_path`
- [ ] Track chunk hashes for incremental embedding updates
- [ ] Embed chunks instead of whole documents

### Phase 4: Hybrid Search (with enhancements)

**Additional tasks:**
- [ ] Implement weighted RRF instead of plain RRF
- [ ] Make fusion weights configurable
- [ ] Add HyDE query expansion option

---

## NEW: Phase 6 - Memory Index System

### Design Philosophy: Focused AI Memory

**Problem with generic journaling:**
- Dumps too much content into AI context window
- No distinction between "knowing something exists" vs "retrieving full content"
- Full-text search returns content, not awareness

**Solution: Two-tier memory system**

1. **Summary Index** (warmup) - Lightweight manifest AI receives at session start
2. **Full Content** (on-demand) - Retrieved only when AI needs details

This is fundamentally different from search. Search answers "find X". Memory index answers "what do I know about?" then "tell me more about Y".

### Design: Memory Index

**Core concept:** Every memory entry has:
- **Title** (concise, max 50 chars - loaded in warmup top 50)
- **Full content** (retrieved on demand via `mdkb_memory_get`)
- **Tags** (for filtering and search)
- **Access count** (determines ranking in warmup index)

**Directory structure:**
```
.mdkb/
├── index.sqlite
├── config.toml
└── memory/
    ├── index.json           # Summary manifest (loaded on warmup)
    ├── entries/             # Full content files
    │   ├── auth-oauth2-flow.md
    │   ├── bug-null-pointer-users.md
    │   └── decision-use-sqlx.md
    └── archive/             # Condensed old entries
```

**index.json structure (what AI sees on warmup):**
```json
{
  "updated": "2026-01-31T14:30:00Z",
  "entries": [
    "auth-oauth2-flow: OAuth2 PKCE implementation #auth #security",
    "bug-null-email: Null email panic in notifications #bug #users",
    "decision-sqlx: sqlx vs diesel choice #database #architecture",
    "topic-error-handling: Error handling patterns #patterns",
    "bug-race-condition: Concurrent user update race #bug #database"
  ]
}
```

**Format:** `{id}: {title} #{tag1} #{tag2}`

**Key insight:** Each entry is ~30 tokens. Top 50 entries ≈ 1.5K tokens. Entries ordered by `access_count` (most used first). AI knows what exists, retrieves details via `mdkb_memory_get`. Entries beyond top 50 are still searchable via `mdkb_memory_search`.

### Entry Types

1. **Topic** - Accumulated knowledge (`topic`)
   - Title: "Error handling patterns" (what, not how)
   - Content: Full explanation, examples, links

2. **Problem** - Bug/issue solved (`problem`)
   - Title: "Null email panic in notifications" (symptom, not solution)
   - Content: Full investigation, root cause, solution, prevention

3. **Decision** - Architectural choice (`decision`)
   - Title: "sqlx vs diesel choice" (options, not winner)
   - Content: Options considered, trade-offs, rationale, outcome

4. **Session** - Optional, for AI memory continuity (`session`)
   - Title: "Refactored auth module" (accomplishment)
   - Content: What happened, decisions made
   - **Enabled via:** `[memory] sessions_enabled = true`

### Schema

```sql
-- Memory entries with title index
CREATE TABLE memory_entries (
    id TEXT PRIMARY KEY,              -- slug: "auth-oauth2-flow"
    title TEXT NOT NULL,              -- Concise title (max 50 chars)
    content TEXT NOT NULL,            -- Full markdown content
    entry_type TEXT NOT NULL,         -- topic, problem, decision, session
    tags TEXT NOT NULL,               -- JSON array: ["auth", "security"]
    status TEXT DEFAULT 'active',     -- active, superseded, archived
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    superseded_by TEXT,               -- ID of newer entry
    access_count INTEGER DEFAULT 0,   -- Track usage for ranking
    last_accessed INTEGER
);

CREATE INDEX idx_memory_type ON memory_entries(entry_type);
CREATE INDEX idx_memory_status ON memory_entries(status);
CREATE INDEX idx_memory_access ON memory_entries(access_count DESC);

-- FTS for content search (beyond top 50)
CREATE VIRTUAL TABLE memory_fts USING fts5(
    id,
    title,
    content,
    tokenize = 'porter unicode61'
);
```

### CLI Commands

```bash
# Memory management
mdkb memory add <id> --title "..." --type topic [--tags tag1,tag2]
mdkb memory edit <id>                  # Open in $EDITOR
mdkb memory show <id>                  # Display full content
mdkb memory rm <id>

# Index operations
mdkb memory index                      # Rebuild index.json from DB
mdkb memory warmup                     # Output top 50 by usage (for AI consumption)

# Maintenance
mdkb memory condense                   # AI-assisted: merge related entries
mdkb memory prune [--unused-days N]    # Archive entries not accessed in N days
```

### MCP Tools

```rust
Tool {
    name: "mdkb_memory_index",
    title: "Memory Index",
    description: "Get top 50 memory entries by usage. Returns compact titles and \
        tags, ordered by access count. Use at session start for awareness of \
        available knowledge. Use mdkb_memory_get for full content, or \
        mdkb_memory_search to find entries beyond top 50.",
    parameters: {
        "filter_tags": { "type": "array", "items": { "type": "string" } },
        "filter_type": { "enum": ["topic", "problem", "decision", "session", "all"] }
    }
}

Tool {
    name: "mdkb_memory_get",
    title: "Get Memory Entry",
    description: "Retrieve full content of a memory entry by ID. Increments \
        access_count for ranking. Use after seeing relevant entry in index.",
    parameters: {
        "id": { "type": "string", "required": true }
    }
}

Tool {
    name: "mdkb_memory_write",
    title: "Write Memory Entry",
    description: "Create or update a memory entry. Title must be concise (max 50 \
        chars) like an article headline - informative but not spoiling content. \
        Use after solving problems, making decisions, or learning something \
        worth persisting.",
    parameters: {
        "id": { "type": "string", "required": true },
        "title": { "type": "string", "required": true, "maxLength": 50 },
        "content": { "type": "string", "required": true },
        "type": { "enum": ["topic", "problem", "decision", "session"], "required": true },
        "tags": { "type": "array", "items": { "type": "string" } },
        "supersedes": { "type": "string" }
    }
}

Tool {
    name: "mdkb_memory_search",
    title: "Search Memory Content",
    description: "Full-text search across ALL memory entries (not just top 50). \
        Use when index doesn't show what you need. Returns matching entries \
        with snippets.",
    parameters: {
        "query": { "type": "string", "required": true },
        "limit": { "type": "integer", "default": 10 }
    }
}
```

### Warmup Flow

On `mdkb serve` or session start:

```rust
async fn warmup() -> MemoryIndex {
    // 1. Load index.json (fast, no DB query needed for read-only)
    let index = read_json(".mdkb/memory/index.json")?;

    // 2. Filter to active entries only
    let active: Vec<_> = index.entries
        .into_iter()
        .filter(|e| e.status == "active")
        .collect();

    // 3. Return compact index (~50 tokens per entry)
    MemoryIndex {
        count: active.len(),
        entries: active,
    }
}
```

**AI receives at session start:**
```
Memory (23 entries):
auth-oauth2-flow: OAuth2 PKCE implementation #auth #security
bug-null-email: Null email panic in notifications #bug #users
decision-sqlx: sqlx vs diesel choice #database #architecture
topic-error-handling: Error handling patterns #patterns
...
```

Compact format: ~1.5K tokens for 50 entries. Ordered by usage (most accessed first). Entries beyond top 50 searchable via `mdkb_memory_search`.

### Configuration

```toml
[memory]
enabled = true
directory = "memory"

# Session tracking (opt-in for AI continuity use case)
sessions_enabled = false

# Index limits
warmup_limit = 50           # Top N entries by usage in warmup index
title_max_chars = 50        # Enforce concise titles
order_by = "access_count"   # Most accessed entries first (alternatives: updated_at, created_at)

# Auto-maintenance
prune_unused_days = 90      # Archive entries not accessed in 90 days
condense_threshold = 10     # Suggest condensing when >10 related entries

# Access tracking
track_access = true         # Log which entries are retrieved
```

### Condensation Strategy

Over time, multiple related entries accumulate. `mdkb memory condense` uses local LLM to:

1. Find entries with overlapping tags
2. Merge into single comprehensive entry
3. Mark originals as `superseded_by: <merged-id>`

Example:
```
Before:
- auth-jwt-basics (2025-06)
- auth-jwt-refresh (2025-08)
- auth-jwt-expiry-fix (2025-11)

After condensation:
- auth-jwt-complete (2026-01)
  Summary: "JWT auth: RS256, 15min access + 7day refresh, auto-rotation on expiry"
  Supersedes: auth-jwt-basics, auth-jwt-refresh, auth-jwt-expiry-fix
```

---

## NEW: Phase 7 - Knowledge Evolution (Supersedes Pattern)

### Research Finding: RFC Model

From RFC research, the key patterns are:
- **Documents are immutable** - once published, never modified
- **Explicit metadata**: `Obsoletes: RFC XXXX`, `Updates: RFC XXXX`
- **Bidirectional display**: Show both what supersedes and what is superseded
- **Status levels**: Documents can be marked "Historic" without deletion
- **Partial updates**: One doc can update parts of another

### Design: Evolution Tracking

**Relationship types:**

| Relationship | Meaning | Old Doc Status | Example |
|-------------|---------|----------------|---------|
| `supersedes` | Complete replacement | `superseded` | v2 replaces v1 |
| `updates` | Partial modification | `current` | Errata updates original |
| `corrects` | Error fix | `current` | Typo correction |
| `retracts` | Withdrawal | `retracted` | Wrong information removed |
| `extends` | Additive content | `current` | Follow-up article |

### Schema Additions

```sql
-- Evolution relationships between documents
CREATE TABLE evolution (
    id INTEGER PRIMARY KEY,
    source_doc_id INTEGER NOT NULL,      -- The newer/superseding document
    target_doc_id INTEGER NOT NULL,      -- The older/superseded document
    relationship TEXT NOT NULL,          -- supersedes, updates, corrects, retracts, extends
    scope TEXT,                          -- NULL = full doc, or section path like "## Auth"
    reason TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(source_doc_id) REFERENCES documents(id) ON DELETE CASCADE,
    FOREIGN KEY(target_doc_id) REFERENCES documents(id) ON DELETE CASCADE,
    UNIQUE(source_doc_id, target_doc_id, scope)
);

CREATE INDEX idx_evolution_source ON evolution(source_doc_id);
CREATE INDEX idx_evolution_target ON evolution(target_doc_id);

-- Add status to documents
ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
ALTER TABLE documents ADD COLUMN status_reason TEXT;
ALTER TABLE documents ADD COLUMN version TEXT;
```

### Frontmatter Convention

```yaml
---
title: "Authentication API v2"
version: "2.0"
supersedes:
  - path: "docs/auth-api-v1.md"
    reason: "Complete redesign with OAuth2"
updates:
  - path: "docs/security.md"
    scope: "## Token Handling"
---
```

### CLI Commands

```bash
# Declare evolution
mdkb evolve <new> supersedes <old> [--reason TEXT]
mdkb evolve <new> updates <old> [--scope SECTION]
mdkb evolve <new> corrects <old>
mdkb evolve <new> retracts <old> [--reason TEXT]

# Query evolution
mdkb history <path>              # Show evolution chain
mdkb current <path>              # Find current version
mdkb superseded-by <path>        # What superseded this?

# Search with evolution awareness
mdkb search <query> --include-superseded
mdkb search <query> --version-context=history

# Auto-detection
mdkb evolve analyze              # Detect potential chains
mdkb evolve apply [--interactive]
```

### MCP Tool

```rust
Tool {
    name: "mdkb_evolution",
    title: "Query Document Evolution",
    description: "Trace document evolution history - what supersedes it, what it \
        supersedes, version chains. Use when finding potentially outdated info or \
        understanding how knowledge evolved.",
    parameters: {
        "path": { "type": "string", "required": true },
        "direction": { "enum": ["ancestors", "descendants", "both"], "default": "both" }
    }
}
```

### Search Behavior

Default search excludes superseded documents. Results indicate supersession:

```
Query: "authentication"
Results:
  1. docs/auth-api-v2.md (score: 0.95) [CURRENT]
     └── supersedes: auth-api-v1.md (2025-06-15)
  2. docs/oauth-guide.md (score: 0.87)
```

With `--include-superseded`:
```
  1. [SUPERSEDED] docs/auth-api-v1.md (score: 0.92)
     Note: Superseded by docs/auth-api-v2.md
```

---

## NEW: Phase 8 - Self-Evaluation Metrics

### Research Finding: Key Metrics

From search quality research:
- **nDCG@10** is standard for retrieval comparison
- **Latency p50/p95/p99** critical for SLOs
- **Zero-result rate** is a passive failure signal
- **Re-search rate** (same query within 30s) indicates poor results

### Design: Metrics System

**Categories:**

1. **Passive metrics** (no user feedback needed):
   - Query latency (p50, p95, p99)
   - Index time per document
   - Zero-result rate
   - Score distribution
   - Cache hit rate

2. **Session-derived** (inferred from patterns):
   - Re-search rate (query modification = partial failure)
   - Search-to-get ratio (lower = more efficient)
   - Abandoned search rate

3. **Explicit feedback** (requires client cooperation):
   - Result position used
   - Time to first useful result

### Schema Additions

```sql
-- Query events for analysis
CREATE TABLE query_events (
    id INTEGER PRIMARY KEY,
    query_hash TEXT NOT NULL,       -- Hash of normalized query
    query_text TEXT NOT NULL,
    search_type TEXT NOT NULL,      -- bm25, semantic, hybrid
    result_count INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    top_score REAL,
    session_id TEXT,                -- For re-search detection
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_query_session ON query_events(session_id, created_at);
CREATE INDEX idx_query_created ON query_events(created_at);

-- Aggregated metrics (hourly rollups)
CREATE TABLE metrics_hourly (
    hour INTEGER NOT NULL,          -- Unix timestamp truncated to hour
    metric_name TEXT NOT NULL,
    metric_value REAL NOT NULL,
    sample_count INTEGER NOT NULL,
    PRIMARY KEY(hour, metric_name)
);

-- Daily summary
CREATE TABLE metrics_daily (
    date TEXT NOT NULL,             -- YYYY-MM-DD
    metric_name TEXT NOT NULL,
    p50 REAL,
    p95 REAL,
    p99 REAL,
    mean REAL,
    total_count INTEGER,
    PRIMARY KEY(date, metric_name)
);

-- Experiment tracking (A/B testing)
CREATE TABLE experiments (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT,
    start_date INTEGER NOT NULL,
    end_date INTEGER,
    config_a TEXT NOT NULL,         -- JSON config for variant A
    config_b TEXT NOT NULL,         -- JSON config for variant B
    status TEXT DEFAULT 'running'   -- running, completed, aborted
);

CREATE TABLE experiment_results (
    experiment_id INTEGER NOT NULL,
    variant TEXT NOT NULL,          -- 'a' or 'b'
    metric_name TEXT NOT NULL,
    metric_value REAL NOT NULL,
    sample_count INTEGER NOT NULL,
    FOREIGN KEY(experiment_id) REFERENCES experiments(id)
);
```

### CLI Commands

```bash
# View metrics
mdkb metrics show [--period DAYS]
mdkb metrics latency [--percentile P]
mdkb metrics quality

# Example output:
# === Query Metrics (last 7 days) ===
# Total queries: 1,234
# Zero-result rate: 3.2%
# Re-search rate: 8.5%
#
# Latency:
#   p50: 12ms
#   p95: 45ms
#   p99: 120ms
#
# Score distribution:
#   > 0.8: 45%
#   0.5-0.8: 38%
#   < 0.5: 17%

# Export for analysis
mdkb metrics export [--format csv|json] [--period DAYS]

# A/B testing
mdkb experiment create "chunking-strategy" \
    --config-a '{"strategy":"fixed"}' \
    --config-b '{"strategy":"markdown"}'
mdkb experiment status "chunking-strategy"
mdkb experiment end "chunking-strategy"
```

### MCP Tool

```rust
Tool {
    name: "mdkb_metrics",
    title: "Query Performance Metrics",
    description: "Get search performance metrics for self-evaluation. Use to \
        understand query patterns, identify issues, and track improvements.",
    parameters: {
        "period_days": { "type": "integer", "default": 7 },
        "include_latency": { "type": "boolean", "default": true },
        "include_quality": { "type": "boolean", "default": true }
    }
}
```

### Configuration

```toml
[metrics]
enabled = true
retention_days = 90             # Raw events
rollup_retention_days = 365     # Aggregated metrics

# Session detection
session_timeout_secs = 300      # 5 min gap = new session
re_search_window_secs = 30      # Same query within 30s = re-search

# Alerts (logged to stderr)
alert_zero_result_rate = 0.10   # >10% = warning
alert_p99_latency_ms = 500      # >500ms = warning

# A/B testing
ab_test_traffic_split = 0.5     # 50/50 split
ab_test_min_samples = 100       # Min samples before significance
```

### Optimization Workflow

1. **Baseline**: Run `mdkb metrics show` to understand current state
2. **Experiment**: Create experiment with different config
3. **Monitor**: Check `mdkb experiment status`
4. **Analyze**: Compare metrics between variants
5. **Apply**: Update config with winning variant

Example: Testing quantization levels
```bash
mdkb experiment create "quantization" \
    --config-a '{"embedding_model":"Q4_K_M"}' \
    --config-b '{"embedding_model":"Q5_K_M"}'

# After sufficient queries...
mdkb experiment status "quantization"
# Variant A (Q4_K_M): avg score 0.72, p95 latency 35ms
# Variant B (Q5_K_M): avg score 0.78, p95 latency 42ms
# Significance: 95% confidence B has better quality, A is faster
```

---

## Updated Implementation Phases

### Phase 1-5: (Unchanged from previous enhancement)

### Phase 6: Memory Index System
- [ ] Create `.mdkb/memory/` directory structure
- [ ] Implement `memory_entries` table with title/content split and access_count
- [ ] Generate and maintain `index.json` manifest
- [ ] Implement `mdkb memory` CLI commands (add, edit, show, warmup)
- [ ] Add MCP tools: `mdkb_memory_index`, `mdkb_memory_get`, `mdkb_memory_write`
- [ ] Implement access tracking for pruning
- [ ] Add `mdkb memory condense` with local LLM support (feature-gated)
- [ ] Optional: session tracking (`sessions_enabled = true`)

### Phase 7: Knowledge Evolution
- [ ] Add `evolution` table and `documents.status`
- [ ] Implement `mdkb evolve` commands
- [ ] Parse `supersedes:` from frontmatter
- [ ] Update search to respect evolution status
- [ ] Add `mdkb_evolution` MCP tool
- [ ] Implement `mdkb evolve analyze` for auto-detection

### Phase 8: Self-Evaluation Metrics
- [ ] Add metrics tables (`query_events`, `metrics_*`)
- [ ] Instrument query paths with timing
- [ ] Implement hourly/daily rollups
- [ ] Add `mdkb metrics` CLI commands
- [ ] Implement A/B experiment infrastructure
- [ ] Add `mdkb_metrics` MCP tool
- [ ] Add configurable alerts

---

## Questions Remaining

- [ ] Should we support PDF/Office documents via adapters (like ripgrep-all)?
- [ ] Is trigram fuzzy matching worth the added complexity?
- [ ] Should we expose MCP resources (URIs) in addition to tools?
- [x] Memory: warmup_limit = 50 entries (~1.5K tokens), ordered by access_count
- [ ] Evolution: Auto-mark `archive/` contents as superseded?
- [ ] Metrics: Store raw queries or just hashes for privacy?

---

## Documentation Links

- [SQLite FTS5 Documentation](https://sqlite.org/fts5.html)
- [MCP Specification (Nov 2025)](https://modelcontextprotocol.io/specification/2025-11-25)
- [Serena MCP Server](https://github.com/oraios/serena)
- [Weaviate Chunking Strategies](https://weaviate.io/blog/chunking-strategies-for-rag)
- [Late Chunking Paper](https://arxiv.org/abs/2409.04701)
- [Azure Hybrid Search Scoring](https://learn.microsoft.com/en-us/azure/search/hybrid-search-ranking)

---

## Next Steps

When ready to implement, run:
- `/wiz:work plans/mdkb-rust-implementation-enhanced.md` - Execute Phase 1
- Iterate through phases as needed

---

## Documentation Links

- [SQLite FTS5 Documentation](https://sqlite.org/fts5.html)
- [MCP Specification (Nov 2025)](https://modelcontextprotocol.io/specification/2025-11-25)
- [Serena MCP Server](https://github.com/oraios/serena)
- [Weaviate Chunking Strategies](https://weaviate.io/blog/chunking-strategies-for-rag)
- [Late Chunking Paper](https://arxiv.org/abs/2409.04701)
- [Azure Hybrid Search Scoring](https://learn.microsoft.com/en-us/azure/search/hybrid-search-ranking)
- [RFC 2026: Internet Standards Process](https://www.rfc-editor.org/rfc/rfc2026.html)
- [KGCL: Knowledge Graph Change Language](https://arxiv.org/abs/2409.13906)
- [BEIR Benchmark](https://github.com/beir-cellar/beir)
- [MTEB Benchmark](https://huggingface.co/spaces/mteb/leaderboard)

---

## Next Steps

When ready to implement, run:
- `/wiz:work plans/mdkb-rust-implementation-enhanced.md` - Execute Phase 1
- Iterate through phases as needed

---

*Enhanced with Claude Code via Happy*
*Research agents: indexing-strategies, serena-mcp-research, mcp-skill-integration, gap-analysis, journaling-systems, superseding-patterns, search-quality-metrics*
