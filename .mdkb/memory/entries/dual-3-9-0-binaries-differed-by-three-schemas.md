---
id: dual-3-9-0-binaries-differed-by-three-schemas
title: "Two binaries both said 3.9.0, three schemas apart"
entry_type: problem
source_type: user_statement
status: active
tags: [release, schema, versioning, measured]
created_at: 1789990915
updated_at: 1789990915
---

Reported by the mdkb-fleet-audit agent, measured 2026-09-21 10:26. Two binaries both reporting 'mdkb 3.9.0' understood different schemas: ~/.cargo/bin/mdkb (a real file, built 09-17) wrote v25, while ~/.local/bin/mdkb via symlink to target/release wrote v28. Homebrew's 3.8.0 wrote v22. Proven against a COPY of the v28 LS/gate-os store: the v25 binary returned 'store schema is v28, but this mdkb binary understands v25. Refusing to open'. Sixteen stores were already unopenable by it; PATH order was the only thing preventing failures. Root cause was a leaked CARGO_TARGET_DIR leaving target/release stale, so the symlink everything points at was a Sep-20 build. Resolved by rebuilding with env -u CARGO_TARGET_DIR and uninstalling the homebrew copies. THE CAUSE IS NOT CLOSED: the version string still cannot distinguish two builds. Put the schema version or a build hash in mdkb --version, or this recurs the next time a stale target dir appears.
