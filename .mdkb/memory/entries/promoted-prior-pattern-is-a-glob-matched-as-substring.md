---
id: promoted-prior-pattern-is-a-glob-matched-as-substring
title: "Promoted prior pattern never matches: glob vs substring"
entry_type: problem
source_type: auto_extracted
status: active
tags: [priors, injection, matcher, distiller]
created_at: 1789564891
updated_at: 1789565071
---

Measured 2026-09-16. The only promoted cluster clu-083c10c9b924ef8b carries pattern '*| grep*' for pre_tool. store::priors::tool_call_matches treats a pattern as: tool-name equality, glob against file_path, or literal substring of the command. Bash has no file_path, so the asterisks are matched literally: the prior fires only when the command text literally contains the asterisks (it did once, when a memory-add command quoted the pattern), never on a real '| grep' pipeline. The distiller prompt does not say which semantics 'pattern' has per tool. Also: 6 candidate clusters carry the same BUDGET LIMIT lesson under different patterns and never merge, so evidence never accumulates while a 2-session prior about '| grep' promoted.
