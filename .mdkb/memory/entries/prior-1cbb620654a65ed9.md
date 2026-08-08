---
id: prior-1cbb620654a65ed9
title: "At stop, either finish and stop, or ask exactly one blocking question; never invent extra work or re-ask settled scope."
entry_type: prior
source_type: auto_extracted
status: active
tags: [auto-mined, stop]
created_at: 1785305499
updated_at: 1785305499
---

At stop, either finish and stop, or ask exactly one blocking question; never invent extra work or re-ask settled scope.

Failure: The agent paused or asked instead of classifying the stop state and acting on it, which violated the stop hook.
Fix: It should have either executed already-agreed scope, ended if finished, or asked one precise user-only question if blocked.
