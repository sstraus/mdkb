# Memory Export & Import

Export memory entries to markdown files and re-import them — useful for backups, version control, migration, or hand-editing entries.

## Export

```bash
# Export all entries to .mdkb/memory/entries/ (default)
mdkb memory export

# Custom output directory
mdkb memory export --dir ./memories

# Include expired entries (omitted by default)
mdkb memory export --include-expired

# Overwrite existing files
mdkb memory export --overwrite

# Preview without writing
mdkb memory export --dry-run
```

Each entry is written as a separate `.md` file named `<id>.md` with YAML frontmatter:

```markdown
---
id: auth-patterns
title: OAuth2 PKCE Flow
entry_type: topic
tags: [auth, security]
source_type: user_statement
confidence: 0.85
created_at: "2026-04-01T10:00:00Z"
updated_at: "2026-04-01T10:00:00Z"
---

Always use PKCE for public clients...
```

Optional frontmatter fields (omitted when null/zero): `ttl`, `due_at`.

**Derived counters** (`access_count`, `last_accessed`, `confirmations`, `last_confirmed_at`) are written to the file but **reset to zero on import** — they reflect live usage, not authored knowledge.

## Import

```bash
# Import from a directory of .md files (auto-detected)
mdkb memory import .mdkb/memory/entries

# Skip entries that already exist
mdkb memory import .mdkb/memory/entries --skip-duplicates

# Preview without writing
mdkb memory import .mdkb/memory/entries --dry-run

# Legacy: import from a JSON batch file
mdkb memory import entries.json
```

Auto-detection: if the path argument is a directory, all `*.md` files are scanned; if it's a file, it is treated as legacy JSON batch format.

## Round-Trip

Export followed by import recreates the full entry state:

```bash
mdkb memory export --dir /tmp/backup
# ... modify or migrate ...
mdkb memory import /tmp/backup --skip-duplicates
```

Fields preserved: `id`, `title`, `entry_type`, `tags`, `source_type`, `confidence`, `created_at`, `updated_at`, `ttl`, `due_at`, and the entry body.

Fields reset on import: `access_count` → 0, `last_accessed` → null, `confirmations` → 0, `last_confirmed_at` → null.

## Use Cases

- **Version control** — commit `.mdkb/memory/entries/` to track knowledge over time
- **Hand-editing** — edit `.md` files directly, then re-import
- **Migration** — move entries between projects or machines
- **Backup** — snapshot before `mdkb memory delete` operations
