---
id: mdkb-setup-stale-registrations
title: setup leaves stale legacy registrations
entry_type: problem
source_type: user_statement
status: active
tags: [setup, hooks, mcp, idempotency, legacy]
created_at: 1780841598
updated_at: 1780841598
---

setup hooks retain() only dropped _managedBy:mdkb entries — legacy untagged mdkb hook entries from pre-tag installs survived re-runs and caused double-fire. setup mcp claude treated 'already exists' as success so a stale 'mdkb serve' was never replaced by 'mdkb mcp'. Both fixed in 3.3.0: hook dedup now also matches by command substring, mcp setup removes before re-adding.
