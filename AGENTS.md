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

## A root cause is measured, never inferred

**Reading the code tells you what could be wrong. Only running it tells you what
is.** A cause you worked out by reading goes in the notes as a hypothesis, and
it stays a hypothesis until an experiment separates it from every other one.

Paid for on 2026-09-18, twice in one session. Two Windows tests failed;
`gitignore_shadow` and `committed_deletions` were read closely, and the
conclusion — git treats `\` in a pathspec as a glob escape, so the backslash
path matched nothing — was written into a plan, a story, a commit and a
changelog entry. It was wrong. Four `eprintln!` calls on a real Windows box
found the truth in one run: `entries_dir` carried the `\\?\` prefix and `root`
did not, `strip_prefix` returned `None`, and **git was never invoked at all**.
The backslash pathspec works fine; reverting that "fix" leaves the suite green.

- Before writing a fix, state the hypothesis and the observation that would
  falsify it. No falsifier → you are not debugging yet.
- After the fix is green, revert each part **separately** and confirm the
  failure returns. A part that can be removed with the tests still green was
  never the cause and does not ship.
- Never write "root cause verified" for something that was reasoned out. Say
  which command produced the evidence.

## Windows

No maintainer has a Windows machine at their desk, so Windows behaviour is
observed in `Test Windows` or on the host in `~/Gits/CC_Playground/itview/.env`.

- **`std::fs::canonicalize` returns `\\?\C:\…` on Windows.** It names the same
  file and compares equal to nothing. Use `crate::domain::canonicalize_plain`,
  which drops the prefix. This has now caused silent failures three times:
  `strip_prefix` returning `None` in `memory_sync`, SQLite `ATTACH` failing in
  `store::heal`, and `git clone` reading the leading `\\` as a UNC share.
- A path used as a store-key or index-key string goes through
  `crate::domain::rel_key`, which forces `/`. That is about key identity, not
  about git — git for Windows accepts either separator in a pathspec.
- A change that can only be verified on Windows says so in its commit message,
  and names the run or the host that proved it. Do not claim it works locally.
