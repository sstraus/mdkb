---
id: prior-083c10c9b924ef8b
title: Use rg or command-native filtering instead of piping shell output to grep.
entry_type: prior
source_type: auto_extracted
status: active
tags: [auto-mined, pre_tool]
created_at: 1789548091
updated_at: 1789548091
---

Use rg or command-native filtering instead of piping shell output to grep.

Failure: A shell command failed because the parser rejected a downstream grep pipeline stage.
Fix: Replaced grep pipelines with supported search or filtering commands.
