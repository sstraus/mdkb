---
id: 002-b3f2
title: Database schema - collections, documents, FTS5, content storage
status: pending
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: [001-a7e1]
---

## Description

Implement SQLite schema with FTS5 full-text search, content-addressable storage, and document tracking.

## Acceptance Criteria

- [ ] Schema version table for migrations
- [ ] Collections table (name, path, pattern)
- [ ] Content table (SHA256 hash, body, created_at)
- [ ] Documents table (id, path, hash, title, metadata, file_modified_at, indexed_at)
- [ ] FTS5 virtual table with porter stemmer + column weighting
- [ ] Indexes on common queries
- [ ] Triggers to keep FTS in sync with documents
- [ ] Database opens/creates in .mdkb/index.sqlite
- [ ] Connection pooling with WAL mode
- [ ] Tests for schema creation

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

[To be filled during implementation]
