---
id: 005-e6i5
title: Storage layer - Store struct, CRUD operations, FTS queries
status: pending
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: [002-b3f2, 004-d5h4]
---

## Description

Implement storage layer with SQLite backend. CRUD operations for collections, documents, FTS search.

## Acceptance Criteria

- [ ] Store struct with rusqlite connection
- [ ] Collection CRUD (add, remove, list, rename)
- [ ] Document CRUD (index, update, delete)
- [ ] Content storage and deduplication
- [ ] BM25 full-text search implementation
- [ ] FTS query parsing and execution
- [ ] Tag operations (add, list, query)
- [ ] Link operations (add, query backlinks)
- [ ] Differential indexing (check file mtime)
- [ ] Tests for all operations

## Implementation Notes

File structure:
- store/mod.rs: Store struct, connection management
- store/collections.rs: Collection operations
- store/documents.rs: Document operations
- store/search.rs: FTS search implementation
- store/tags.rs: Tag operations
- store/links.rs: Wiki-link operations

Key methods:
- Store::open(path) -> Result<Store>
- Store::index_file(path, content) -> Result<DocId>
- Store::search(query) -> Result<Vec<SearchResult>>
- Store::get(docid) -> Result<Document>
- Store::status() -> DatabaseStatus

## Work Log

[To be filled during implementation]
