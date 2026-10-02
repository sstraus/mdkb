---
id: prior-1ca7e813fa00bd94
title: "As a managed peer, send exactly one final RESULT or BLOCKED to the parent and write the same text to the required results file."
entry_type: prior
source_type: auto_extracted
status: active
tags: [auto-mined, prompt]
created_at: 1790781322
updated_at: 1790781322
---

As a managed peer, send exactly one final RESULT or BLOCKED to the parent and write the same text to the required results file.

Failure: The session required explicit peer completion reporting and durable result-file delivery.
Fix: Use the agent send primitive and create the mandated results file before ending.
