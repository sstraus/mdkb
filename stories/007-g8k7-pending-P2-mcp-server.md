---
id: 007-g8k7
title: MCP server - rmcp integration, tools, auto-init warmup pattern
status: pending
priority: P2
created: 2026-01-31
updated: 2026-01-31
dependencies: [001-a7e1, 005-e6i5]
---

## Description

Implement MCP server using rmcp SDK. Expose mdkb_search and other core tools, implement Serena-pattern auto-init on serve.

## Acceptance Criteria

- [ ] MCP server struct with rmcp
- [ ] mdkb_search tool (BM25 full-text search)
- [ ] mdkb_get tool (document retrieval)
- [ ] mdkb_status tool
- [ ] mdkb_update tool (trigger reindex)
- [ ] mdkb_collection tool (list)
- [ ] Auto-init on serve (find .git or .mdkb)
- [ ] Warmup: verify index freshness
- [ ] Tool descriptions with usage guidance
- [ ] Error handling (all logs to stderr)
- [ ] Tests for tool behavior

## Implementation Notes

File structure:
- mcp/mod.rs: MCP server struct
- mcp/tools.rs: Tool implementations
- mcp/init.rs: Auto-init and warmup logic

Serena pattern:
1. Walk up from CWD looking for .mdkb/config.toml or .git
2. Auto-init if needed
3. Verify index freshness
4. Start MCP server

Tool naming:
- mdkb_search: Keyword/BM25 search
- mdkb_get: Document retrieval
- mdkb_status: Index health
- mdkb_update: Reindex trigger
- mdkb_collection_list: Collection management

CRITICAL: All logs must go to stderr, never stdout (JSON-RPC protocol).

## Work Log

[To be filled during implementation]
