---
id: decision-distiller-codex-luna
title: "Distiller: codex gpt-5.6-luna by default, ollama/claude/grok documented"
entry_type: decision
source_type: user_statement
status: active
tags: [priors, distiller, codex, ollama, claude, grok]
created_at: 1789549480
updated_at: 1789549480
---

Measured 2026-09-16 with one distill prompt. codex luna 8s (7s with --ignore-user-config, which is the only working way to skip MCP startup; -c 'mcp_servers={}' is a silent no-op, openai/codex#16045), gpt-reserve 7s, sol 12s: same lesson. ollama gemma4:12b-mlx 2-5s warm, correct 3/3 with --think=false --hidethinking --nowordwrap --format json; gemma4:e4b-mlx unusable (invents trigger kinds). claude -p Haiku 9s via subscription but fences the JSON and --bare drops the login. grok-4.5 17s with --deny 'mcp__*', prompt only as argument. Boss chose codex luna as default; all four documented in daemon.toml.
