---
id: prior-trigger-kinds-injector-mismatch
title: Prior trigger kinds stop/repo/post_tool can never be injected
entry_type: problem
source_type: user_statement
status: active
tags: [priors, injection, trigger]
created_at: 1789549480
updated_at: 1789549480
---

VALID_TRIGGER_KINDS in prior_distill.rs allows prompt, pre_tool, post_tool, stop, repo; trigger_matches in store/priors.rs handles only pre_tool and prompt (_ => false). 32 of 58 clusters incl. both promoted ones are unmatchable. Also confirmed_count is never incremented at runtime, so the injection score (belief 0.5, recurrence 0.67 at 2 sessions) starts at 0.33 vs threshold 0.3 and decays below in ~20 days. Stories 079 and 080.
