---
id: cli-machine-output-must-be-machine-readable
title: Machine output formats must carry no prose
entry_type: problem
source_type: user_statement
status: active
tags: [cli, json, csv, output, parser]
created_at: 1789551175
updated_at: 1789551175
---

mdkb search on the default scope printed '## Documents' and '## Memory Entries' markdown headings before the payload in EVERY format, json and csv included. A caller doing 'mdkb search q --format json | jq' failed on line 1, so consumers fell back to parsing prose or to ad-hoc Python. Fixed 2026-09-16: the combined scope emits one JSON object with 'documents' and 'memory' keys; csv drops the headings. Class of defect to check for elsewhere: a formatter that is chosen per-record while the SECTION structure around it is printed unconditionally. Related: get accepted fewer path forms than graph (collection/path, collection:path, no .md) although search prints collection:path - both now share resolve_entity_ref.
