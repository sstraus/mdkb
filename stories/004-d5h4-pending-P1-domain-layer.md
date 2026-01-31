---
id: 004-d5h4
title: Domain layer - Collection, Document, Search abstractions (hexagonal)
status: pending
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: [001-a7e1]
---

## Description

Implement domain layer with hexagonal architecture. Pure business logic independent of storage/CLI/MCP.

## Acceptance Criteria

- [ ] Collection entity and operations
- [ ] Document entity with metadata
- [ ] SearchQuery and SearchResult abstractions
- [ ] Indexing logic (frontmatter parsing, chunking)
- [ ] Tag extraction from metadata
- [ ] Wiki-link parsing and normalization
- [ ] Search interfaces (trait-based for mockability)
- [ ] Tests with mock storage

## Implementation Notes

Domain modules:
- domain/collection.rs: Collection struct, operations
- domain/document.rs: Document, Metadata, Tag
- domain/search.rs: SearchQuery, SearchResult, SearchConfig
- domain/indexing.rs: Index operations, frontmatter parsing
- domain/links.rs: Wiki-link extraction
- domain/tags.rs: Tag management

Traits (storage layer implements):
- DocumentStore
- IndexManager

## Work Log

[To be filled during implementation]
