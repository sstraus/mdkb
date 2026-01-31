---
id: 003-c4g3
title: Configuration system - TOML parsing, defaults, per-repo config
status: pending
priority: P1
created: 2026-01-31
updated: 2026-01-31
dependencies: [001-a7e1]
---

## Description

Implement TOML configuration system for .mdkb/config.toml with sensible defaults.

## Acceptance Criteria

- [ ] Config struct with serde serialization
- [ ] Load from .mdkb/config.toml with fallback to defaults
- [ ] Sections: [indexing], [chunking], [search], [memory], [models]
- [ ] Validation of config values on load
- [ ] Default config generation for new projects
- [ ] Environment variable override support
- [ ] Tests for parsing and defaults

## Implementation Notes

Config sections:
- indexing: default_pattern, debounce_ms, parse_frontmatter, parse_wikilinks
- chunking: strategy (fixed/markdown), max_tokens, overlap_tokens
- search: default_limit, min_score, rrf_k, weights
- memory: enabled, warmup_limit, order_by, track_access
- models: (Phase 3+) embedding/reranker repos and files

## Work Log

[To be filled during implementation]
