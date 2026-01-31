---
id: 002-b3f2
title: Database schema - collections, documents, FTS5, content storage
status: done
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: [001-a7e1]
---

## Description

Implement SQLite schema with FTS5 full-text search, content-addressable storage, and document tracking.

## Acceptance Criteria

- [x] Schema version table for migrations
- [x] Collections table (name, path, pattern)
- [x] Content table (SHA256 hash, body, created_at)
- [x] Documents table (id, path, hash, title, metadata, file_modified_at, indexed_at)
- [x] FTS5 virtual table with porter stemmer + column weighting
- [x] Indexes on common queries
- [x] Triggers to keep FTS in sync with documents
- [x] Database opens/creates in .mdkb/index.sqlite (WAL mode in Store)
- [x] Connection pooling with WAL mode (via Store pragmas)
- [x] Tests for schema creation (19 tests)

## Implementation Notes

Key design:
- Content deduplication via SHA256 hash
- FTS5 with porter stemmer + unicode61
- Column weighting: title 10x, body 1x
- Metadata as JSON for frontmatter
- Triggers keep FTS updated automatically

PRAGMAs:
```
PRAGMA journal_mode = WAL;
PRAGMA mmap_size = 1GB;
PRAGMA temp_store = memory;
```

## Work Log

2026-01-31: Completed with TDD
- RED: Wrote 19 failing tests first (schema, tables, FTS5, triggers)
- GREEN: Implemented init_schema() and get_schema_version()
- Schema includes: schema_version, collections, content, documents tables
- FTS5 with porter stemmer and BM25 column weighting (title 10x, body 1x)
- Triggers: documents_ai (insert), documents_ad (delete), documents_au (update)
- Indexes: idx_documents_collection, idx_documents_hash, idx_documents_path
