---
id: prior-settlement-no-automated-confirmation
title: A session never confirms a prior; only a person does
entry_type: decision
source_type: user_statement
status: active
tags: [priors, belief, settle-injections, mdkb]
created_at: 1789634079
updated_at: 1789634079
---

Story 092-777f, shipped 0b69bcc. settle_injections has three outcomes (refuted, unobservable, unrefuted) and only refuted moves a counter. confirmed_count is written only by apply_belief_from_memory, i.e. a human 'mdkb memory confirm'. WHY an automated positive rule is wrong, not merely weak: hook_pre_tool_use_impl (src/mcp/dispatch.rs) returns additionalContext and NEVER a permissionDecision deny, so the command is already committed when the prior is injected. A pre_tool injection therefore proves the warned-about operation ran WITH THE LESSON NOT APPLIED. The heeded case (agent rewrites the command, matcher stops firing) produces no injection at all, so no row to settle. An automated 'confirmed' is fed exclusively by the un-heeded case and rises fastest on priors whose failure is least deterministic. Second reason: tool_call_matches accepts a bare tool name, which prior_distill.rs offers the model as a pattern form, so pattern 'Bash' fires on every shell call. Rejected alternatives: discounted counter (same inverted signal at lower weight); re-evaluating the matcher against the next tool call (tool arguments are never persisted, so impossible at Stop; live it needs per-session state plus a DB touch on a hot path that deliberately opens none, and 'matcher stopped firing' is indistinguishable from 'agent moved on'). Consequence: a prior that keeps actually recurring still survives via mining, which moves recurrence and last_seen_at; one that is never re-mined, never refuted and never human-confirmed decays out - that is the intended TTL. Second opinion (Fable 5) agreed; /wiz:second-opinion was unusable because OpenRouter timed out at 300s even on a trivial prompt.
