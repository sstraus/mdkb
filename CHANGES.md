# Changelog

## 1.2.0 (2026-04-08)

### Fixed

- **Code index: duplicate symbols crash** — `UNIQUE constraint failed` on JS/TS files with same-line redeclarations (e.g., minified code, `var` re-declarations). Changed `INSERT` to `INSERT OR REPLACE` in symbol storage.
- **Startup reindex silent failure** — the above crash was logged but silently ignored, leaving the code index stale after server restart.

### Added

- **Shebang language detection** — extensionless scripts with shebangs (`#!/usr/bin/env node`, `#!/usr/bin/python3`, etc.) are now detected and indexed as their respective languages.
- **Semantic code search enabled by default** — `code.semantic_search.enabled` defaults to `true`. Embedding-based code search (`scope="code"`) works out of the box.

### Changed

- **MCP instructions rewritten** — removed "always use mdkb search before Grep" rule. New instructions clarify when to use mdkb (semantic queries, code_graph, memory) vs Grep (exact pattern matching). `code_graph` promoted to a primary workflow step.
