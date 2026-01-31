---
id: 001-a7e1
title: Project setup - Cargo.toml, dependencies, basic structure
status: pending
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: []
---

## Description

Set up Rust project structure with all dependencies defined in Cargo.toml. Create module stubs and error handling infrastructure.

## Acceptance Criteria

- [ ] Cargo.toml populated with all Phase 1-2 dependencies
- [ ] Basic error types defined (error.rs with thiserror)
- [ ] Module structure created: main.rs, lib.rs, cli/, store/, domain/
- [ ] Basic tracing/logging setup
- [ ] Project compiles without warnings

## Implementation Notes

Dependencies for Phase 1-2:
- clap (CLI parsing)
- rusqlite with FTS5 (database)
- serde/serde_json/toml (serialization)
- tokio (async)
- rmcp (MCP protocol)
- thiserror/anyhow (error handling)
- tracing (logging)
- sha2, walkdir, glob, globset (utilities)
- pulldown-cmark, regex (markdown)

Feature flags:
- Default: no features
- llm: llama-cpp-rs + hf-hub (Phase 3+)

## Work Log

Starting Phase 1 implementation.
