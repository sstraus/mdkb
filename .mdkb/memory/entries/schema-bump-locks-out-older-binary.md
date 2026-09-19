---
id: schema-bump-locks-out-older-binary
title: A schema bump locks out the installed binary
entry_type: problem
source_type: user_statement
status: active
tags: [mdkb, schema, migration, operations]
created_at: 1789829393
updated_at: 1789829393
---

refuse_future_schema (src/store/schema.rs) refuses to open a store whose schema_version exceeds the binary's SCHEMA_VERSION. So the moment a new build opens a store, the migration runs and the INSTALLED binary can no longer open it — hooks and daemon included. Consequence for any story that bumps SCHEMA_VERSION (108-e302 took it 27 to 28): do not smoke-test the new build against the live .mdkb store. Use a throwaway store, and install the new binary plus 'mdkb daemon restart' before the live store is opened by it. The additive ALTER TABLE itself is harmless; the version row is what locks the door.
