---
id: mdkb-cli-memory-upsert-bug
title: CLI memory-write crashed on re-write (UNIQUE)
entry_type: problem
source_type: user_statement
status: active
tags: [memory, upsert, cli, bridge, sqlite]
created_at: 1780841598
updated_at: 1780841598
---

handle_memory_add called add_entry (INSERT-only) while MCP memory_write did upsert. Re-writing an existing id via CLI/bridge crashed with UNIQUE constraint failed. Fixed in 3.3.0: CLI now checks existence, updates+saves revision if found, inserts otherwise. Matches MCP path.
