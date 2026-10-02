---
id: prior-hook-matchers-starve-tool-priors
title: Tool priors never fire for tools the hook matchers exclude
entry_type: problem
source_type: auto_extracted
status: active
tags: []
created_at: 1790153835
updated_at: 1790153835
---

Measured 2026-09-23 in tuicommander: 9 of 17 never-injected promoted clusters target tools the installed hooks never see. src/cli/setup.rs:509-511 registers PreToolUse only for Grep|Bash and PostToolUse only for Edit|Write|NotebookEdit|MultiEdit, so pre_tool Edit/AskUserQuestion/mcp__* and post_tool Bash/mcp__* priors are dead. The other 8 are prompt-kind priors blocked by the sigil gate: recall_mode returns Off before prompt_prior_block runs (dispatch.rs ~4629). A trigger on the native Agent tool would also never fire.
