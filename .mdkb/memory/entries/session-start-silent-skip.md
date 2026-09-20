---
id: session-start-silent-skip
title: "SessionStart returns {} for four different reasons"
entry_type: problem
source_type: auto_extracted
status: active
tags: [hooks, session-start, false-negative, daemon]
created_at: 1789926407
updated_at: 1789926407
---

Measured 2026-09-20 on mdkb itself: 'mdkb hook session-start' with a valid event and cwd produced 0 bytes stdout, 0 stderr, exit 0, and -vv showed no debug line - while session_start_enabled defaults true (src/config.rs:740), .mdkb exists and every other command opens the store. hook_session_start_inner (src/mcp/dispatch.rs:3763) has four early returns that all yield json!({}): hooks disabled, no .mdkb dir, ensure_handle_context failed, and the tail. The daemon maps any {} to outcome=skipped (dispatch.rs:5567) and emit_hook_response (src/cli/hook_client.rs:426) prints nothing for an empty object. So 'hooks switched off on purpose' and 'the store would not open' reach the caller identically - which is exactly what the v27/v28 binary mismatch did that morning, 21 failed runs reporting 'open store failed' with no context emitted. Story 127-fcaf.
