---
id: prior-d7ef4e2bbd4ec912
title: "Quote shell glob arguments such as --include='*.rs' to prevent zsh from expanding unmatched patterns before the command runs."
entry_type: prior
source_type: auto_extracted
status: active
tags: [auto-mined, pre_tool]
created_at: 1789926100
updated_at: 1789926100
---

Quote shell glob arguments such as --include='*.rs' to prevent zsh from expanding unmatched patterns before the command runs.

Failure: An unquoted --include=*.rs glob caused zsh to abort with no matches found.
Fix: Quote the glob argument so the downstream command receives the pattern literally.
