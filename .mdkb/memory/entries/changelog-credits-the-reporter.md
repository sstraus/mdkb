---
id: changelog-credits-the-reporter
title: Changelog always credits the reporter
entry_type: decision
source_type: user_statement
status: active
tags: [changelog, conventions, agents-md, credits]
created_at: 1789730473
updated_at: 1789730473
---

Boss, 2026-09-18: every CHANGES.md entry resolving an outside report must name the reporter, and the rule belongs in AGENTS.md so it survives the machine. Format already in use: *(#6, reported by Steve Muchow (@smuchow1962))* — real name plus handle, and the verb says which of reported/diagnosed/fixed is true. CLAUDE.md is gitignored in this repo, so shareable conventions go in AGENTS.md (tracked, commit e1dd78c). AGENTS.md also carries two rules paid for the same day: an issue closes on a test that ran on the platform the issue is about, not on the fix landing; and a git pathspec goes through domain::rel_key. Gap that prompted it: 3.9.0 credits five reporters, the Unreleased section had eleven entries and zero credit lines.
