---
id: test-leaked-tempdir-into-repos-json
title: A test registered its tempdir in the real repo map
entry_type: problem
source_type: user_statement
status: active
tags: [testing, daemon, repo-map, isolation, measured]
created_at: 1789981337
updated_at: 1789981337
---

Measured 2026-09-21. ~/.mdkb/repos.json - the production daemon RepoMap - held /Users/stefano.straus/Gits/.tmp/.tmpxX18gd as a root. That is a cargo test tempdir (tests run with TMPDIR=~/Gits/.tmp per the Defender workaround), so a test reached the real daemon instead of an isolated one and made it adopt a directory that disappears when the test ends. RepoMap only forgets a root whose path is gone, so the entry survived until it was removed by hand. Two consequences: the daemon keeps a root that no longer exists, and any fleet audit of repos.json reports a phantom. The isolation to check is tests/common/cli.rs::isolated_home - some path around the daemon is not taking it.
