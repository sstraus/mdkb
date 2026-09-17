---
id: prior-trigger-matcher-typed-d4
title: "Prior triggers: typed selectors replace one guessed string"
entry_type: decision
source_type: auto_extracted
status: active
tags: [priors, trigger-matcher, distiller, measurement, d4]
created_at: 1789655305
updated_at: 1789655305
---

Story 090-45ae / plan Step 8 (D4), 2026-09-17. tool_call_matches took one untyped 'pattern' and tried it against the tool name, then a path glob, then a command substring, stopping at the first hit.

Measured by replaying 87 stored candidates (56 of them tool-kind) against 5124 unique tool calls extracted from 19 session transcripts: 308 total matches, of which 285 came from the bare tool-name arm, and 50 of 56 patterns matched nothing at all. The one cluster that matched for a real reason, '*| grep*', hit 7 of 4802 commands because it was written as a glob and matched as a literal substring. Re-expressed as {"command_contains":"| grep"} the same lesson matches 1201. {"tool":"Edit","path_glob":"**/*.rs"} matches 123 of 141 Edit calls, a narrowing the old {"pattern":"Edit"} (141, every edit) could not express.

Decision: four named selectors (tool case-insensitive, path_glob case-sensitive glob, command_contains case-sensitive literal, prompt_contains case-insensitive literal). At least one required, all present ones ANDed. A selector the context cannot supply FAILS rather than falling away — otherwise the matcher widens as the context thins, which is how a narrow-looking pattern fired everywhere. No regex: untrusted model output, and its failure mode is a stall not a miss.

Old-shape rows are ARCHIVED by schema migration v26, never reinterpreted: there is no safe reading to migrate to. Evidence stays on the row for hand re-expression. Replay harness kept at tests/prior_matcher_replay.rs, #[ignore]d, reading its corpora from env vars — the event corpus is not committed because transcript paths carry the local username.
