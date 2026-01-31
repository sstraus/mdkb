---
id: 006-f7j6
title: CLI layer - init, collection, search, get, status, update commands
status: pending
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: [003-c4g3, 005-e6i5]
---

## Description

Implement CLI command handler with clap. Core Phase 1 commands: init, collection, search, get, status, update.

## Acceptance Criteria

- [ ] CLI struct with clap subcommands
- [ ] init: Create .mdkb/ directory and config
- [ ] collection add/remove/list/rename
- [ ] search with --limit, --collection, --output-format
- [ ] get: Retrieve document by ID or path
- [ ] status: Show database and index statistics
- [ ] update: Trigger differential reindex
- [ ] Error messages are clear and helpful
- [ ] Output formatting (text, JSON)
- [ ] Tests for command parsing

## Implementation Notes

File structure:
- cli/mod.rs: CLI struct, main dispatch
- cli/init.rs: Initialize project
- cli/collection.rs: Collection commands
- cli/search.rs: Search command
- cli/get.rs: Document retrieval
- cli/status.rs: Status display
- cli/update.rs: Reindex

Output formats:
- Text: Human-readable table/list
- JSON: Structured output

Exit codes:
- 0: Success
- 1: General error
- 2: Not found
- 3: Invalid arguments

## Work Log

[To be filled during implementation]
