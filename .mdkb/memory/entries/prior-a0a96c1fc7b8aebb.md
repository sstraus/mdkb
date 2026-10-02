---
id: prior-a0a96c1fc7b8aebb
title: "Avoid blocked git reset modes; use a permitted, non-destructive recovery workflow instead."
entry_type: prior
source_type: auto_extracted
status: active
tags: [auto-mined, post_tool]
created_at: 1790927953
updated_at: 1790927953
---

Avoid blocked git reset modes; use a permitted, non-destructive recovery workflow instead.

Failure: A Bash command attempted git reset --soft/--mixed/--keep and was blocked by the git_history safety hook.
Fix: Continue with corrective shell commands and edits without retrying the blocked history rewrite.
