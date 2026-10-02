---
id: prior-a9d94a5711ac60ac
title: "Use the exact case-sensitive shell tool name exposed by the environment, such as Bash instead of bash."
entry_type: prior
source_type: auto_extracted
status: active
tags: [auto-mined, post_tool]
created_at: 1790854831
updated_at: 1790854831
---

Use the exact case-sensitive shell tool name exposed by the environment, such as Bash instead of bash.

Failure: Calling lowercase bash failed because tool names are case-sensitive.
Fix: Retried with the available Bash tool and continued successfully.
