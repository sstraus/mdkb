---
id: prior-mining-dead-distiller-400
title: "Prior mining silently dead: distiller model rejected with HTTP 400"
entry_type: problem
source_type: user_statement
status: active
tags: [priors, distiller, codex, mining, silent-failure]
created_at: 1789549480
updated_at: 1789549480
---

From 2026-08-01 to 2026-09-16 no prior candidate was mined. Root cause: ~/.mdkb/daemon.toml ran codex exec -m gpt-5.4-mini, which a ChatGPT account rejects with HTTP 400. stdout empty, parse_distilled -> NotJson, logged only at debug while the daemon runs at info, and the Stop hook outcome is always 'skipped' because distillation is detached. How found: run the exact distiller command by hand with a fixed prompt and read stderr. Prevention: story 082 adds a doctor check, warn-level logs and real Stop outcomes. Fixed by switching to gpt-5.6-luna with --ignore-user-config; first candidate mined 45s after restart.
