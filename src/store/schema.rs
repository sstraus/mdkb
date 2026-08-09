//! Database schema management and migrations.

use crate::error::{Error, Result};
use rusqlite::{Connection, OptionalExtension};

/// Current schema version.
pub const SCHEMA_VERSION: i32 = 20;

/// Identifies a legacy System-B behavioural prior: `prior-` plus 16 hex digits.
/// One spelling, used by both the v12 purge and the v20 sweep that cleans up
/// after it — two copies of this pattern is how the two halves drifted apart.
const LEGACY_PRIOR_PREDICATE: &str = "entry_type = 'prior' AND id GLOB \
     'prior-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]\
[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]'";

/// Ids the v12 purge is responsible for, read before the rows go away.
fn legacy_prior_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM memory_entries WHERE {LEGACY_PRIOR_PREDICATE}"
    ))?;
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
}

/// Retire the markdown projection of rows a migration removed.
///
/// Best-effort: a schema migration must not fail because a file could not be
/// renamed. A file left behind is reported by the drift count in `mdkb stats`
/// and swept on a later run; a migration that aborts halfway leaves a store
/// nobody can open.
fn dispose_projections(conn: &Connection, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let Some(memory_dir) = crate::store::memory_file::memory_dir_of(conn) else {
        return;
    };
    for id in ids {
        if let Err(e) = crate::store::memory_file::archive_projection(&memory_dir, id) {
            tracing::warn!("migration: could not archive projection for {id}: {e}");
        }
    }
}

/// Archive every `entries/prior-<16 hex>.md` that has no row behind it.
///
/// Best-effort like [`dispose_projections`], and for the same reason: a store
/// that cannot be opened is worse than a file left on disk.
fn sweep_orphaned_legacy_priors(conn: &Connection) {
    let Some(memory_dir) = crate::store::memory_file::memory_dir_of(conn) else {
        return;
    };
    let Ok(read_dir) = std::fs::read_dir(memory_dir.join("entries")) else {
        return;
    };
    let mut swept = 0usize;
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_legacy_prior_filename(stem) {
            continue;
        }
        let has_row: bool = conn
            .query_row("SELECT 1 FROM memory_entries WHERE id = ?1", [stem], |_| {
                Ok(true)
            })
            .unwrap_or(false);
        if has_row {
            continue;
        }
        match crate::store::memory_file::archive_projection(&memory_dir, stem) {
            Ok(()) => swept += 1,
            Err(e) => tracing::warn!("migration: could not archive orphaned prior {stem}: {e}"),
        }
    }
    if swept > 0 {
        tracing::warn!(
            "migration: archived {swept} orphaned legacy prior projection(s) left by the \
             v11->v12 purge; they are in .mdkb/memory/archive/"
        );
    }
}

