---
id: cerebro-org-graph-on-mdkb
title: "Cerebro: org knowledge-map as mdkb frontmatter edges"
entry_type: decision
source_type: user_statement
status: active
tags: [architecture]
created_at: 1783756767
updated_at: 1783756767
---

Org connection map (people/competencies/projects/repos/channels) built as markdown entity docs whose frontmatter keys are typed graph edges indexed by mdkb — NOT a graph DB (neo4j rejected: new runtime, breaks local-first). Reuses mdkb index/search/graph/recall for free. MCP sources harvested per-source with model tier in sources.toml: haiku for structured APIs (jira/gh-metrics/quill), sonnet for free-text+privacy-sensitive (slack/meetings). Lives at ../cerebro.
