---
id: retire-a-store-needs-repos-json
title: Retiring a store needs repos.json too
entry_type: problem
source_type: user_statement
status: active
tags: [daemon, repo-map, store-lifecycle, measured]
created_at: 1789974058
updated_at: 1789974058
---

Renaming or deleting a project .mdkb/ is not enough. The daemon RepoMap (~/.mdkb/repos.json) keeps every root it ever opened, and RepoMap only drops a root whose PATH is gone - the repo path survives, only the store went away. Measured 2026-09-21 on ~/Gits/CC_Playground/brainstorming: after mv .mdkb .mdkb.retired-20260921 and a daemon restart, the daemon recreated .mdkb/ holding one file, index.sqlite.writer.lock, and every command from a subfolder then failed with 'unable to open database file' instead of the intended anchor refusal. Correct procedure: mdkb daemon stop; remove the root from ~/.mdkb/repos.json; rm -rf the recreated .mdkb; mdkb daemon restart. There is no CLI to forget a repo - daemon has only status/stop/restart. RootHealth::NoStore exists for exactly this state, so recreating the directory looks like a defect worth fixing.
