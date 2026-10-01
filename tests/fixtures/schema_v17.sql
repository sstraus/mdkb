-- Recorded from this repository's history, not written by hand: SCHEMA_SQL and
-- BM25_WEIGHTS_SQL of src/store/schema.rs at 44cc461^ (SCHEMA_VERSION = 17),
-- the statements `init_schema` executed to build a v17 store.

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
    projected_at INTEGER                       -- when the markdown projection was last written; NULL = never
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

CREATE TRIGGER IF NOT EXISTS memory_au AFTER UPDATE ON memory_entries BEGIN
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


INSERT OR REPLACE INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');

