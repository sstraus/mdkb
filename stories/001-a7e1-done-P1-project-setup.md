---
id: 001-a7e1
title: Project setup - Cargo.toml, dependencies, basic structure
status: done
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: []
---

## Description

Set up Rust project structure with all dependencies defined in Cargo.toml. Create module stubs and error handling infrastructure.

## Acceptance Criteria

- [x] Cargo.toml populated with all Phase 1-2 dependencies
- [x] Basic error types defined (error.rs with thiserror)
- [x] Module structure created: main.rs, lib.rs, cli/, store/, domain/
- [x] Basic tracing/logging setup
- [x] Project compiles without warnings

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

2026-01-31: Completed project setup
- Cargo.toml with 25+ dependencies (Phase 1-2)
- Error types with thiserror (15 variants)
- Module structure: lib.rs, cli/, store/, domain/, config.rs
- CLI with clap: init, collection, search, get, status, update, serve
- Tracing setup with verbosity levels
- 5 passing unit tests
