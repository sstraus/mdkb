---
id: symbol-search-cli-mcp-parity
title: "scope=symbols: fuzzy wins, filters in SQL"
entry_type: decision
source_type: user_statement
status: active
tags: [mdkb, search, cli, mcp, parity, sqlite]
created_at: 1786526603
updated_at: 1786526603
---

Decision (3.7.13): the CLI and the MCP server must answer scope=symbols identically. Fuzzy (FTS5 trigram substring) wins over exact name match, because exact lookup already has its own command on both surfaces (code find / code_find), and the CLI cheatsheet already documented the scope as fuzzy — the CLI code had drifted, not the doc.

Structural fix: both surfaces route through core::code::search_symbols_scoped, and scope=code routes through core::code::semantic_search_scoped. Divergence cannot come back by editing one side.

Five bugs closed in this family:
1. CLI search --scope symbols parsed --limit and ignored it; handle_code_find had no limit at all, so "tests" dumped 593 lines. code find had no --limit flag either.
2. CLI --scope symbols was exact-match while MCP was fuzzy — same query, different answers per surface.
3. CLI --scope code ran substring FTS while the help promised semantic search. It never called semantic_search.
4. code.semantic_search.threshold was dead config: nothing read it, MCP hardcoded a 0.5 serde default that shadowed it. threshold is now Option<f32>; None means "use config" (default 0.3).
5. Cheatsheet showed --file with glob syntax (*hook*) but the filter is a LIKE substring, so the documented example matched nothing.

Rule learned: kind/file filters MUST be applied in SQL, not by retaining rows in Rust after a capped fetch. Post-filtering a capped fetch returns fewer rows than exist (the cap already dropped the rows the filter would keep) and makes the true total unknowable. CodeDb::query_symbols(NameMatch, kind, file, limit) does the filtering in SQL and returns (rows, total_before_cap).

Rule learned: never truncate silently. CLI reports "Showing N of M" on stderr so JSON/CSV stdout stays parseable; MCP carries total in the payload.

Context: Codex drives mdkb through the CLI, not MCP, so the CLI was the surface actually burning context — the MCP path had always applied its limit.
