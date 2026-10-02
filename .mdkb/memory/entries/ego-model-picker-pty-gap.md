---
id: ego-model-picker-pty-gap
title: ego model picker final-screen PTY gap
entry_type: problem
source_type: user_statement
status: active
tags: [ego, tui, pty, tdd]
created_at: 1790416289
updated_at: 1790416289
---

Archived story 045 passed with a minimal Ollama catalogue and assertions against raw PTY bytes at 24 rows. With the full built-in catalogue at 8x40, Reedline scrolled Select model off the final screen; Question right status also overwrote the modal heading. Story 137 adds a vt100 final-screen assertion and windows choices around the active index. RED /model had no visible title; GREEN commit 2fb3b03.
