---
id: decision-mdkb-3-7-5-final-validation
title: mdkb 3.7.5 final validation
entry_type: decision
source_type: auto_extracted
status: active
tags: [mdkb, validation, sqlite, clippy]
created_at: 1784731483
updated_at: 1784731535
---

Final validation on 2026-07-22: cargo fmt applied; `cargo clippy --all-targets --all-features -- -D warnings` clean without new allows; standard suite 1607 passed/32 ignored, and explicit ignored suite 32 passed; `cargo build --release --all-features` succeeded. Release `mdkb --format json stats --no-color` succeeded on eight live stores (mdkb, tuicommander, investimenti, agent2, itview, gh-metrics, aicheck, global home). PRAGMA quick_check returned ok for all 16 active index.sqlite/code.sqlite databases. No commit or push performed.
