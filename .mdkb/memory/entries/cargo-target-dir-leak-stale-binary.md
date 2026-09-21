---
id: cargo-target-dir-leak-stale-binary
title: CARGO_TARGET_DIR leak makes the installed mdkb stale
entry_type: problem
source_type: user_statement
status: active
tags: [build, mbx, toolchain, measured]
created_at: 1789981337
updated_at: 1789981337
---

Measured 2026-09-21. CARGO_TARGET_DIR was set in the Claude Code session environment to /Users/stefano.straus/Gits/personal/tuicommander/src-tauri/target - not in .zshrc, inherited from the parent process (TUIC or a wiz hook). Every cargo build of mdkb therefore wrote its artifacts into tuicommander's target dir: release/mdkb there had mtime 10:52:59 and reported 3.9.0, while ~/Gits/personal/mdkb/target/release/mdkb was still the Sep 20 21:25 build. ~/.local/bin/mdkb and ~/.cargo/bin/mdkb are symlinks to that stale path, so every mdkb on PATH was a v28 binary while the project store had already been migrated to v29 (544 rows in document_aliases, store mtime 10:37:28) by a debug binary out of the leaked target dir. Symptom: every CLI call failed with 'store schema is v29, but this mdkb binary understands v28'. Fix: build with 'env -u CARGO_TARGET_DIR cargo build --release' so the artifact lands where the symlink looks, then mdkb daemon restart. Do not export the unset in a profile. The global CLAUDE.md already forbids setting CARGO_TARGET_DIR; the new fact is that something sets it for you.