/// Does this filename look like a legacy System-B prior projection?
///
/// Matches the SQL predicate above in Rust, for the sweep — which works from
/// filenames, since by definition the rows are already gone.
fn is_legacy_prior_filename(stem: &str) -> bool {
    stem.strip_prefix("prior-").is_some_and(|hex| {
        hex.len() == 16
            && hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}

/// SQL for creating the database schema.
const SCHEMA_SQL: &str = r#"
-- Schema version for migrations
CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY
);

-- Collections configuration
CREATE TABLE IF NOT EXISTS collections (
    name TEXT PRIMARY KEY,
    path TEXT NOT NULL,
    pattern TEXT DEFAULT '**/*.md',
    source TEXT DEFAULT 'manual',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Content-addressable storage (deduplication)
CREATE TABLE IF NOT EXISTS content (
    hash TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

-- Documents (file system mapping)
CREATE TABLE IF NOT EXISTS documents (
    id INTEGER PRIMARY KEY,
    collection TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    hash TEXT NOT NULL,
    title TEXT,
    metadata TEXT,
    file_modified_at INTEGER NOT NULL,
    indexed_at INTEGER NOT NULL,
    status TEXT DEFAULT 'current',        -- current, superseded, retracted
    status_reason TEXT,                   -- Reason for status change
    version TEXT,                         -- Optional version identifier
    FOREIGN KEY(collection) REFERENCES collections(name) ON DELETE CASCADE,
    FOREIGN KEY(hash) REFERENCES content(hash),
    UNIQUE(collection, relative_path)
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_documents_collection ON documents(collection);
CREATE INDEX IF NOT EXISTS idx_documents_hash ON documents(hash);
CREATE INDEX IF NOT EXISTS idx_documents_path ON documents(relative_path);

-- Full-text search index with porter stemmer + column weighting
CREATE VIRTUAL TABLE IF NOT EXISTS documents_fts USING fts5(
    title,
    body,
    tokenize = 'porter unicode61',
    content='',
    content_rowid='id'
);

-- Trigger to keep FTS in sync on INSERT
CREATE TRIGGER IF NOT EXISTS documents_ai AFTER INSERT ON documents BEGIN
    INSERT INTO documents_fts(rowid, title, body)
    SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
END;

-- Trigger to keep FTS in sync on DELETE
CREATE TRIGGER IF NOT EXISTS documents_ad AFTER DELETE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, title, body)
    VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
END;

-- Trigger to keep FTS in sync on UPDATE
CREATE TRIGGER IF NOT EXISTS documents_au AFTER UPDATE ON documents BEGIN
    INSERT INTO documents_fts(documents_fts, rowid, title, body)
    VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
    INSERT INTO documents_fts(rowid, title, body)
    SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
END;

-- Memory entries for AI knowledge persistence
CREATE TABLE IF NOT EXISTS memory_entries (
    id TEXT PRIMARY KEY,              -- slug: "auth-oauth2-flow"
    title TEXT NOT NULL,              -- Concise title (max 50 chars)
    content TEXT NOT NULL,            -- Full markdown content
    entry_type TEXT NOT NULL,         -- topic, problem, decision
    tags TEXT NOT NULL DEFAULT '[]',  -- JSON array: ["auth", "security"]
    status TEXT DEFAULT 'active',     -- active, superseded, archived
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    superseded_by TEXT,               -- ID of newer entry
    access_count INTEGER DEFAULT 0,   -- Track usage for ranking
    last_accessed INTEGER,
    source_path TEXT,                 -- Original file path (for journal imports)
    confirmations INTEGER DEFAULT 0,  -- Positive confidence signals
    corrections INTEGER DEFAULT 0,    -- Negative confidence signals
    last_confirmed_at INTEGER,        -- Timestamp of last confirmation
    source_type TEXT DEFAULT 'user_statement',  -- official_docs, user_statement, inference
    expires_at INTEGER,                        -- Unix timestamp; NULL = permanent
    due_at INTEGER,                            -- Unix timestamp; surfaces reminders at/after this time
    created_session TEXT,                      -- session id that authored this entry (provenance)
    created_agent TEXT,                        -- agent/tool that authored this entry (provenance)
    projected_at INTEGER,                      -- when the markdown projection was last written; NULL = never
    projected_hash TEXT                        -- SHA-256 of the bytes last projected; NULL = predates the column
);

CREATE INDEX IF NOT EXISTS idx_memory_type ON memory_entries(entry_type);
CREATE INDEX IF NOT EXISTS idx_memory_status ON memory_entries(status);
CREATE INDEX IF NOT EXISTS idx_memory_access ON memory_entries(access_count DESC);

-- FTS for memory content search (includes tags as space-separated text)
CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    id,
    title,
    content,
    tags,
    tokenize = 'porter unicode61',
    content='',
    content_rowid='rowid'
);

-- Triggers to keep memory FTS in sync
-- Tags stored as JSON array, stripped to space-separated text for FTS
CREATE TRIGGER IF NOT EXISTS memory_ai AFTER INSERT ON memory_entries BEGIN
    INSERT INTO memory_fts(rowid, id, title, content, tags)
    VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
            REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
END;

CREATE TRIGGER IF NOT EXISTS memory_ad AFTER DELETE ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
    VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
            REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
END;

-- Scoped to the indexed columns on purpose. `get_entry` bumps `access_count`
-- and `last_accessed` on every read, so an unscoped AFTER UPDATE made every
-- memory *read* delete and reinsert the entry's FTS5 segments — churning the
-- blob-heavy `memory_fts_data` shadow table for a counter the index never sees.
CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE OF id, title, content, tags
ON memory_entries BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
    VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
            REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
    INSERT INTO memory_fts(rowid, id, title, content, tags)
    VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
            REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
END;

-- Memory revision history (max 3 per entry, stores diffs)
CREATE TABLE IF NOT EXISTS memory_revisions (
    id INTEGER PRIMARY KEY,
    memory_id TEXT NOT NULL,
    diff TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id ON memory_revisions(memory_id);

-- Evolution tracking for document relationships (RFC-style)
CREATE TABLE IF NOT EXISTS evolution (
    id INTEGER PRIMARY KEY,
    source_doc_id INTEGER NOT NULL,      -- The newer/superseding document
    target_doc_id INTEGER NOT NULL,      -- The older/superseded document
    relationship TEXT NOT NULL,          -- supersedes, updates, corrects, retracts, extends
    scope TEXT,                          -- NULL = full doc, or section path
    reason TEXT,                         -- Explanation for the evolution
    created_at INTEGER NOT NULL,
    FOREIGN KEY(source_doc_id) REFERENCES documents(id) ON DELETE CASCADE,
    FOREIGN KEY(target_doc_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_evolution_source ON evolution(source_doc_id);
CREATE INDEX IF NOT EXISTS idx_evolution_target ON evolution(target_doc_id);

-- Knowledge-graph edges: typed relations from a document to an entity slug/path.
-- source is always an indexed document; target_ref is free text that may resolve
-- to a document at query time, or stay dangling until its target is indexed.
CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY,
    source_doc_id INTEGER NOT NULL,      -- The document the edge originates from
    target_ref TEXT NOT NULL,            -- Raw slug/path of the target entity
    relation TEXT NOT NULL,              -- Frontmatter key, or "links_to" for wikilinks
    source_kind TEXT NOT NULL,           -- 'frontmatter' (strong) | 'wikilink' (soft)
    scope TEXT,                          -- NULL = full doc, or section path
    created_at INTEGER NOT NULL,
    FOREIGN KEY(source_doc_id) REFERENCES documents(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_doc_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_ref);
CREATE INDEX IF NOT EXISTS idx_edges_relation ON edges(relation);

-- Behavioral prior clusters: deduped, promotable "when X do Y" lessons mined
-- from sessions. A cluster accumulates recurrence evidence across sessions and
-- is promoted into memory_entries (entry_type=prior) once it clears the gate.
CREATE TABLE IF NOT EXISTS prior_clusters (
    id TEXT PRIMARY KEY,                     -- canonical trigger-key hash
    canonical_trigger_key TEXT NOT NULL,     -- normalized trigger identity (dedup key)
    trigger_kind TEXT NOT NULL,              -- prompt|pre_tool|post_tool|stop|repo
    trigger_matcher TEXT NOT NULL,           -- JSON: machine-matchable condition
    lesson TEXT NOT NULL,                    -- imperative lesson, <=160 chars
    scope TEXT NOT NULL,                     -- JSON: {repo, languages, paths}
    evidence_count INTEGER NOT NULL DEFAULT 0,
    distinct_sessions INTEGER NOT NULL DEFAULT 0,
    injected_count INTEGER NOT NULL DEFAULT 0,
    confirmed_count INTEGER NOT NULL DEFAULT 0,
    refuted_count INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'candidate',  -- candidate|promoted|refuted|expired
    promoted_memory_id TEXT,                 -- memory_entries.id once promoted
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    embedding BLOB,                          -- lesson embedding (f32 LE) for semantic cluster-merge
    FOREIGN KEY(promoted_memory_id) REFERENCES memory_entries(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_prior_clusters_trigger ON prior_clusters(canonical_trigger_key);
CREATE INDEX IF NOT EXISTS idx_prior_clusters_state ON prior_clusters(state);

-- Individual observed episodes (one per session) feeding a cluster. Never
-- injected directly; they accumulate into a cluster which may be promoted.
CREATE TABLE IF NOT EXISTS prior_candidates (
    id TEXT PRIMARY KEY,
    cluster_id TEXT,                         -- assigned when merged into a cluster
    state TEXT NOT NULL DEFAULT 'candidate',
    trigger_kind TEXT NOT NULL,
    trigger_matcher TEXT NOT NULL,           -- JSON
    lesson TEXT NOT NULL,
    scope TEXT NOT NULL,                     -- JSON
    evidence_failure TEXT,
    evidence_fix TEXT,
    source_session TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(cluster_id) REFERENCES prior_clusters(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_prior_candidates_cluster ON prior_candidates(cluster_id);

-- Typed memory graph edges: a relation from a memory entry to another memory
-- entry or a document. source is always an existing memory (FK, cascade delete);
-- target_ref is free text (memory slug or doc relative path) resolved at query
-- time, dangling-tolerant like the doc `edges` table.
CREATE TABLE IF NOT EXISTS memory_edges (
    source_id   TEXT NOT NULL REFERENCES memory_entries(id) ON DELETE CASCADE,
    target_ref  TEXT NOT NULL,                    -- memory slug or doc relative path
    target_kind TEXT NOT NULL DEFAULT 'memory',   -- 'memory' | 'doc'
    relation    TEXT NOT NULL,                    -- supports|contradicts|supersedes|derived_from|relates_to
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (source_id, target_ref, relation)
);
CREATE INDEX IF NOT EXISTS idx_memedges_source ON memory_edges(source_id);
CREATE INDEX IF NOT EXISTS idx_memedges_target ON memory_edges(target_ref);
"#;

/// SQL for setting BM25 column weights (title 10x, body 1x).
const BM25_WEIGHTS_SQL: &str = r#"
INSERT OR REPLACE INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');
"#;

/// Initialize the database schema.
pub fn init_schema(conn: &Connection) -> Result<()> {
    // Enable foreign keys
    conn.execute_batch("PRAGMA foreign_keys = ON;")?;

    // Refuse a store from the future before executing any schema or ranking
    // statement. An older binary must not modify the database it says it cannot
    // understand.
    refuse_future_schema(conn)?;

    // Create schema
    conn.execute_batch(SCHEMA_SQL)?;

    // Set BM25 weights
    conn.execute_batch(BM25_WEIGHTS_SQL)?;

    // Check for migrations
    let current = get_schema_version(conn)?;
    match current {
        None => {
            // Fresh database
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?)",
                [SCHEMA_VERSION],
            )?;
        }
        Some(v) if v < SCHEMA_VERSION => {
            // Run migrations
            migrate_schema(conn, v)?;
        }
        _ => {
            // Up to date
        }
    }

    Ok(())
}

/// Refuse a store created by a newer binary without modifying it.
pub fn refuse_future_schema(conn: &Connection) -> Result<()> {
    if let Some(v) = get_schema_version(conn)?
        && v > SCHEMA_VERSION
    {
        return Err(Error::other(format!(
            "store schema is v{v}, but this mdkb binary understands v{SCHEMA_VERSION}. \
             Refusing to open: a newer store served by an older binary corrupts it \
             silently. Upgrade mdkb, and restart any running daemon (`mdkb daemon \
             restart`) so it is not left on the old build."
        )));
    }
    Ok(())
}

/// Migrate schema from old version to current.
///
/// Wrapped in a transaction for atomicity — partial migration on crash
/// is rolled back automatically by SQLite.
fn migrate_schema(conn: &Connection, from_version: i32) -> Result<()> {
    conn.execute("BEGIN IMMEDIATE", [])?;
    let result = migrate_schema_inner(conn, from_version);
    match &result {
        Ok(()) => {
            conn.execute("COMMIT", [])?;
        }
        Err(_) => {
            let _ = conn.execute("ROLLBACK", []);
        }
    }
    result
}

fn migrate_schema_inner(conn: &Connection, from_version: i32) -> Result<()> {
    // Migration from v1 to v2: add status/version columns to documents and evolution table
    if from_version < 2 {
        // Add new columns to documents table if they don't exist
        // SQLite doesn't have IF NOT EXISTS for ALTER TABLE, so we check first
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_status {
            conn.execute_batch(
                r#"
                ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
                ALTER TABLE documents ADD COLUMN status_reason TEXT;
                ALTER TABLE documents ADD COLUMN version TEXT;
                "#,
            )?;
        }

        // Evolution table is created by SCHEMA_SQL with IF NOT EXISTS
    }

    // Migration from v2 to v3: add source_path to memory_entries
    if from_version < 3 {
        let has_source_path: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'source_path'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_source_path {
            conn.execute("ALTER TABLE memory_entries ADD COLUMN source_path TEXT", [])?;
        }
    }

    // Migration from v3 to v4: add source column to collections
    if from_version < 4 {
        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_source {
            conn.execute(
                "ALTER TABLE collections ADD COLUMN source TEXT DEFAULT 'manual'",
                [],
            )?;
        }
    }

    // Migration from v4 to v5: fix FTS INSERT trigger to use NEW.rowid directly
    if from_version < 5 {
        conn.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS memory_ai;
            CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries BEGIN
                INSERT INTO memory_fts(rowid, id, title, content)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content);
            END;
            "#,
        )?;
    }

    // Migration from v5 to v6: add tags column to memory FTS
    if from_version < 6 {
        conn.execute_batch(
            r#"
            DROP TABLE IF EXISTS memory_fts;
            CREATE VIRTUAL TABLE memory_fts USING fts5(
                id, title, content, tags,
                tokenize = 'porter unicode61',
                content='', content_rowid='rowid'
            );

            DROP TRIGGER IF EXISTS memory_ai;
            DROP TRIGGER IF EXISTS memory_ad;
            DROP TRIGGER IF EXISTS memory_au;

            CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries BEGIN
                INSERT INTO memory_fts(rowid, id, title, content, tags)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
                        REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
            END;
            CREATE TRIGGER memory_ad AFTER DELETE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
                        REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
            END;
            CREATE TRIGGER memory_au AFTER UPDATE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
                        REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
                INSERT INTO memory_fts(rowid, id, title, content, tags)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
                        REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
            END;

            -- Repopulate FTS from existing entries
            INSERT INTO memory_fts(rowid, id, title, content, tags)
            SELECT rowid, id, title, content,
                   REPLACE(REPLACE(REPLACE(tags, '"', ''), '[', ''), ']', '')
            FROM memory_entries;
            "#,
        )?;
    }

    // Migration from v6 to v7: add confidence columns to memory_entries
    if from_version < 7 {
        let has_confirmations: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'confirmations'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_confirmations {
            conn.execute_batch(
                r#"
                ALTER TABLE memory_entries ADD COLUMN confirmations INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN corrections INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN last_confirmed_at INTEGER;
                ALTER TABLE memory_entries ADD COLUMN source_type TEXT DEFAULT 'user_statement';
                "#,
            )?;
        }
    }

    // Migration from v7 to v8: add memory_revisions table for change history
    if from_version < 8 {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS memory_revisions (
                id INTEGER PRIMARY KEY,
                memory_id TEXT NOT NULL,
                diff TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id
                ON memory_revisions(memory_id);
            "#,
        )?;
    }

    // Migration from v8 to v9: add expires_at column to memory_entries for TTL support
    if from_version < 9 {
        let has_expires_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'expires_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_expires_at {
            conn.execute_batch("ALTER TABLE memory_entries ADD COLUMN expires_at INTEGER;")?;
        }
    }

    // Migration from v9 to v10: add due_at column to memory_entries for reminder support
    if from_version < 10 {
        let has_due_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'due_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_due_at {
            conn.execute_batch("ALTER TABLE memory_entries ADD COLUMN due_at INTEGER;")?;
        }
    }

    // Migration from v10 to v11: knowledge-graph edges table.
    // Table + indexes are created by SCHEMA_SQL with IF NOT EXISTS (runs on every
    // open), so existing databases pick it up here without an explicit DDL step.

    // Migration from v11 to v12: purge legacy "System B" behavioral priors.
    //
    // A removed prior-generator minted one durable `prior` per session with a
    // transcript-hash id (`prior-<16 hex>`) and a useless title/body
    // (`fix: fix|tools:Edit->Bash->...`). The writer is gone, but the rows
    // linger — many with no `expires_at`, so they never age out and get
    // re-injected into every session's context, burning tokens for zero signal.
    // This purge runs once per database on upgrade.
    //
    // The id pattern MUST be exactly `^prior-[0-9a-f]{16}$` (GLOB anchors the
    // whole string, so the 16 char-classes also fix the length). This spares
    // System-A priors (`prior-proof-*`) and hand-authored priors with slug ids
    // (e.g. `canvas-terminal-dont-reinvent-xterm`). Deletes cascade to FTS,
    // embeddings, and revisions via existing triggers/foreign keys.
    if from_version < 12 {
        // Collect before deleting: after the DELETE there is nothing left to say
        // which files the purge is responsible for.
        let purged = legacy_prior_ids(conn)?;
        conn.execute(
            &format!("DELETE FROM memory_entries WHERE {LEGACY_PRIOR_PREDICATE}"),
            [],
        )?;
        dispose_projections(conn, &purged);
    }

    // Migration from v12 to v13: behavioral-prior mining tables (prior_clusters,
    // prior_candidates). Created by SCHEMA_SQL with IF NOT EXISTS (runs on every
    // open), so existing databases pick them up here without an explicit DDL step.

    // Migration from v13 to v14: typed memory graph.
    // The `memory_edges` table + indexes are created by SCHEMA_SQL with IF NOT
    // EXISTS (runs on every open), so they are already present here. The
    // provenance columns need explicit ALTER guarded on pragma_table_info.
    if from_version < 14 {
        let has_provenance: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'created_session'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if !has_provenance {
            conn.execute_batch(
                r#"
                ALTER TABLE memory_entries ADD COLUMN created_session TEXT;
                ALTER TABLE memory_entries ADD COLUMN created_agent TEXT;
                "#,
            )?;
        }
    }

    // Migration from v14 to v15: lesson embedding on prior_clusters for semantic
    // cluster-merge (equivalent lessons from a non-deterministic distiller land on
    // different canonical trigger keys; the embedding lets them merge so recurrence
    // can promote). Nullable BLOB, added by explicit ALTER on existing databases.
    if from_version < 15 {
        // On a real open SCHEMA_SQL runs first, so prior_clusters exists (created
        // fresh WITH the column, or pre-existing WITHOUT it). Migration unit tests
        // call migrate_schema directly with no SCHEMA_SQL, so the table may be
        // absent — guard on its existence before ALTER.
        let table_exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'prior_clusters'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        let has_embedding: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('prior_clusters') WHERE name = 'embedding'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);

        if table_exists && !has_embedding {
            conn.execute("ALTER TABLE prior_clusters ADD COLUMN embedding BLOB", [])?;
        }
    }

    // Migration from v15 to v16: `sessions.agent` labels a session's origin so
    // hook traffic (which has no MCP session) can be attributed to a reserved
    // `agent='hooks'` pseudo-session instead of being dropped from call stats.
    if from_version < 16 {
        let table_exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sessions'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        let has_agent: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'agent'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if table_exists && !has_agent {
            conn.execute("ALTER TABLE sessions ADD COLUMN agent TEXT", [])?;
        }
    }

    // Migration from v16 to v17: `memory_entries.projected_at` marks when an
    // entry's markdown projection was last written. NULL = never projected (a
    // DB-only entry to backfill); set = projected (so a subsequently-missing
    // file is a deliberate deletion, archived on next sync). Distinguishes the
    // two so file sync never wrongly archives never-projected DB entries.
    if from_version < 17 {
        let has_projected: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'projected_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_projected {
            conn.execute(
                "ALTER TABLE memory_entries ADD COLUMN projected_at INTEGER",
                [],
            )?;
        }
    }

    // Migration from v17 to v18: scope the memory FTS update trigger to the
    // columns the index actually stores. Before this, `get_entry`'s
    // access-count bump fired a full FTS5 delete+reinsert, so every memory read
    // rewrote `memory_fts_data` segments — the single hottest writer in the
    // store, on one of the three tables that field corruption keeps damaging.
    if from_version < 18 {
        conn.execute_batch(
            r#"
            DROP TRIGGER IF EXISTS memory_au;
            CREATE TRIGGER memory_au AFTER UPDATE OF id, title, content, tags
            ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content, tags)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content,
                        REPLACE(REPLACE(REPLACE(OLD.tags, '"', ''), '[', ''), ']', ''));
                INSERT INTO memory_fts(rowid, id, title, content, tags)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content,
                        REPLACE(REPLACE(REPLACE(NEW.tags, '"', ''), '[', ''), ']', ''));
            END;
            "#,
        )?;
    }

    // Migration from v18 to v19: `memory_entries.projected_hash` records the
    // SHA-256 of the exact bytes last written to the entry's markdown file, so
    // reconciliation can tell "the file changed" from "the file was rewritten by
    // a checkout". mtime cannot answer that — git stamps every file it writes
    // during a checkout/merge with the checkout time, so after any `git pull`
    // every pulled file, changed or not, looks newer than `projected_at`.
    // NULL = the projection predates this column (re-project once, §migration).
    if from_version < 19 {
        let has_hash: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'projected_hash'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_hash {
            conn.execute(
                "ALTER TABLE memory_entries ADD COLUMN projected_hash TEXT",
                [],
            )?;
        }
    }

    // Migration from v19 to v20: sweep the projections the v12 prior purge left
    // behind. A store that migrated before the purge learned to dispose of its
    // files is already past v12, so replaying that migration will never reach
    // it — the heal has to be its own one-time step, which is exactly what a
    // migration is for. Measured on one store: 113 files, every one matching the
    // purge pattern, all with `status: active` frontmatter and no row.
    //
    // Since bidirectional sync (v19) these are not litter but a live bug: a file
    // with no row is imported, so the next `mdkb update` would resurrect the
    // rows the purge deleted.
    //
    // Only files whose id has the legacy shape AND has no row are touched. A
    // colleague's entry arriving in a checkout also has no row, and must still
    // import.
    if from_version < 20 {
        sweep_orphaned_legacy_priors(conn);
    }

    // Update schema version
    conn.execute("UPDATE schema_version SET version = ?", [SCHEMA_VERSION])?;

    Ok(())
}

