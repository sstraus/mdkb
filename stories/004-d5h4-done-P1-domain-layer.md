---
id: 004-d5h4
title: Domain layer - Collection, Document, Search abstractions (hexagonal)
status: done
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: [001-a7e1]
---

## Description

Implement domain layer with hexagonal architecture. Pure business logic independent of storage/CLI/MCP.

## Acceptance Criteria

- [x] Collection entity and operations
- [x] Document entity with metadata
- [x] SearchQuery and SearchResult abstractions
- [x] Indexing logic (frontmatter parsing via gray-matter)
- [x] Tag extraction from metadata (array or comma-separated)
- [x] Wiki-link parsing and normalization
- [x] Search interfaces (trait-based for mockability)
- [x] Tests with mock storage (32 tests)

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

2026-01-31: Completed with TDD
- frontmatter.rs: YAML parsing with gray-matter, title/tag extraction (13 tests)
- links.rs: Wiki-link extraction, normalization, code block filtering (16 tests)
- traits.rs: DocumentStore, CollectionStore, SearchEngine, TagStore, LinkStore traits (3 tests)
- All domain types storage-agnostic with mock implementations
