---
id: 008-h9l8
title: File watching - notify integration, debounced indexing, auto-reindex on serve
status: pending
priority: P2
created: 2026-01-31
updated: 2026-01-31
dependencies: [007-g8k7]
---

## Description

Implement file system watcher using notify crate. Auto-reindex on file changes in MCP mode with debouncing.

## Acceptance Criteria

- [ ] FileWatcher struct using notify
- [ ] Debounced event batching (100ms window)
- [ ] Track created/modified/deleted files
- [ ] Respect exclusion patterns
- [ ] Skip known patterns (SKILL.md, .claude/, etc)
- [ ] Auto-reindex on changes
- [ ] Graceful error handling
- [ ] Tests for event batching

## Implementation Notes

File structure:
- watcher/mod.rs: FileWatcher struct
- watcher/events.rs: Event batching logic

Pattern matching:
- Use globset for efficient pattern matching
- Skip files matching default exclusions
- Exclude symlinks to prevent loops

Debouncing:
- Collect events for 100ms
- Batch into single reindex
- Run async to not block MCP server

Events:
- Create: New file, index it
- Modify: Check mtime, reindex if changed
- Delete: Remove from database
- Rename: Detect via content hash

## Work Log

[To be filled during implementation]
