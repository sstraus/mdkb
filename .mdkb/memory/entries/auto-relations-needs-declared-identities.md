---
id: auto-relations-needs-declared-identities
title: relations=auto is a no-op without declared identities
entry_type: topic
source_type: user_statement
status: active
tags: [graph, relations, auto, precondition, measured]
created_at: 1789990915
updated_at: 1789990915
---

Falsifies a prediction I made. I told the fleet-audit agent that relations=auto would largely self-resolve the 40 repos running the stock allowlist with zero matches. It ran the experiment instead of accepting it. Measured 2026-09-21 on copies of LS/maccollect and LS/quill-builder. auto scores a frontmatter key by whether its values resolve to INDEXED documents, so it does nothing unless documents declare id: or aliases:. quill-builder printed exactly the sentence the feature was built to print: 'No document declares an id: or aliases:, so nothing can resolve by name and no key can be measured. This is a missing precondition, not a finding.' maccollect indexed 19 story docs all carrying dependencies/plan/plan_step and still produced 0 edges - every dependencies value was an empty list, plan_step is a scalar label and correctly scores 0, and the plan targets live in a plans/ directory that was never registered as a collection. Of 61 repos under ~/Gits with frontmatter markdown, 26 declare no identity anywhere, so auto is a guaranteed no-op there. TWO COROLLARIES: counting raw frontmatter key occurrences overstates the graph gap because it counts empty values and scalar labels - mdkb graph relations is the honest instrument; and in several repos the relation keys are invisible because stories/ and plans/ were never registered as collections, not because of the allowlist, so widening allowlists will not help them.
