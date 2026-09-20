---
id: cross-repo-search-is-open-handles-only
title: "root=* searches only the open handles, not every repo"
entry_type: problem
source_type: auto_extracted
status: active
tags: [mcp, cross-repo, registry, daemon, false-negative]
created_at: 1789926397
updated_at: 1789926397
---

Measured 2026-09-20. cross_repo_search_impl (src/mcp/dispatch.rs:1583) fans out over registry.all_handles(), which returns only the repos currently in the in-memory DashMap (src/daemon/registry.rs:273), capped at max_active_repos = 5 (src/daemon/config.rs:16) with LRU eviction and lost on daemon restart. A repo that is known but not open is silently absent, so 'No results across repos' cannot be told from 'not searched'. The map itself is a side effect of who knocked: MCP roots/list handshake (server.rs:332), an explicit root= (server.rs:286), or a lifecycle hook (daemon/hook_runtime.rs:133). DaemonConfig.repos, commented 'Pre-registered repositories' (config.rs:76), is parsed and tested but read by no production code. Second defect in the same function: the query is embedded once per repo because embed_query_off_lock sits inside the per-repo future (dispatch.rs:1635). Boss's decision: persist the map in ~/.mdkb/repos.json, fan out on read-only connections outside the LRU, declare coverage. Stories 124-264e, 125-7419, 126-5dab.
