---
id: setup-honours-claude-config-dir
title: setup writes hooks to CLAUDE_CONFIG_DIR
entry_type: problem
source_type: user_statement
status: active
tags: [setup, hooks, claude-config-dir]
created_at: 1789562229
updated_at: 1789562229
---

claude_settings_path resolved user scope to $HOME/.claude only, so sessions under CLAUDE_CONFIG_DIR=~/.claude-private read a settings file setup never wrote: 4 of 5 events registered, no Stop entry, no prior mining at all. Fix (story 081-ea44): claude_profile_dir reads CLAUDE_CONFIG_DIR first, --profile-dir still outranks it; mdkb setup check names both settings files in effect and reports missing or duplicated HOOK_EVENTS, reporting both halves before exiting. Test isolation must clear CLAUDE_CONFIG_DIR alongside HOME or the suite rewrites the developer's real settings.
