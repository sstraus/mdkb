---
id: frontmatter-parse-failure-is-silent
title: A YAML frontmatter parse failure is stored as null
entry_type: problem
source_type: user_statement
status: active
tags: [frontmatter, silent-failure, yaml, graph, measured]
created_at: 1789982046
updated_at: 1789982046
---

Measured 2026-09-21 on brainstorming/work. Three documents carried an unquoted at-prefixed item inside a YAML flow sequence (aliases with @sstraus, @saikalur, @carlo-medas). At is a YAML reserved indicator at the start of a plain scalar, so the parser refused the ENTIRE frontmatter block. mdkb wrote the JSON string 'null' into documents.metadata and reported nothing - mdkb update printed '330 indexed' with 0 errors. Consequence: those documents lost id, type, org and projects; the v29 identity backfill found nothing to read; every edge pointing at them dangled. Those three names were exactly the three distinct unresolved targets behind all 25 remaining lines of graph dangling. Quoting the aliases and running mdkb update --force took dangling from 25 to 1. Falsifier run before writing this: yaml.safe_load('aliases: [Boss, x@y.it, @sstraus]') raises ScannerError and the quoted form parses, so the parse error exists and is being discarded, not absent. Detection query for any store: SELECT relative_path FROM documents WHERE metadata='null'. Filed as a P1 in the mdkb story queue.