/// Get the current schema version from the database.
pub fn get_schema_version(conn: &Connection) -> Result<Option<i32>> {
    // Check if table exists first
    let exists = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
            [],
            |_| Ok(true),
        )
        .optional()?;

    if exists.is_none() {
        return Ok(None);
    }

    let version = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .optional()?;

    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        Connection::open_in_memory().expect("failed to open in-memory db")
    }

    // ==================== Schema Version Tests ====================

    #[test]
    fn test_init_schema_creates_version_table() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // schema_version table should exist
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='schema_version'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "schema_version table should exist");
    }

    #[test]
    fn test_init_schema_sets_version() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let version = get_schema_version(&conn)
            .expect("get_schema_version failed")
            .expect("version should be set");
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn test_init_schema_idempotent() {
        let conn = setup_db();
        init_schema(&conn).expect("first init_schema failed");
        init_schema(&conn).expect("second init_schema should succeed (idempotent)");

        let version = get_schema_version(&conn)
            .expect("get_schema_version failed")
            .expect("version should be set");
        assert_eq!(version, SCHEMA_VERSION);
    }

    // ==================== Collections Table Tests ====================

    #[test]
    fn test_collections_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='collections'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "collections table should exist");
    }

    #[test]
    fn test_collections_table_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Insert a collection to verify columns
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        )
        .expect("insert into collections should work");

        let (name, path, pattern): (String, String, String) = conn
            .query_row(
                "SELECT name, path, pattern FROM collections WHERE name = ?",
                ["docs"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query collections should work");

        assert_eq!(name, "docs");
        assert_eq!(path, "./docs");
        assert_eq!(pattern, "**/*.md");
    }

    #[test]
    fn test_collections_name_is_primary_key() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        )
        .expect("first insert should work");

        // Second insert with same name should fail
        let result = conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./other", "**/*.md", "1706700000", "1706700000"],
        );
        assert!(result.is_err(), "duplicate collection name should fail");
    }

    // ==================== Content Table Tests ====================

    #[test]
    fn test_content_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "content table should exist");
    }

    #[test]
    fn test_content_deduplication_by_hash() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let hash = "abc123def456";
        let body = "# Hello World\n\nThis is content.";

        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [hash, body, "1706700000"],
        )
        .expect("first insert should work");

        // Same hash should fail (content deduplication)
        let result = conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [hash, "different body", "1706700000"],
        );
        assert!(
            result.is_err(),
            "duplicate hash should fail (deduplication)"
        );
    }

    // ==================== Documents Table Tests ====================

    #[test]
    fn test_documents_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='documents'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "documents table should exist");
    }

    #[test]
    fn test_documents_table_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up collection and content first
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        )
        .expect("insert collection");

        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash123", "# Test\n\nBody content", "1706700000"],
        )
        .expect("insert content");

        // Insert document
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, metadata, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "hash123", "Test", r#"{"tags": ["test"]}"#, "1706700000", "1706700000"],
        )
        .expect("insert document should work");

        let (id, title, metadata): (i64, String, String) = conn
            .query_row(
                "SELECT id, title, metadata FROM documents WHERE relative_path = ?",
                ["readme.md"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("query document");

        assert!(id > 0);
        assert_eq!(title, "Test");
        assert!(metadata.contains("tags"));
    }

    #[test]
    fn test_documents_unique_path_per_collection() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "body1", "1706700000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash2", "body2", "1706700000"],
        )
        .unwrap();

        // First document
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "hash1", "Test 1", "1706700000", "1706700000"],
        )
        .expect("first document");

        // Same path in same collection should fail
        let result = conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "hash2", "Test 2", "1706700000", "1706700000"],
        );
        assert!(
            result.is_err(),
            "duplicate path in same collection should fail"
        );
    }

    #[test]
    fn test_documents_foreign_key_to_collection() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "body", "1706700000"],
        )
        .unwrap();

        // Insert document with non-existent collection should fail
        let result = conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["nonexistent", "readme.md", "hash1", "Test", "1706700000", "1706700000"],
        );
        assert!(
            result.is_err(),
            "foreign key constraint should prevent invalid collection"
        );
    }

    // ==================== FTS5 Tests ====================

    #[test]
    fn test_fts5_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='documents_fts'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "documents_fts FTS5 table should exist");
    }

    #[test]
    fn test_fts5_uses_porter_stemmer() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Check FTS5 table SQL contains porter tokenizer
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name='documents_fts'",
                [],
                |row| row.get(0),
            )
            .expect("should get FTS table SQL");

        assert!(
            sql.to_lowercase().contains("porter"),
            "FTS5 should use porter stemmer, got: {sql}"
        );
    }

    #[test]
    fn test_fts5_search_with_stemming() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up test data
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [
                "hash1",
                "The runners are running quickly through the park",
                "1706700000",
            ],
        )
        .unwrap();

        // Insert document (should trigger FTS update via trigger)
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash1", "Running Test", "1706700000", "1706700000"],
        ).unwrap();

        // Search for "run" should match "runners" and "running" due to stemming
        let count: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents_fts WHERE documents_fts MATCH 'run'",
                [],
                |row| row.get(0),
            )
            .expect("FTS search should work");

        assert_eq!(
            count, 1,
            "stemmed search for 'run' should find document with 'running'"
        );
    }

    #[test]
    fn test_fts5_bm25_ranking() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up test data
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();

        // Doc 1: "rust" in title only
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "This is about programming.", "1706700000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "doc1.md", "hash1", "Rust Guide", "1706700000", "1706700000"],
        ).unwrap();

        // Doc 2: "rust" in body only
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            [
                "hash2",
                "Rust is a systems programming language.",
                "1706700000",
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "doc2.md", "hash2", "Programming Languages", "1706700000", "1706700000"],
        ).unwrap();

        // Search with BM25 ranking - title match should rank higher (10x weight)
        let results: Vec<(i64, f64)> = conn
            .prepare("SELECT rowid, bm25(documents_fts) FROM documents_fts WHERE documents_fts MATCH 'rust' ORDER BY bm25(documents_fts)")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(results.len(), 2, "should find 2 documents");
        // First result should have better (more negative) BM25 score
        // BM25 returns negative scores, more negative = better match
        assert!(
            results[0].1 < results[1].1,
            "title match (doc1) should rank higher than body match (doc2)"
        );
    }

    // ==================== Trigger Tests ====================

    #[test]
    fn test_fts_insert_trigger() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "Searchable body content here", "1706700000"],
        )
        .unwrap();

        // FTS should be empty before document insert
        let count_before: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 0);

        // Insert document
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash1", "Searchable Title", "1706700000", "1706700000"],
        ).unwrap();

        // FTS should have 1 entry after document insert (trigger fired)
        let count_after: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 1, "FTS insert trigger should add entry");

        // Verify content is searchable
        let found: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM documents_fts WHERE documents_fts MATCH 'searchable'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, 1);
    }

    #[test]
    fn test_fts_delete_trigger() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up and insert
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash1", "Body content", "1706700000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash1", "Title", "1706700000", "1706700000"],
        ).unwrap();

        let count_before: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_before, 1);

        // Delete document
        conn.execute("DELETE FROM documents WHERE relative_path = 'test.md'", [])
            .unwrap();

        // FTS should be empty after delete (trigger fired)
        let count_after: i32 = conn
            .query_row("SELECT COUNT(*) FROM documents_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count_after, 0, "FTS delete trigger should remove entry");
    }

    // ==================== Index Tests ====================

    #[test]
    fn test_indexes_exist() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name NOT LIKE 'sqlite_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        // Should have at least these indexes
        assert!(
            indexes
                .iter()
                .any(|i| i.contains("documents") || i.contains("doc")),
            "should have document-related indexes"
        );
    }

    // ==================== Evolution Table Tests ====================

    #[test]
    fn test_evolution_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='evolution'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "evolution table should exist");
    }

    #[test]
    fn test_evolution_indexes_exist() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_evolution_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(
            indexes.iter().any(|i| i.contains("source")),
            "should have idx_evolution_source index"
        );
        assert!(
            indexes.iter().any(|i| i.contains("target")),
            "should have idx_evolution_target index"
        );
    }

    #[test]
    fn test_edges_table_exists() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='edges'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(exists, "edges table should exist");
    }

    #[test]
    fn test_edges_table_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // A row referencing a non-existent document must fail the FK, proving the
        // FK is wired; insert a document first, then a valid edge.
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at)
             VALUES ('docs', './docs', '**/*.md', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('h1', '# A', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at)
             VALUES ('docs', 'a.md', 'h1', 1, 1)",
            [],
        )
        .unwrap();
        let doc_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO edges (source_doc_id, target_ref, relation, source_kind, scope, created_at)
             VALUES (?1, 'alice', 'owner', 'frontmatter', NULL, 1)",
            [doc_id],
        )
        .expect("inserting a valid edge should work");

        let (target_ref, relation, source_kind): (String, String, String) = conn
            .query_row(
                "SELECT target_ref, relation, source_kind FROM edges WHERE source_doc_id = ?1",
                [doc_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(target_ref, "alice");
        assert_eq!(relation, "owner");
        assert_eq!(source_kind, "frontmatter");
    }

    #[test]
    fn test_edges_indexes_exist() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_edges_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(
            indexes.iter().any(|i| i.contains("source")),
            "should have idx_edges_source index"
        );
        assert!(
            indexes.iter().any(|i| i.contains("target")),
            "should have idx_edges_target index"
        );
        assert!(
            indexes.iter().any(|i| i.contains("relation")),
            "should have idx_edges_relation index"
        );
    }

    #[test]
    fn test_documents_has_status_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Check status column exists
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_status, "documents table should have status column");

        // Check status_reason column exists
        let has_reason: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status_reason'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_reason,
            "documents table should have status_reason column"
        );

        // Check version column exists
        let has_version: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'version'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_version, "documents table should have version column");
    }

    #[test]
    fn test_documents_status_default() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        // Set up collection and content first
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1706700000", "1706700000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["hash123", "# Test", "1706700000"],
        )
        .unwrap();

        // Insert document without specifying status
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "test.md", "hash123", "Test", "1706700000", "1706700000"],
        ).unwrap();

        let status: String = conn
            .query_row(
                "SELECT status FROM documents WHERE relative_path = 'test.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(status, "current", "default status should be 'current'");
    }

    #[test]
    fn test_schema_version_is_current() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let version = get_schema_version(&conn).unwrap().unwrap();
        assert_eq!(
            version, SCHEMA_VERSION,
            "schema version should match SCHEMA_VERSION"
        );
    }

    // ==================== Migration Tests ====================
    //
    // These tests create a genuine old schema from scratch (no init_schema),
    // then call migrate_schema to verify migrations work correctly.

    /// Create a v1 schema: no status/version columns on documents, no evolution table,
    /// no source_path on memory_entries, no source on collections.
    fn create_v1_schema(conn: &Connection) {
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_version (version INTEGER PRIMARY KEY);
            INSERT INTO schema_version (version) VALUES (1);

            CREATE TABLE collections (
                name TEXT PRIMARY KEY,
                path TEXT NOT NULL,
                pattern TEXT DEFAULT '**/*.md',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE content (
                hash TEXT PRIMARY KEY,
                body TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );

            CREATE TABLE documents (
                id INTEGER PRIMARY KEY,
                collection TEXT NOT NULL,
                relative_path TEXT NOT NULL,
                hash TEXT NOT NULL,
                title TEXT,
                metadata TEXT,
                file_modified_at INTEGER NOT NULL,
                indexed_at INTEGER NOT NULL,
                FOREIGN KEY(collection) REFERENCES collections(name) ON DELETE CASCADE,
                FOREIGN KEY(hash) REFERENCES content(hash),
                UNIQUE(collection, relative_path)
            );

            CREATE INDEX idx_documents_collection ON documents(collection);
            CREATE INDEX idx_documents_hash ON documents(hash);
            CREATE INDEX idx_documents_path ON documents(relative_path);

            CREATE VIRTUAL TABLE documents_fts USING fts5(
                title, body,
                tokenize = 'porter unicode61',
                content='', content_rowid='id'
            );

            CREATE TRIGGER documents_ai AFTER INSERT ON documents BEGIN
                INSERT INTO documents_fts(rowid, title, body)
                SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
            END;
            CREATE TRIGGER documents_ad AFTER DELETE ON documents BEGIN
                INSERT INTO documents_fts(documents_fts, rowid, title, body)
                VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
            END;
            CREATE TRIGGER documents_au AFTER UPDATE ON documents BEGIN
                INSERT INTO documents_fts(documents_fts, rowid, title, body)
                VALUES('delete', OLD.id, OLD.title, (SELECT body FROM content WHERE hash = OLD.hash));
                INSERT INTO documents_fts(rowid, title, body)
                SELECT NEW.id, NEW.title, c.body FROM content c WHERE c.hash = NEW.hash;
            END;

            CREATE TABLE memory_entries (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                content TEXT NOT NULL,
                entry_type TEXT NOT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                status TEXT DEFAULT 'active',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                superseded_by TEXT,
                access_count INTEGER DEFAULT 0,
                last_accessed INTEGER
            );

            CREATE INDEX idx_memory_type ON memory_entries(entry_type);
            CREATE INDEX idx_memory_status ON memory_entries(status);
            CREATE INDEX idx_memory_access ON memory_entries(access_count DESC);

            CREATE VIRTUAL TABLE memory_fts USING fts5(
                id, title, content,
                tokenize = 'porter unicode61',
                content='', content_rowid='rowid'
            );

            CREATE TRIGGER memory_ai AFTER INSERT ON memory_entries BEGIN
                INSERT INTO memory_fts(rowid, id, title, content)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content);
            END;
            CREATE TRIGGER memory_ad AFTER DELETE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content);
            END;
            CREATE TRIGGER memory_au AFTER UPDATE ON memory_entries BEGIN
                INSERT INTO memory_fts(memory_fts, rowid, id, title, content)
                VALUES('delete', OLD.rowid, OLD.id, OLD.title, OLD.content);
                INSERT INTO memory_fts(rowid, id, title, content)
                VALUES (NEW.rowid, NEW.id, NEW.title, NEW.content);
            END;

            INSERT OR REPLACE INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');
            "#,
        )
        .unwrap();
    }

    #[test]
    fn test_migrate_v1_to_current() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);

        // Verify starting at v1
        assert_eq!(get_schema_version(&conn).unwrap(), Some(1));

        // v1 should NOT have status column on documents
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_status, "v1 should not have status column");

        // Run migration
        migrate_schema(&conn, 1).unwrap();

        // Should be at current version
        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        // v2 migration: documents should now have status, status_reason, version
        let has_status: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('documents') WHERE name = 'status'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_status, "should have status column after migration");

        // v3 migration: memory_entries should have source_path
        let has_source_path: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'source_path'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_source_path,
            "should have source_path column after migration"
        );

        // v4 migration: collections should have source
        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_source, "should have source column after migration");
    }

    #[test]
    fn test_migrate_v1_preserves_existing_data() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);

        // Insert data at v1
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?, ?, ?, ?, ?)",
            ["docs", "./docs", "**/*.md", "1000", "1000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?, ?, ?)",
            ["h1", "body text", "1000"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, file_modified_at, indexed_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            ["docs", "readme.md", "h1", "Readme", "1000", "1000"],
        ).unwrap();
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            ["test-entry", "Test", "Content", "topic", "[\"tag1\"]", "1000", "1000"],
        ).unwrap();

        // Migrate
        migrate_schema(&conn, 1).unwrap();

        // Data should survive
        let title: String = conn
            .query_row(
                "SELECT title FROM documents WHERE relative_path = 'readme.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(title, "Readme");

        let mem_title: String = conn
            .query_row(
                "SELECT title FROM memory_entries WHERE id = 'test-entry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mem_title, "Test");

        // New columns should have defaults
        let status: String = conn
            .query_row(
                "SELECT status FROM documents WHERE relative_path = 'readme.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "current");

        let source: String = conn
            .query_row(
                "SELECT source FROM collections WHERE name = 'docs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source, "manual");
    }

    #[test]
    fn test_migrate_v3_to_current() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);
        // Manually apply v2 and v3 migrations to get to v3
        conn.execute_batch(
            r#"
            ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
            ALTER TABLE documents ADD COLUMN status_reason TEXT;
            ALTER TABLE documents ADD COLUMN version TEXT;
            ALTER TABLE memory_entries ADD COLUMN source_path TEXT;
            UPDATE schema_version SET version = 3;
            "#,
        )
        .unwrap();

        // v3 should NOT have source on collections
        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_source, "v3 should not have source column");

        // Migrate from v3
        migrate_schema(&conn, 3).unwrap();

        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        let has_source: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('collections') WHERE name = 'source'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_source,
            "should have source column after v3→v4 migration"
        );
    }

    #[test]
    fn test_malformed_tags_json_returns_empty_vec() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);
        migrate_schema(&conn, 1).unwrap();

        // Insert a memory entry with malformed JSON in tags
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            ["bad-tags", "Bad Tags", "Content", "topic", "not valid json{{{", "1000", "1000"],
        ).unwrap();

        // Read it back via the memory module
        let entry = crate::store::memory::get_entry(&conn, "bad-tags")
            .expect("get_entry should not error")
            .expect("entry should exist");

        assert!(
            entry.tags.is_empty(),
            "malformed tags_json should result in empty tags vec, got: {:?}",
            entry.tags
        );
    }

    #[test]
    fn test_migrate_v8_to_v9_adds_expires_at() {
        let conn = Connection::open_in_memory().unwrap();
        // Create fresh schema at v8 by init + roll back version
        init_schema(&conn).unwrap();

        // Verify expires_at does NOT exist yet at v8
        // (It will exist because init_schema creates latest DDL. So we simulate
        // a v8 database by dropping the column — but SQLite doesn't support DROP COLUMN
        // before 3.35. Instead, we test the migration path directly.)

        // Create a v8-like database from v1 + migrations up to v7
        let conn2 = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn2);
        // Apply migrations up through v7 manually
        conn2
            .execute_batch(
                r#"
                ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
                ALTER TABLE documents ADD COLUMN status_reason TEXT;
                ALTER TABLE documents ADD COLUMN version TEXT;
                ALTER TABLE memory_entries ADD COLUMN source_path TEXT;
                ALTER TABLE collections ADD COLUMN source TEXT DEFAULT 'manual';
                CREATE TABLE IF NOT EXISTS document_evolution (
                    id INTEGER PRIMARY KEY,
                    document_path TEXT NOT NULL,
                    from_version TEXT,
                    to_version TEXT NOT NULL,
                    change_type TEXT NOT NULL,
                    diff TEXT,
                    created_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_evolution_path ON document_evolution(document_path);
                ALTER TABLE memory_entries ADD COLUMN confirmations INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN corrections INTEGER DEFAULT 0;
                ALTER TABLE memory_entries ADD COLUMN last_confirmed_at INTEGER;
                ALTER TABLE memory_entries ADD COLUMN source_type TEXT DEFAULT 'user_statement';
                CREATE TABLE IF NOT EXISTS memory_revisions (
                    id INTEGER PRIMARY KEY,
                    memory_id TEXT NOT NULL,
                    diff TEXT NOT NULL,
                    created_at INTEGER NOT NULL,
                    FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id
                    ON memory_revisions(memory_id);
                UPDATE schema_version SET version = 8;
                "#,
            )
            .unwrap();

        assert_eq!(get_schema_version(&conn2).unwrap(), Some(8));

        // Verify expires_at does NOT exist at v8
        let has_expires_at: bool = conn2
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'expires_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_expires_at, "v8 should not have expires_at column");

        // Run migration
        migrate_schema(&conn2, 8).unwrap();

        assert_eq!(get_schema_version(&conn2).unwrap(), Some(SCHEMA_VERSION));

        // Verify expires_at exists after migration
        let has_expires_at: bool = conn2
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'expires_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_expires_at,
            "should have expires_at column after v8→v9 migration"
        );

        // Verify existing entries have NULL expires_at
        conn2
            .execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
                 VALUES ('test', 'Test', 'Content', 'topic', '[]', 1000, 1000)",
                [],
            )
            .unwrap();

        let expires_at: Option<i64> = conn2
            .query_row(
                "SELECT expires_at FROM memory_entries WHERE id = 'test'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            expires_at.is_none(),
            "existing entries should have NULL expires_at"
        );
    }

    #[test]
    fn test_migrate_v9_to_v10_adds_due_at() {
        let conn = Connection::open_in_memory().unwrap();
        create_v1_schema(&conn);
        // Apply migrations up through v9 manually to simulate a v9 database.
        conn.execute_batch(
            r#"
            ALTER TABLE documents ADD COLUMN status TEXT DEFAULT 'current';
            ALTER TABLE documents ADD COLUMN status_reason TEXT;
            ALTER TABLE documents ADD COLUMN version TEXT;
            ALTER TABLE memory_entries ADD COLUMN source_path TEXT;
            ALTER TABLE collections ADD COLUMN source TEXT DEFAULT 'manual';
            ALTER TABLE memory_entries ADD COLUMN confirmations INTEGER DEFAULT 0;
            ALTER TABLE memory_entries ADD COLUMN corrections INTEGER DEFAULT 0;
            ALTER TABLE memory_entries ADD COLUMN last_confirmed_at INTEGER;
            ALTER TABLE memory_entries ADD COLUMN source_type TEXT DEFAULT 'user_statement';
            ALTER TABLE memory_entries ADD COLUMN expires_at INTEGER;
            CREATE TABLE IF NOT EXISTS memory_revisions (
                id INTEGER PRIMARY KEY,
                memory_id TEXT NOT NULL,
                diff TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                FOREIGN KEY (memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_memory_revisions_memory_id
                ON memory_revisions(memory_id);
            UPDATE schema_version SET version = 9;
            "#,
        )
        .unwrap();

        assert_eq!(get_schema_version(&conn).unwrap(), Some(9));

        // Insert an entry at v9 to verify data preservation.
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('pre-migration', 'Pre', 'Content', 'topic', '[]', 1000, 1000)",
            [],
        )
        .unwrap();

        // Verify due_at does NOT exist at v9.
        let has_due_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'due_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_due_at, "v9 should not have due_at column");

        // Run migration v9 → current.
        migrate_schema(&conn, 9).unwrap();

        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        // Verify due_at exists after migration.
        let has_due_at: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = 'due_at'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_due_at,
            "should have due_at column after v9→v10 migration"
        );

        // Pre-existing entry should survive with NULL due_at.
        let due_at: Option<i64> = conn
            .query_row(
                "SELECT due_at FROM memory_entries WHERE id = 'pre-migration'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            due_at.is_none(),
            "existing entries should have NULL due_at after migration"
        );
    }

    #[test]
    fn test_migrate_v10_to_v11_creates_edges() {
        // Simulate a v10 database: fully initialised, then drop edges and roll the
        // recorded version back to 10. Re-running init_schema must recreate the
        // edges table (via SCHEMA_SQL IF NOT EXISTS) and bump the version to 11.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).expect("initial init_schema failed");

        // Insert a document so we can prove pre-existing data survives the upgrade.
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at)
             VALUES ('docs', './docs', '**/*.md', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('h1', '# A', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at)
             VALUES ('docs', 'a.md', 'h1', 1, 1)",
            [],
        )
        .unwrap();

        conn.execute_batch("DROP TABLE edges; UPDATE schema_version SET version = 10;")
            .unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), Some(10));

        let has_edges: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='edges'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_edges, "simulated v10 db should not have edges table");

        // Real upgrade path: init_schema runs SCHEMA_SQL (recreates edges), then migrates.
        init_schema(&conn).expect("v10→v11 init_schema failed");

        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));
        let has_edges: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='edges'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_edges, "edges table should exist after v10→v11 upgrade");

        // Pre-existing document survives the upgrade.
        let doc_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))
            .unwrap();
        assert_eq!(doc_count, 1, "documents must survive the v10→v11 upgrade");
    }

    #[test]
    fn test_migrate_v12_to_v13_creates_prior_tables() {
        // Simulate a v12 database: initialise, drop the prior-mining tables, and
        // roll the recorded version back to 12. Re-running init_schema must
        // recreate them (via SCHEMA_SQL IF NOT EXISTS) and bump to 13.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).expect("initial init_schema failed");

        conn.execute_batch(
            "DROP TABLE prior_candidates; DROP TABLE prior_clusters;
             UPDATE schema_version SET version = 12;",
        )
        .unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), Some(12));

        init_schema(&conn).expect("v12→v13 init_schema failed");
        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        for table in ["prior_clusters", "prior_candidates"] {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(exists, "{table} must exist after v12→v13 upgrade");
        }
    }

    #[test]
    fn test_migrate_v11_to_v12_purges_legacy_priors() {
        // A v11 database polluted with legacy "System B" priors must have exactly
        // those rows removed on upgrade — while System-A priors, hand-authored
        // slug-id priors, and every non-prior entry survive untouched.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).expect("initial init_schema failed");

        let insert = |id: &str, entry_type: &str| {
            conn.execute(
                "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
                 VALUES (?1, 'title', 'content', ?2, 1, 1)",
                rusqlite::params![id, entry_type],
            )
            .unwrap();
        };

        // Legacy System-B garbage — must be purged (ids: prior-<16 hex>).
        insert("prior-2720158528794be7", "prior");
        insert("prior-0db3f4bb4dca1250", "prior");
        insert("prior-abcdef0123456789", "prior");
        // Survivors: System-A, hand-authored slug prior, and non-prior entries.
        insert("prior-proof-fix-fix", "prior");
        insert("canvas-terminal-dont-reinvent-xterm", "prior");
        insert("prior-read-mdkb-hook-context-first", "prior"); // slug, not 16-hex
        insert("prior-abcdef0123456789ff", "prior"); // 18 hex — too long, not System-B
        insert("some-decision", "decision");

        // Roll the recorded version back to 11 to exercise the real upgrade path.
        conn.execute_batch("UPDATE schema_version SET version = 11;")
            .unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), Some(11));

        init_schema(&conn).expect("v11→v12 init_schema failed");
        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        let surviving: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT id FROM memory_entries ORDER BY id")
                .unwrap();

            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };

        assert_eq!(
            surviving,
            vec![
                "canvas-terminal-dont-reinvent-xterm".to_string(),
                "prior-abcdef0123456789ff".to_string(),
                "prior-proof-fix-fix".to_string(),
                "prior-read-mdkb-hook-context-first".to_string(),
                "some-decision".to_string(),
            ],
            "only the three legacy prior-<16 hex> rows should be purged"
        );
    }

    // ==================== Schema v14: memory_edges + provenance ====================

    #[test]
    fn test_fresh_db_reports_v14_with_memory_edges() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        let has_edges: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_edges'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_edges, "fresh DB should have memory_edges table");
    }

    #[test]
    fn test_fresh_db_v15_has_prior_cluster_embedding_column() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");
        let has_embedding: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('prior_clusters') WHERE name = 'embedding'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(
            has_embedding,
            "fresh v15 DB should have prior_clusters.embedding for semantic merge"
        );
    }

    #[test]
    fn test_migrate_v14_to_v15_adds_prior_cluster_embedding() {
        // A v14 database already has prior_clusters (created by SCHEMA_SQL) but
        // without the embedding column; init_schema must ALTER it in.
        let conn = setup_db();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        conn.execute("ALTER TABLE prior_clusters DROP COLUMN embedding", [])
            .unwrap();
        conn.execute_batch("INSERT INTO schema_version (version) VALUES (14);")
            .unwrap();

        init_schema(&conn).expect("v14→v15 migration failed");

        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));
        let has_embedding: bool = conn
            .query_row(
                "SELECT 1 FROM pragma_table_info('prior_clusters') WHERE name = 'embedding'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_embedding, "migration must add the embedding column");
    }

    #[test]
    fn test_memory_edges_indexes_exist() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        let indexes: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='index' AND name LIKE 'idx_memedges_%'",
            )
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert!(
            indexes.iter().any(|i| i.contains("source")),
            "should have idx_memedges_source index"
        );
        assert!(
            indexes.iter().any(|i| i.contains("target")),
            "should have idx_memedges_target index"
        );
    }

    #[test]
    fn test_memory_entries_has_provenance_columns() {
        let conn = setup_db();
        init_schema(&conn).expect("init_schema failed");

        for col in ["created_session", "created_agent"] {
            let has: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = ?1",
                    [col],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(has, "memory_entries should have {col} column");
        }
    }

    #[test]
    fn test_memory_edges_fk_and_columns() {
        // A memory_edge referencing a non-existent memory must fail the FK, proving
        // the FK is wired; insert a memory entry first, then a valid edge.
        let conn = setup_db();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).expect("init_schema failed");

        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
             VALUES ('src-mem', 'Src', 'Content', 'topic', 1, 1)",
            [],
        )
        .unwrap();

        // FK violation: source_id points at a memory that does not exist.
        let bad = conn.execute(
            "INSERT INTO memory_edges (source_id, target_ref, target_kind, relation, created_at)
             VALUES ('missing', 'src-mem', 'memory', 'supports', 1)",
            [],
        );
        assert!(
            bad.is_err(),
            "FK on source_id should reject dangling source"
        );

        // Valid edge: dangling target_ref is allowed (resolved at query time).
        conn.execute(
            "INSERT INTO memory_edges (source_id, target_ref, target_kind, relation, created_at)
             VALUES ('src-mem', 'not-yet-indexed', 'memory', 'relates_to', 1)",
            [],
        )
        .expect("valid edge with dangling target should insert");

        let (target_ref, kind, relation): (String, String, String) = conn
            .query_row(
                "SELECT target_ref, target_kind, relation FROM memory_edges WHERE source_id = 'src-mem'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(target_ref, "not-yet-indexed");
        assert_eq!(kind, "memory");
        assert_eq!(relation, "relates_to");
    }

    #[test]
    fn test_migrate_v13_to_v14_adds_edges_and_provenance() {
        // Simulate a v13 database: initialise fully, drop memory_edges, remove the
        // provenance columns is impossible pre-3.35 DROP COLUMN, so instead we
        // rebuild memory_entries without them and roll the version back to 13.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).expect("initial init_schema failed");

        // Pre-existing entry must survive the upgrade with NULL provenance.
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, created_at, updated_at)
             VALUES ('pre-v14', 'Pre', 'Content', 'topic', '[]', 1000, 1000)",
            [],
        )
        .unwrap();

        // Roll back to a v13 shape: drop the new table + record version 13. The
        // provenance columns are added by an ALTER guarded on pragma_table_info,
        // so leaving them present would make the migration a no-op for columns —
        // to exercise the ALTER path we must remove them. SQLite ≥3.35 supports
        // DROP COLUMN; use it to get a genuine v13 memory_entries shape.
        conn.execute_batch(
            "DROP TABLE memory_edges;
             ALTER TABLE memory_entries DROP COLUMN created_session;
             ALTER TABLE memory_entries DROP COLUMN created_agent;
             UPDATE schema_version SET version = 13;",
        )
        .unwrap();
        assert_eq!(get_schema_version(&conn).unwrap(), Some(13));

        let has_edges_before: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_edges'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(!has_edges_before, "v13 db should not have memory_edges");

        // Real upgrade path: init_schema runs SCHEMA_SQL (recreates edges) then migrates.
        init_schema(&conn).expect("v13→v14 init_schema failed");
        assert_eq!(get_schema_version(&conn).unwrap(), Some(SCHEMA_VERSION));

        let has_edges: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_edges'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        assert!(has_edges, "memory_edges should exist after v13→v14 upgrade");

        for col in ["created_session", "created_agent"] {
            let has: bool = conn
                .query_row(
                    "SELECT 1 FROM pragma_table_info('memory_entries') WHERE name = ?1",
                    [col],
                    |_| Ok(true),
                )
                .unwrap_or(false);
            assert!(has, "{col} should exist after v13→v14 migration");
        }

        // Pre-existing row survives with NULL provenance.
        let (title, sess): (String, Option<String>) = conn
            .query_row(
                "SELECT title, created_session FROM memory_entries WHERE id = 'pre-v14'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(title, "Pre");
        assert!(
            sess.is_none(),
            "migrated entry should have NULL created_session"
        );
    }
}
