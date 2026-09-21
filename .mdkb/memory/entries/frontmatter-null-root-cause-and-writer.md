---
id: frontmatter-null-root-cause-and-writer
title: "Frontmatter parse errors: one .ok() and one bad writer"
entry_type: problem
source_type: user_statement
status: active
tags: [frontmatter, silent-failure, yaml, root-cause, measured]
created_at: 1789990145
updated_at: 1789990145
---

Extends and corrects frontmatter-parse-failure-is-silent. TWO separate causes, both measured 2026-09-21. (1) mdkb side, the reason nothing is reported: src/domain/frontmatter.rs line 73 reads - let frontmatter: Option<Value> = result.data.and_then(|d| d.deserialize().ok()); - the .ok() throws the deserialization error away, so unparseable YAML becomes None, is stored as the JSON string null, and produces no warning, no count and no log. One line. A map_err with a tracing::warn makes the entire class visible at index time. (2) Corpus side, the reason documents are broken at all: the at-sign-is-a-YAML-reserved-indicator diagnosis was right for brainstorming/work but does NOT generalise. Classified with PyYAML across the fleet, 34 live broken documents have four causes and none is the at-sign: 22 double-encoded dependencies values, 6 unescaped inner quotes in title, 3 unquoted titles starting with a bracket, 3 with trailing text after a quoted scalar. 31 of the 34 come from ONE upstream writer - every file is under a repo stories/archive directory, so the wiz stories CLI emits values it does not YAML-quote. Fixing that writer stops the class reproducing in every wiz repo. INTERACTION WITH v29: the document_aliases backfill filters json_valid(metadata) AND json_type(metadata,dollar.id)=text; for metadata=null json_valid is 1 but json_type is NULL, so all 37 are skipped silently. Correct behaviour on broken data, but the identity feature is blind exactly where the data already failed - release-note item.
