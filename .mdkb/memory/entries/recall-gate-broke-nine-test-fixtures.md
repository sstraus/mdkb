---
id: recall-gate-broke-nine-test-fixtures
title: "The recall gate broke 9 test fixtures, not the code"
entry_type: problem
source_type: auto_extracted
status: active
tags: [mdkb, testing, recall, fixtures]
created_at: 1789572932
updated_at: 1789572932
---

Story 083 put an absolute floor in search_entries_hybrid_fts. Nine tests failed and none was a bug: they seeded entries and queried them with ordinary shared words ("topic content", "oauth prior", "ranking signal", "fixture", keyword "cacherefreshpolicy"). No embedding service runs under test, so the distance arm is unavailable and only strong lexical can admit - and one shared common word is exactly what the gate rejects. Fix in every case was to give the fixture a genuine identifier (recall_gate_fixture, oauth_flow_details, cache_refresh_policy, parity_fixture_word) or a three-word consecutive phrase, never to loosen the gate. Files: src/mcp/dispatch.rs (6 hook prompts), tests/mcp/memory_search_payload_test.rs, tests/e2e_mcp.rs, tests/hooks/user_prompt_submit_test.rs, tests/surface_parity.rs. Rule for the next gate change: a fixture that matches on common words was never testing retrieval.
