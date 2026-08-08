---
id: cli-smoke-test-workflow
title: CLI smoke test and test-before-commit rule
entry_type: topic
source_type: user_statement
status: active
tags: [testing, cli, smoke-test, workflow]
created_at: 1776612802
updated_at: 1776612802
---

**Pattern:** Run `cargo test` before every commit. `cargo test --test cli_smoke` exercises all 37 subcommands in isolated tempdirs. **Gotcha:** `get` uses collection-relative paths (e.g., `guide.md` not `docs/guide.md`). **Anti-pattern:** Never assume a test failure is pre-existing — builds are green on main.
