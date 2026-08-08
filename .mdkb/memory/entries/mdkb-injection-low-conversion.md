---
id: mdkb-injection-low-conversion
title: "MCP injection dead, code-search loses 300:1"
entry_type: problem
source_type: user_statement
status: active
tags: [mcp, injection, hooks, adoption, code-search, pretooluse]
created_at: 1780830509
updated_at: 1780841598
---

RESOLVED in 3.3.0. Root cause confirmed via 490-session transcript scan: PreToolUse matched the Grep TOOL (never used) while Claude greps via Bash (5889 calls). Fix: hook now intercepts Bash grep/rg with quote-aware parser, zsh -lc unwrap (Codex), ~710 redirects per 490 sessions on corpus, 0 false positives. Conversion telemetry added (mdkb_invocation outcome + Conv column in stats). MCP kept as portable fallback but BASE_INSTRUCTIONS slimmed. Benchmark pending: restart CC session in tuicommander to activate new matcher.
