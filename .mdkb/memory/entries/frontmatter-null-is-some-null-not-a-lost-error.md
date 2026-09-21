---
id: frontmatter-null-is-some-null-not-a-lost-error
title: "The engine returns Some(Null), it does not lose an error"
entry_type: problem
source_type: user_statement
status: active
tags: [frontmatter, root-cause, measured, correction]
created_at: 1789993579
updated_at: 1789993579
---

Refines frontmatter-null-root-cause-and-writer, which credited the reported cause: the discarded .ok() at src/domain/frontmatter.rs:73. Measured 2026-09-21 by printing what the parser actually returns for four inputs. The gray_matter YAML engine does NOT return an error for a block it cannot read - it hands back a Pod that deserializes CLEANLY to Value::Null, so .ok() never sees an Err at all. Results: 'aliases: [@sstraus]' gives Some(Null); a document with no block gives None; a good block gives Some(Object). Some(Null) is what was serialized into documents.metadata as the JSON string 'null', and that is why every later reader skipped those rows silently - the v29 identity backfill filters on json_type(metadata,'$.id') and for 'null' that is NULL. The usable signal is therefore not an error to stop discarding but a NON-EMPTY raw block (result.matter) that produced anything other than a JSON object. Fixed by adding ParsedDocument::frontmatter_error, which separates the two Nones, and pushing it into UpdateResult::errors. Measured on the mdkb repo before and after: documents with metadata='null' 13 to 0, errors printed by mdkb update 0 to 13. Lesson: 'the error is being swallowed by .ok()' was a plausible reading of the code and it was wrong; only running it showed there was never an Err.
