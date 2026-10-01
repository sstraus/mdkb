-- Recorded from this repository's history, not written by hand: the statements
-- `init_schema` executed at 296b415 (SCHEMA_VERSION = 33): SCHEMA_SQL, DOCUMENT_ALIASES_SQL, RELATION_CANDIDATES_SQL, RECALL_LEDGER_SQL, BM25_WEIGHTS_SQL.

-- SCHEMA_SQL

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
    last_refuted_at INTEGER,          -- Timestamp of last refutation; never moves last_confirmed_at
    last_audited_at INTEGER,          -- Timestamp of last `memory audit` pass; a look, not a verdict: never a decay reference
    source_type TEXT DEFAULT 'user_statement',  -- official_docs, user_statement, inference
    expires_at INTEGER,                        -- Unix timestamp; NULL = permanent
    due_at INTEGER,                            -- Unix timestamp; surfaces reminders at/after this time
    created_session TEXT,                      -- session id that authored this entry (provenance)
    created_agent TEXT,                        -- agent/tool that authored this entry (provenance)
    projected_at INTEGER,                      -- when the markdown projection was last written; NULL = never
    projected_hash TEXT,                       -- SHA-256 of the bytes last projected; NULL = predates the column
    triggers TEXT NOT NULL DEFAULT '[]'        -- JSON array of durable trigger matchers
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

-- `id TEXT PRIMARY KEY` is not `NOT NULL`: SQLite allows NULL in a PRIMARY KEY
-- column for backward compatibility, and TEXT affinity does not convert a BLOB.
-- One NULL-id row in the field broke `memory export` with InvalidColumnType,
-- because every reader binds the column to String. Adding NOT NULL needs a table
-- rebuild, which renumbers rowid and would detach memory_embeddings, vec_memory
-- and memory_fts from their entries — so the invariant is a trigger instead.
-- `typeof(id) = 'text'` is the exact condition those readers depend on.
CREATE TRIGGER IF NOT EXISTS memory_entries_id_guard_bi
BEFORE INSERT ON memory_entries
WHEN typeof(NEW.id) <> 'text'
     OR TRIM(NEW.id, ' ' || char(9) || char(10) || char(13)) = ''
BEGIN
    SELECT RAISE(ABORT, 'memory_entries.id must be non-blank text');
END;

CREATE TRIGGER IF NOT EXISTS memory_entries_id_guard_bu
BEFORE UPDATE OF id ON memory_entries
WHEN typeof(NEW.id) <> 'text'
     OR TRIM(NEW.id, ' ' || char(9) || char(10) || char(13)) = ''
BEGIN
    SELECT RAISE(ABORT, 'memory_entries.id must be non-blank text');
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
    misfired_count INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'candidate',  -- candidate|promoted|refuted|expired|archived
    promoted_memory_id TEXT,                 -- memory_entries.id once promoted
    created_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    embedding BLOB,                          -- lesson embedding (f32 LE) for semantic cluster-merge
    error_signature TEXT,                    -- the failure this lesson exists to prevent (recurrence = refutation)
    FOREIGN KEY(promoted_memory_id) REFERENCES memory_entries(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_prior_clusters_trigger ON prior_clusters(canonical_trigger_key);
CREATE INDEX IF NOT EXISTS idx_prior_clusters_state ON prior_clusters(state);

-- One row per (cluster, session) in which the prior was injected, settled at the
-- next Stop hook into `refuted`, `unrefuted` or `unobservable`, or explicitly
-- judged as `confirmed`, `refuted` or `misfired` by the model. The composite
-- primary key makes one verdict per cluster and session structural.
CREATE TABLE IF NOT EXISTS prior_injections (
    cluster_id TEXT NOT NULL,
    session TEXT NOT NULL,
    injected_at INTEGER NOT NULL,            -- first injection in this session
    outcome TEXT,                            -- NULL until settled; see outcomes above
    settled_at INTEGER,
    PRIMARY KEY (cluster_id, session),
    FOREIGN KEY(cluster_id) REFERENCES prior_clusters(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_prior_injections_open ON prior_injections(session, outcome);

-- A durable entry is surfaced once per session, independent of prior verdicts.
CREATE TABLE IF NOT EXISTS memory_trigger_injections (
    memory_id TEXT NOT NULL,
    session TEXT NOT NULL,
    injected_at INTEGER NOT NULL,
    PRIMARY KEY (memory_id, session),
    FOREIGN KEY(memory_id) REFERENCES memory_entries(id) ON DELETE CASCADE
);

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


-- DOCUMENT_ALIASES_SQL

-- Identities a document DECLARES for itself: `id:` and each `aliases:` entry.
-- An edge target is stored verbatim, so a reference like `person:alice` only
-- resolves if some document claims that name. Not a column on `documents`
-- because `aliases:` is a list and one document may claim several names.
--
-- No global UNIQUE on `alias` on purpose: two documents claiming one identity
-- is a repository defect to report, and refusing the insert would turn it into
-- a failed index.
CREATE TABLE IF NOT EXISTS document_aliases (
    doc_id INTEGER NOT NULL,
    alias  TEXT    NOT NULL,
    source_key TEXT NOT NULL,            -- which frontmatter key declared it
    FOREIGN KEY(doc_id) REFERENCES documents(id) ON DELETE CASCADE,
    UNIQUE(doc_id, alias)
);

CREATE INDEX IF NOT EXISTS idx_document_aliases_alias ON document_aliases(alias);


-- RELATION_CANDIDATES_SQL

-- What the relation-key detector last measured, so a latency-bounded reader
-- (SessionStart, 200 ms) can report it without scanning the corpus.
--
-- `extracted` records whether the key was in the effective extraction set on
-- the run that wrote this row: a key already producing edges must not be
-- reported as one the user is missing.
CREATE TABLE IF NOT EXISTS relation_candidates (
    key        TEXT PRIMARY KEY,
    hits       INTEGER NOT NULL,
    total      INTEGER NOT NULL,
    extracted  INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);


-- RECALL_LEDGER_SQL

CREATE TABLE IF NOT EXISTS recall_prompts (
    id              INTEGER PRIMARY KEY,
    session         TEXT NOT NULL,
    mode            TEXT NOT NULL,     -- sigil | automatic | shadow
    floor           REAL NOT NULL,     -- injection floor
    candidate_floor REAL NOT NULL,     -- recording floor
    created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_recall_prompts_session ON recall_prompts(session);
CREATE INDEX IF NOT EXISTS idx_recall_prompts_created ON recall_prompts(created_at);
CREATE TABLE IF NOT EXISTS recall_candidates (
    prompt_id  INTEGER NOT NULL REFERENCES recall_prompts(id) ON DELETE CASCADE,
    entry_id   TEXT NOT NULL,
    rank       INTEGER NOT NULL,
    cosine     REAL,                   -- NULL for an FTS-only hit
    entry_type TEXT NOT NULL,
    age_days   INTEGER NOT NULL,
    overlap    INTEGER NOT NULL,
    injected   INTEGER NOT NULL,
    holdout    INTEGER NOT NULL,
    outcome    TEXT,                   -- NULL until settled with a strong signal
    outcome_at INTEGER,
    PRIMARY KEY (prompt_id, entry_id)
);
CREATE INDEX IF NOT EXISTS idx_recall_candidates_entry ON recall_candidates(entry_id);


-- BM25_WEIGHTS_SQL

INSERT OR REPLACE INTO documents_fts(documents_fts, rank) VALUES('rank', 'bm25(10.0, 1.0)');

