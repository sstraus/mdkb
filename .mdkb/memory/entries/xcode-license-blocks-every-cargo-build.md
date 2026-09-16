---
id: xcode-license-blocks-every-cargo-build
title: "Xcode 27 update blocks cc: every cargo build fails until the license is accepted"
entry_type: problem
source_type: user_statement
status: active
tags: [build, xcode, macos, toolchain, blocker]
created_at: 1789551200
updated_at: 1789551200
---

2026-09-16: every cargo build failed with 'linking with cc failed: exit status: 69' and 'You have not agreed to the Xcode license agreements'. Cause: Command Line Tools updated to 27.0.0 while /Library/Preferences/com.apple.dt.Xcode records agreement for 26.4.1 only; xcode-select points at /Applications/Xcode.app. Symptom looks like a Rust or mbx problem and is neither - the failing crates are build scripts of serde, libc, tree-sitter. Workaround that needs no sudo and changes no system state: prefix the build with DEVELOPER_DIR=/Library/Developer/CommandLineTools (the standalone CLT link fine). Permanent fix needs a human: sudo xcodebuild -license accept. Check first with: cc on a trivial .c file.
