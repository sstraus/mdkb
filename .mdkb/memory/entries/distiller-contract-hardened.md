---
id: distiller-contract-hardened
title: "Distiller: fence tolerance, warn logs, setup check"
entry_type: decision
source_type: user_statement
status: active
tags: [priors, distiller, mining, cli]
created_at: 1789552355
updated_at: 1789552355
---

Prior mining was dead 2026-08-01..2026-09-16: codex gpt-5.4-mini returned HTTP 400, stdout was empty, parse_distilled rejected it as NotJson, and every failure path logged at debug while the daemon runs at info. Story 082 fixed the class, not the instance. parse_distilled now slices first { to last } so a fenced answer (claude -p) or a prose preamble (grok with MCP) parses; no-braces output is still NotJson. run_distiller_cli returns stdout+stderr+exit_code; stderr is never parsed but is quoted in the failure line, because a rejected model states its HTTP status only there. distiller_failure() is one predicate shared by the daemon and the new 'mdkb setup check', so the check cannot pass a distiller the daemon would reject; a validator verdict on a well-formed answer (NotReusable etc) stays debug, only misconfiguration warns. distiller_args supports a {prompt} placeholder: substituted into argv and stdin closed, for CLIs like grok that never read a pipe. Verified live 2026-09-16 with setup check: codex gpt-5.6-luna 8-9s, ollama gemma4:12b-mlx 18s, claude Haiku 4.5 19s, grok-4.5 33s - all four returned a valid prior; claude and grok could not work at all before this.
