---
id: mbx-views-stale-installed-binary
title: mbx target views leave target/release stale
entry_type: problem
source_type: auto_extracted
status: active
tags: [mbx, build, macos, codesign, mdkb]
created_at: 1789912122
updated_at: 1789912122
---

2026-09-20, story 111-bca6: ~/.local/bin/mdkb symlinks to <repo>/target/release/mdkb, but mbx has [target] views = true, so cargo build --release writes to /Users/stefano.straus/Gits/.mbx/targets/v1/<hash>/release/mdkb and never touches <repo>/target/release. The installed binary therefore stayed on the pre-v28 build and refused the migrated v28 store. Fix: copy the mbx artifact over target/release/mdkb, then codesign --force --sign - target/release/mdkb - a plain cp of a linker-signed adhoc binary is SIGKILLed (exit 137) until re-signed. Stop the daemon before the copy (ETXTBSY) and restart it after.
