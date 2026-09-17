---
id: dup-ignore-stores-reviewed-membership-d7
title: "D7: the dup ignore list stores the reviewed membership"
entry_type: decision
source_type: user_statement
status: active
tags: [code-intel, duplication, ignore-list, hashing, review-2026-09-16]
created_at: 1789661172
updated_at: 1789661172
---

Plan Step 17 / story 099-6061, landed d627a76. Decision: an accepted cluster records the membership that was reviewed, and stays accepted while its members are a SUBSET of that set. A removal keeps the decision; a cluster splitting in two leaves both halves accepted. Gaining a member resurfaces it — nobody looked at the new copy — labelled 'Changed since accepted' with only the new members marked NEW, so the review that did happen is not repeated.

Rejected: 'any ignored member silences the cluster'. One ignored member would go on silencing genuinely new duplication that joined later.

Member key: (module_path, name) is not stable — two overloads share it and it names a different symbol after a rename, and a collision there silently suppresses somebody else's finding. It is now repo-relative path, language, qualified path, kind, name and signature.

Identity widened from 8 hex to the full 64 (32 bits is a coin flip across a few tens of thousands of clusters). Reports print a 12-hex prefix, JSON carries both, and any unambiguous prefix is accepted as an id — two prefixes that both fit suppress nothing, because a lost finding is silent and an extra one is a line.

No existing entry is rewritten. The old digest cannot be un-hashed, so the membership it stood for is not recoverable and there is nothing to convert: an old entry is left alone, its cluster is reported again, and accepting it once more from a fresh mdkb dup run writes the membership down. That re-run is the whole migration — verified harmless here, this store held 0 dup-ignore entries.

No clone-detection tool has a standard for this: PMD CPD, jscpd, Simian and NiCad all use source markers and path exclusions, SonarQube uses analysis config. A recorded decision over reviewed members is more defensible than copying a supposed standard.
