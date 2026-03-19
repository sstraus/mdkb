# mdkb Memory Guidelines

Guidelines for AI assistants on when and how to use the mdkb memory system for knowledge persistence.

## When to Write Memories

Use `memory_write` in these scenarios:

### 1. After Solving a Problem (type: `problem`)

**Trigger**: You just fixed a bug, resolved an error, or solved a technical challenge.

**Title format**: Describe the **symptom**, not the solution.
- Good: "Null email panic in notifications"
- Bad: "Fixed null check in send_email()"

**Content structure**:
```markdown
## Symptoms
What was observed (error message, behavior)

## Investigation
Steps taken to diagnose

## Root Cause
Why it happened

## Solution
What fixed it (with code snippets)

## Prevention
How to avoid in future
```

**Example**:
```
memory_write(
  id: "bug-null-email-panic",
  title: "Null email panic in notifications",
  type: "problem",
  tags: ["bug", "notifications", "null-safety"],
  content: "## Symptoms\nPanic at send_email() when user has no email...\n\n## Root Cause\n..."
)
```

### 2. After Making an Architectural Decision (type: `decision`)

**Trigger**: You evaluated options and chose an approach with trade-offs.

**Title format**: Describe the **choice made**, not the winner.
- Good: "sqlx vs diesel for database access"
- Bad: "We're using sqlx"

**Content structure**:
```markdown
## Context
Why this decision was needed

## Options Considered
1. Option A - pros/cons
2. Option B - pros/cons

## Decision
What was chosen and why

## Consequences
Trade-offs accepted, future implications
```

**Example**:
```
memory_write(
  id: "decision-sqlx-vs-diesel",
  title: "sqlx vs diesel for database access",
  type: "decision",
  tags: ["database", "architecture", "rust"],
  content: "## Context\nNeed async database access for MCP server...\n\n## Decision\nChose sqlx because..."
)
```

### 3. After Learning a Pattern or Concept (type: `topic`)

**Trigger**: You understood something complex that will be useful again.

**Title format**: Name the **concept or pattern**.
- Good: "Error handling patterns in this codebase"
- Bad: "How we handle errors"

**Content structure**:
```markdown
## Overview
What this pattern/concept is

## Implementation
How it works in this codebase (with examples)

## When to Use
Appropriate scenarios

## Related
Links to docs, other memories
```

**Example**:
```
memory_write(
  id: "topic-error-handling-patterns",
  title: "Error handling patterns in this codebase",
  type: "topic",
  tags: ["patterns", "error-handling", "rust"],
  content: "## Overview\nThis codebase uses thiserror with...\n\n## Implementation\n..."
)
```

## Title Conventions

- **Maximum 50 characters** (enforced)
- Write like a **newspaper headline** - informative but not spoiling
- Focus on **what** (symptom, choice, concept), not **how** (solution)
- Use **lowercase with hyphens** for IDs: `auth-oauth2-pkce`, `bug-null-email`

## Decision Tree: Should I Write This?

```
Did you just...
├─ Fix a bug or error?
│  └─ YES → Write as "problem"
├─ Make a choice between options?
│  └─ YES → Write as "decision"
├─ Learn something complex about this codebase?
│  └─ YES → Write as "topic"
└─ Complete routine work?
   └─ NO → Don't write
```

**Don't write memories for**:
- Trivial fixes (typos, obvious bugs)
- Routine operations (running tests, formatting)
- Information already in docs/README
- Temporary debugging notes

## When to Update vs Create New

**Update existing** (`memory_write` with same ID):
- Adding detail to existing entry
- Correcting information
- Entry is still fundamentally about the same thing

**Create new with supersedes**:
- Complete rethink of the topic
- Previous approach was wrong, not just incomplete
- Significantly different solution

To supersede, use a new ID and update the old entry's status:
1. Create new entry with comprehensive content
2. Old entry can be archived via `mdkb memory prune`

## Tags Best Practices

Use 2-5 tags per entry:

- **Domain tags**: `auth`, `database`, `api`, `ui`
- **Type tags**: `bug`, `performance`, `security`
- **Technology tags**: `rust`, `sqlite`, `tokio`
- **Severity tags**: `critical`, `minor` (for problems)

## Memory Search vs Memory Index

- **Warmup index** (`memory_list`): Top 50 entries by usage. Check this first.
- **Search** (`search(query, scope="memory")`): Hybrid BM25+vector search for specific entries.
- **Confidence**: Each entry has a confidence score (0-1) based on temporal decay, confirmations, and corrections. Use `memory_confirm(id)` when you verify knowledge is still accurate. Use `memory_correct(id, correction)` when you find errors.

The warmup index auto-loads at session start. Use `get(id)` to retrieve full content.

## Integration with Claude Code

Add to your project's `CLAUDE.md`:

```markdown
## Memory Persistence

This project uses mdkb for memory persistence. At session start, you'll receive a memory index.

- Use `get(id)` to retrieve full content
- Use `memory_write(...)` after solving problems, making decisions, or learning patterns
- Follow memory guidelines in docs/memory-guidelines.md
```
