---
id: all-features-gap-hides-compile-errors
title: Feature-gated constructors only break under --all-features
entry_type: problem
source_type: inference
status: active
tags: [ci, features, cfg, compile, lint]
created_at: 1789564953
updated_at: 1789564953
---

DispatchContext gained a 'background' field in 0fc2d14; the constructor behind #[cfg(feature = "http-server")] (src/mcp/dispatch.rs:529) was never updated. The default build does not enable that feature, so cargo build/test locally were green while CI's Lint (cargo clippy --all-features) and Test (cargo test --all-features) both failed with E0063, and Test Windows failed with the same error for a reason that looked Windows-specific but was not. Fixed in 0e2aa98. Rule: after adding a field to a struct that has any cfg-gated constructor, run cargo clippy --all-features before pushing — the default build cannot see the gap.
