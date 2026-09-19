---
id: windows-test-host-needs-domain-login
title: Windows test host SSH needs the domain-qualified user
entry_type: problem
source_type: user_statement
status: active
tags: [windows, ssh, test-host, lab]
created_at: 1789809392
updated_at: 1789809392
---

The Windows test host at 10.37.2.56 rejects the bare username from itview/.env with Permission denied (publickey,password,keyboard-interactive), for password AND keyboard-interactive, with the correct password. It accepts the SAME password when the principal carries the lab domain: lab\\<user> or <user>@lab.local. Measured 2026-09-19 after three failed bare-name attempts were briefly mistaken for a rotated or locked credential; Boss pointed out other processes were running tests on the box at that moment, which ruled the host out. Sources live at C:\\mdkbtest (a plain copy, NOT a git clone — ship changes with tar over scp). Build there with CARGO_INCREMENTAL=0 and CARGO_PROFILE_DEV_DEBUG=0: a full-debug build fills the disk and linking dies with LNK1180.
