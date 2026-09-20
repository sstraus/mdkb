---
id: hybrid-admission-gate-drops-bare-tokens
title: A bare token fails hybrid search when no ONNX model is present
entry_type: problem
source_type: auto_extracted
status: active
tags: [testing, hybrid, search, fixtures, onnx]
created_at: 1789929939
updated_at: 1789929939
---

Measured 2026-09-20 while writing tests/cross_repo_search.rs. A test fixture entry found by the bare token 'zonkharvest' returned nothing even from a healthy, open repo - and it was the deliberate CONTROL test failing, which is the only reason it was caught before three genuine red tests were read as proof of a defect. Cause: with no ONNX model in the test environment the vector leg of hybrid search is absent, so every candidate must pass the lexical arm of store::hybrid::admits. strong_lexical_match (src/store/hybrid.rs:164) admits on a verbatim identifier, a three-word phrase, or two distinct rare terms. A lone bare token is none of the three, so the admission gate drops it even though the FTS index is fine - MATCH 'zonkharvest' returned the row directly. Fix for fixtures: use an identifier-shaped term such as 'zonk_harvest' so arm 1 fires, or a three-word phrase, or two rare terms. General lesson: a search test that runs without the model is testing the lexical arm only, and a fixture term that a human would consider obviously unique can still be inadmissible. Always keep a positive control in a search test suite - without it a broken fixture is indistinguishable from the defect under test.
