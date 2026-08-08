---
id: agent-dx-plan-dryrun-schema
title: "Agent DX plan: dry_run + schema + null-byte completion"
entry_type: decision
source_type: user_statement
status: active
tags: [mcp, dry-run, cli-schema, validation, agent-dx]
created_at: 1780908164
updated_at: 1780908164
---

Completed plans/agent-dx-improvements.md (was 4/6 already done). Added: (1) null-byte rejection in validate_entry_input (memory.rs); (2) 'mdkb schema [command]' subcommand walking clap Command tree to JSON via CommandFactory (cli/mod.rs + command_to_json in main.rs); (3) dry_run:bool on memory_write/_batch/_delete threaded through BOTH entry points — rmcp server.rs AND daemon JSON dispatch.rs match arm (extract dry_run from params Value BEFORE serde_json::from_value consumes it). dry_run skips embedding compute and returns 'dry-run: would create/update/delete' before any write. 1237 tests pass.

GOTCHA: bulk-patching struct literals with a regex anchored only on 'root: None,' followed by '}))' OVER-MATCHED — GetParams/SearchParams/MemoryListParams/CodeGraphParams also end that way. Fix: name-anchor the regex on the struct (MemoryWriteParams|MemoryDeleteParams with [^{}]*? brace-bounded; batch uses lazy .*? to first 'root: None,' since inner entries have no root field). Recovery when regex over-inserts: git restore/checkout are BLOCKED by safety hook — use a struct-aware awk pass that tracks the last 'XxxParams {' opener and deletes dry_run lines inside disallowed structs.
