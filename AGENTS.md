# AGENTS.md

Repository conventions for any coding agent working on mdkb. Tracked in git on
purpose: these outlive one machine and one assistant. Machine-local preferences
belong in `CLAUDE.md`, which is gitignored.

## Credit the reporter in the changelog. Always.

**Every `CHANGES.md` entry that resolves a report from outside this repo names
the person who reported it.** Not optional, not "when it feels significant". A
bug report is work — often the hardest part, because it is the half nobody can
do from inside the repo.

Format, already in use throughout `CHANGES.md`:

```
*(#12, reported by Steve Muchow (@smuchow1962))*
```

- Place it at the end of the entry, italic, in parentheses.
- Use the person's real name **and** their GitHub handle. `gh api users/<handle>
  --jq .name` gives the name; do not invent one and do not guess pronouns.
- `reported by` for an issue. `diagnosed by` when they also found the cause.
  `fixed by` when the patch is theirs. Say which one is true.
- Several people on one entry: name all of them.
- A defect found by CI or by the maintainer needs no credit line, but say what
  found it — a reader asking "how did this survive?" deserves the answer.

**Before writing an entry, check who filed the issue.** `gh issue view <n> --json
author`. Closing an issue without crediting its author in the changelog is a
defect in the release notes, and it gets fixed the same way any other defect
does.

## Closing an issue needs evidence, not a fix

An issue closes when a test proves the behaviour on the platform the issue is
about — not when the code that should fix it has landed. Name the test and the
CI run in the closing comment. "Should be fixed by X" is not a close.

## Windows

`Test Windows` is the only place Windows behaviour is ever observed here; no
maintainer has a Windows machine. Two rules follow:

- A path handed to `git` as a pathspec goes through `crate::domain::rel_key`.
  `Path::strip_prefix` yields the native separator, and git reads `\` in a
  pathspec as a glob escape — it matches nothing and reports no error.
- A change that can only be verified on Windows says so in its commit message,
  and the CI run is the proof. Do not claim it works locally.
