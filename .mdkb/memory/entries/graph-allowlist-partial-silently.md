---
id: graph-allowlist-partial-silently
title: Default frontmatter_relations gives a partial graph
entry_type: problem
source_type: user_statement
status: active
tags: [graph, config, frontmatter, measured]
created_at: 1789974058
updated_at: 1789974058
---

graph.frontmatter_relations defaults to owner/stakeholders/themes/related. That is a starting vocabulary, not detection: a repo writing other relation keys gets a partial graph and nothing reports it, because edges never extracted cannot appear in graph dangling or graph hubs. Measured 2026-09-21 on a 62-node operational graph (brainstorming/work/graph): 56 edges under the defaults, 182 edges over the same files after adding the 12 keys the repo actually wrote (org, attendees, initiative(s), people, decisions, projects, presenters, co_presenters, organizers, prepares, escalated_to, coordination). Auto-detection is not the fix: extract_relation_refs accepts any string or list of strings, so type: person and org: [org:acme] are indistinguishable by value, and auto-edging would make 'person' a hub with degree = number of people. After editing the allowlist run mdkb update --force - a plain update skips files whose mtime has not moved and skips their edges with them.
