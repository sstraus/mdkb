---
id: api-change-requires-dashboard-plugin-update
title: API changes require both TUICommander consumers updated
entry_type: decision
source_type: user_statement
status: active
tags: [api, tuicommander, plugin, daemon, process]
created_at: 1789911983
updated_at: 1789913781
---

Boss (2026-09-20): every mdkb API change must also update, commit and push ~/Gits/personal/tuicommander. TWO consumers, both break silently: (1) the app itself - src-tauri/src/mdkb_client.rs, mdkb_daemon.rs, mdkb_commands.rs - speaks JSON-RPC to the mdkb daemon socket (ping, outline, goto_definition, references, code_find) and depends on response shapes plus the 0-based/1-based symbol line convention; (2) plugins/mdkb-dashboard shells out to 'mdkb --format json' (stats, memory list, update, code index --force, embed, compact). Bump manifest.json version when the plugin changes. Recorded in mdkb/CLAUDE.md.
