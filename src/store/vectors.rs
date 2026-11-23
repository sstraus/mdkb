//! Vector storage for semantic search.
//!
//! Uses sqlite-vec for vector similarity search.

use std::sync::Once;

use rusqlite::{Connection, ffi::sqlite3_auto_extension, params};
use sqlite_vec::sqlite3_vec_init;
use zerocopy::AsBytes;

use crate::error::Result;
use crate::store::{chunks, documents};

use crate::llm::embeddings::Model;

/// Desired identity is resolved per connection, never from a process-wide switch.
pub fn selected_model(conn: &Connection) -> Result<Model> {
    Ok(store_config(conn)?.map_or(Model::MiniLm, |config| config.embedding.model))
}

/// The configuration beside this connection's store; `None` for an in-memory store.
fn store_config(conn: &Connection) -> Result<Option<crate::config::Config>> {
    let filename: String = conn.query_row("PRAGMA database_list", [], |r| r.get(2))?;
    if filename.is_empty() {
        return Ok(None);
    }
    let path = std::path::Path::new(&filename)
        .parent()
        .unwrap()
        .join("config.toml");
    // Same tolerance as `Config::load_for_repo`: only embedding.* and removed rerank
    // controls are refused; other config errors use defaults, so an unknown encoder
    // never falls back to MiniLM but an unrelated typo cannot make the store unopenable.
    crate::config::Config::load_for_repo(&path).map(Some)
}

/// Whether document embedding covers session transcripts (`[search] auto_embed_sessions`).
/// Automatic embedding and full regeneration share this scope.
pub fn embeds_sessions(conn: &Connection) -> Result<bool> {
    Ok(store_config(conn)?.is_some_and(|config| config.search.auto_embed_sessions))
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IdentityStatus {
    pub selected_model: String,
    pub current_model: Option<String>,
    pub pending_model: Option<String>,
    pub stale: bool,
    pub ambiguous: bool,
}

/// Inspect durable metadata and every legacy row label before accepting identity.
pub fn identity_status(conn: &Connection) -> Result<IdentityStatus> {
    let key = std::ptr::from_ref(conn) as usize;
    let in_scope = !conn.is_autocommit()
        && SNAPSHOT_IDENTITY.with(|s| s.borrow().as_ref().is_some_and(|c| c.conn == key));
    if !in_scope {
        return identity_status_inner(conn, true);
    }
    // Own writes inside the transaction change what the snapshot sees.
    let changes: i64 = conn.query_row("SELECT total_changes()", [], |r| r.get(0))?;
    let cached = SNAPSHOT_IDENTITY.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|c| c.status.as_ref().filter(|_| c.changes == changes).cloned())
    });
    if let Some(status) = cached {
        return Ok(status);
    }
    let status = identity_status_inner(conn, true)?;
    SNAPSHOT_IDENTITY.with(|s| {
        if let Some(c) = s.borrow_mut().as_mut() {
            c.changes = changes;
            c.status = Some(status.clone());
        }
    });
    Ok(status)
}

/// Identity probed inside the read transaction opened by [`with_read_snapshot`].
struct SnapshotIdentity {
    conn: usize,
    changes: i64,
    status: Option<IdentityStatus>,
}

thread_local! {
    static SNAPSHOT_IDENTITY: std::cell::RefCell<Option<SnapshotIdentity>> =
        const { std::cell::RefCell::new(None) };
}

/// Run read work in one SQLite read transaction, so every semantic read in it
/// shares one identity probe. A WAL read transaction sees one fixed snapshot, so
/// the probe result stays exact for all of them. A recall turn makes several
/// semantic reads, and each probe walks every vector key (story 065-9a07).
/// Do not write through `conn` inside `body`: a read snapshot that later writes
/// fails with `SQLITE_BUSY_SNAPSHOT` when another connection committed first.
pub fn with_read_snapshot<T>(conn: &Connection, body: impl FnOnce() -> Result<T>) -> Result<T> {
    if !conn.is_autocommit() {
        return body();
    }
    struct Restore(Option<SnapshotIdentity>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SNAPSHOT_IDENTITY.with(|s| *s.borrow_mut() = self.0.take());
        }
    }
    conn.execute_batch("BEGIN")?;
    let _restore = Restore(SNAPSHOT_IDENTITY.with(|s| {
        s.borrow_mut().replace(SnapshotIdentity {
            conn: std::ptr::from_ref(conn) as usize,
            changes: -1,
            status: None,
        })
    }));
    let result = body();
    if conn.is_autocommit() {
        return result;
    }
    match result {
        Ok(value) => conn
            .execute_batch("COMMIT")
            .map(|()| value)
            .map_err(Into::into),
        Err(error) => {
            if let Err(unwind) = conn.execute_batch("ROLLBACK") {
                tracing::error!(unwind_error = %unwind, original_error = %error, "ROLLBACK failed after read snapshot error");
            }
            Err(error)
        }
    }
}

fn identity_status_inner(conn: &Connection, inspect_orphans: bool) -> Result<IdentityStatus> {
    use rusqlite::OptionalExtension;
    let selected = selected_model(conn)?.identity().to_string();
    let metadata_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='embedding_identity')",
        [],
        |r| r.get(0),
    )?;
    let (mut current, pending): (Option<String>, Option<String>) = if metadata_exists {
        conn.query_row(
            "SELECT current_model, pending_model FROM embedding_identity WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or_default()
    } else {
        (None, None)
    };
    // Two index-boundary probes per corpus detect mixed labels without scanning
    // every existing row on every batch write.
    let mut models = std::collections::BTreeSet::new();
    for table in ["embeddings", "memory_embeddings"] {
        for order in ["ASC", "DESC"] {
            let sql = format!("SELECT model FROM {table} ORDER BY model {order} LIMIT 1");
            if let Some(model) = conn
                .query_row(&sql, [], |r| r.get::<_, String>(0))
                .optional()?
            {
                models.insert(model);
            }
        }
    }
    let models: Vec<_> = models.into_iter().collect();
    let unknown = models.iter().any(|m| Model::from_identity(m).is_none());
    let mixed = models.len() > 1
        || current
            .as_ref()
            .is_some_and(|c| models.iter().any(|m| m != c));
    if current.is_none() && models.len() == 1 && !unknown {
        current = models.first().cloned();
    }
    // Chunk vectors have no row label. Only a labelled parent embedding proves ownership.
    // Every read runs this probe, several times per recall turn. Scanning the vec0
    // tables and joining `document_chunks` row by row read every vector and every
    // chunk body: recall search p50 was 242 ms on an 18.7k-chunk store against
    // 49 ms without the probe (story 065-9a07). The sqlite-vec `<table>_rowids`
    // shadow tables hold the vec0 primary key as rowid, and the chunk side is an
    // anti-join over the covering `idx_chunks_document` index, so no vector or
    // chunk body is read.
    let orphan: bool = (inspect_orphans || current.is_none()) && conn.query_row("SELECT EXISTS(SELECT 1 FROM vec_chunks_rowids v WHERE v.rowid NOT IN (SELECT c.id FROM document_chunks c INDEXED BY idx_chunks_document WHERE c.document_id IN (SELECT document_id FROM embeddings))) OR EXISTS(SELECT 1 FROM vec_documents_rowids v LEFT JOIN embeddings e ON e.document_id=v.rowid WHERE e.document_id IS NULL) OR EXISTS(SELECT 1 FROM vec_memory_rowids v LEFT JOIN memory_embeddings e ON e.memory_rowid=v.rowid WHERE e.memory_rowid IS NULL)", [], |r| r.get(0))?;
    let invalid_schema: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name IN ('vec_documents','vec_chunks','vec_memory') AND replace(upper(sql),' ','') NOT LIKE '%FLOAT[384]%')", [], |r| r.get(0))?;
    let stale = unknown
        || mixed
        || orphan
        || invalid_schema
        || pending.is_some()
        || current.as_ref().is_some_and(|m| m != &selected);
    Ok(IdentityStatus {
        selected_model: selected,
        current_model: current,
        pending_model: pending,
        stale,
        ambiguous: unknown || mixed || orphan || invalid_schema,
    })
}

/// Automatic migration is a daemon capability, not a liveness check.
/// Keep diagnostics conditional: eligible processes may not have opened the store yet.
pub(crate) fn automatic_reembedding_available() -> bool {
    cfg!(unix) && crate::core::routing::daemon_serves_this_process()
}

/// Ambiguous stores are rebuilt by the daemon like stale ones, so the guidance
/// is the same: the explicit command only where no daemon can own the rebuild.
pub(crate) fn recovery_guidance() -> &'static str {
    if automatic_reembedding_available() {
        "automatic re-embedding runs when the daemon opens this store; use lexical search meanwhile"
    } else {
        "run `mdkb update --reembed`"
    }
}

/// Hybrid consumers retain lexical retrieval when semantic identity is unavailable.
/// Call inside the same read snapshot as the subsequent vector query.
pub fn model_is_current(conn: &Connection, model: Model) -> Result<bool> {
    let status = identity_status(conn)?;
    let current = !status.stale && status.selected_model == model.identity();
    if !current {
        if status.ambiguous {
            tracing::warn!(
                "Semantic retrieval unavailable: ambiguous embedding identity; {}; using lexical retrieval",
                recovery_guidance()
            );
        } else {
            tracing::debug!(
                "Semantic retrieval unavailable: embedding identity is stale or incompatible; {}; using lexical retrieval",
                recovery_guidance()
            );
        }
    }
    Ok(current)
}

/// A wrong model is an error even when both encoders produce 384 floats.
pub fn require_model(conn: &Connection, model: &str, write: bool) -> Result<()> {
    if write {
        // Reserve the SQLite writer before reading identity in the caller's
        // savepoint. A read-first WAL snapshot cannot upgrade after another
        // writer commits (SQLITE_BUSY_SNAPSHOT), regardless of busy_timeout.
        conn.execute("UPDATE embedding_identity SET id = id WHERE id = 1", [])?;
    }
    let status = if write {
        identity_status_inner(conn, false)?
    } else {
        identity_status(conn)?
    };
    let rebuilding = write
        && !status.ambiguous
        && status.pending_model.as_deref() == Some(model)
        && status.current_model.as_deref().is_none_or(|m| m == model);
    if status.selected_model != model || (status.stale && !rebuilding) {
        return Err(crate::error::Error::other(format!(
            "embedding identity mismatch (selected {}, current {:?}, pending {:?}, supplied {model}); {}",
            status.selected_model,
            status.current_model,
            status.pending_model,
            recovery_guidance()
        )));
    }
    if write {
        conn.execute("INSERT INTO embedding_identity(id,current_model) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET current_model=excluded.current_model", [model])?;
    }
    Ok(())
}

/// Explicit destructive vector reset; source documents and memories are retained.
/// The pending marker survives failures so queries cannot serve a partial migration.
pub fn begin_reembed(conn: &Connection) -> Result<()> {
    let model = selected_model(conn)?;
    documents::with_savepoint(conn, "reembed", || {
        conn.execute_batch("DELETE FROM vec_chunks; DELETE FROM document_chunks; DELETE FROM embeddings; DELETE FROM memory_embeddings;
            DROP TABLE vec_chunks; DROP TABLE vec_documents; DROP TABLE vec_memory;
            CREATE VIRTUAL TABLE vec_documents USING vec0(document_id INTEGER PRIMARY KEY, embedding FLOAT[384]);
            CREATE VIRTUAL TABLE vec_chunks USING vec0(chunk_id INTEGER PRIMARY KEY, embedding FLOAT[384]);
            CREATE VIRTUAL TABLE vec_memory USING vec0(memory_rowid INTEGER PRIMARY KEY, embedding FLOAT[384]);")?;
        conn.execute("INSERT INTO embedding_identity(id,current_model,pending_model) VALUES(1,NULL,?1) ON CONFLICT(id) DO UPDATE SET current_model=NULL,pending_model=excluded.pending_model", [model.identity()])?;
        Ok(())
    })
}

pub fn finish_reembed(conn: &Connection) -> Result<()> {
    let model = selected_model(conn)?;
    let status = identity_status(conn)?;
    // Session transcripts outside the embedding scope stay without vectors, as
    // after automatic embedding; they are not part of the generation.
    let missing: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM documents d LEFT JOIN embeddings e ON e.document_id=d.id WHERE e.document_id IS NULL AND (?1 OR d.collection != ?2)) OR EXISTS(SELECT 1 FROM memory_entries m LEFT JOIN memory_embeddings e ON e.memory_rowid=m.rowid WHERE e.memory_rowid IS NULL)", params![embeds_sessions(conn)?, crate::domain::COLLECTION_CLAUDE_SESSIONS], |r| r.get(0))?;
    if missing || status.ambiguous || status.pending_model.as_deref() != Some(model.identity()) {
        return Err(crate::error::Error::other(
            "re-embedding is incomplete; rerun `mdkb update --reembed`",
        ));
    }
    conn.execute("UPDATE embedding_identity SET current_model=?1,pending_model=NULL WHERE id=1 AND pending_model=?1", [model.identity()])?;
    Ok(())
}

/// Ensures sqlite-vec extension is initialized exactly once.
static INIT_SQLITE_VEC: Once = Once::new();

/// Initialize sqlite-vec extension globally.
/// Safe to call multiple times - will only initialize once.
/// Must be called before opening any connections that use vectors.
pub fn init_sqlite_vec() {
    INIT_SQLITE_VEC.call_once(|| {
        // SAFETY: sqlite3_vec_init is an extern "C" function with the signature expected by
        // sqlite3_auto_extension: `fn(*mut sqlite3, **mut c_char, *const sqlite3_api_routines) -> c_int`.
        // The transmute converts the function pointer to the type expected by the SQLite C API.
        // This is the standard pattern for registering SQLite extensions via rusqlite's ffi.
        unsafe {
            sqlite3_auto_extension(Some(std::mem::transmute(sqlite3_vec_init as *const ())));
        }
    });
}

/// Initialize vector storage without changing or discarding legacy vectors.
pub fn init_vector_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS embedding_identity(id INTEGER PRIMARY KEY CHECK(id=1), current_model TEXT, pending_model TEXT);")?;
    // Create embeddings table
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS embeddings (
            document_id INTEGER PRIMARY KEY,
            embedding BLOB NOT NULL,
            model TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE
        )
        "#,
        [],
    )?;

    // Create virtual table for vector search (no-op if already exists)
    conn.execute(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS vec_documents USING vec0(
            document_id INTEGER PRIMARY KEY,
            embedding FLOAT[384]
        )
        "#,
        [],
    )?;

    // Document chunks for chunk-level embeddings
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS document_chunks (
            id INTEGER PRIMARY KEY,
            document_id INTEGER NOT NULL,
            chunk_index INTEGER NOT NULL,
            content TEXT NOT NULL,
            heading_path TEXT,
            FOREIGN KEY(document_id) REFERENCES documents(id) ON DELETE CASCADE,
            UNIQUE(document_id, chunk_index)
        )
        "#,
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_chunks_document ON document_chunks(document_id)",
        [],
    )?;

    // Vector index for chunk-level search
    conn.execute(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS vec_chunks USING vec0(
            chunk_id INTEGER PRIMARY KEY,
            embedding FLOAT[384]
        )
        "#,
        [],
    )?;

    // Memory embeddings storage
    conn.execute(
        r#"
        CREATE TABLE IF NOT EXISTS memory_embeddings (
            memory_rowid INTEGER PRIMARY KEY,
            embedding BLOB NOT NULL,
            model TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )
        "#,
        [],
    )?;

    conn.execute_batch("CREATE INDEX IF NOT EXISTS embeddings_model ON embeddings(model); CREATE INDEX IF NOT EXISTS memory_embeddings_model ON memory_embeddings(model);")?;

    // Vector index for memory entries
    conn.execute(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS vec_memory USING vec0(
            memory_rowid INTEGER PRIMARY KEY,
            embedding FLOAT[384]
        )
        "#,
        [],
    )?;

    // Cleanup triggers: delete orphan vector entries when parent rows are deleted.
    // FK CASCADE handles `embeddings` and `document_chunks`, but sqlite-vec
    // virtual tables don't support FK constraints, so we need explicit triggers.
    conn.execute_batch(
        r#"
        CREATE TRIGGER IF NOT EXISTS documents_delete_vec AFTER DELETE ON documents BEGIN
            DELETE FROM vec_documents WHERE document_id = OLD.id;
        END;

        CREATE TRIGGER IF NOT EXISTS chunks_delete_vec AFTER DELETE ON document_chunks BEGIN
            DELETE FROM vec_chunks WHERE chunk_id = OLD.id;
        END;

        CREATE TRIGGER IF NOT EXISTS memory_delete_embeddings AFTER DELETE ON memory_entries BEGIN
            DELETE FROM memory_embeddings WHERE memory_rowid = OLD.rowid;
            DELETE FROM vec_memory WHERE memory_rowid = OLD.rowid;
        END;
        "#,
    )?;

    let status = identity_status(conn)?;
    if !status.ambiguous && status.pending_model.is_none() {
        if let Some(current) = status.current_model {
            conn.execute(
                "INSERT OR IGNORE INTO embedding_identity(id,current_model) VALUES(1,?1)",
                [current],
            )?;
        }
    }
    Ok(())
}

/// Store embedding for a document.
///
/// Atomic: embeddings + vec_documents are kept in sync via transaction.
pub fn store_embedding(
    conn: &Connection,
    document_id: i64,
    embedding: &[f32],
    model: &str,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let embedding_bytes = embedding.as_bytes();

    documents::with_savepoint(conn, "store_embedding", || {
        require_model(conn, model, true)?;
        conn.execute(
            "INSERT OR REPLACE INTO embeddings (document_id, embedding, model, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![document_id, embedding_bytes, model, now],
        )?;

        // sqlite-vec virtual tables don't support INSERT OR REPLACE
        conn.execute(
            "DELETE FROM vec_documents WHERE document_id = ?1",
            params![document_id],
        )?;
        conn.execute(
            "INSERT INTO vec_documents (document_id, embedding) VALUES (?1, ?2)",
            params![document_id, embedding_bytes],
        )?;
        Ok(())
    })
}

/// Get embedding for a document.
pub fn get_embedding(conn: &Connection, document_id: i64) -> Result<Option<Vec<f32>>> {
    documents::with_savepoint(conn, "get_embedding_identity", || {
        require_model(conn, Model::MiniLm.identity(), false)?;
        use rusqlite::OptionalExtension;
        let result: Option<Vec<u8>> = conn
            .query_row(
                "SELECT embedding FROM embeddings WHERE document_id = ?1",
                params![document_id],
                |row| row.get(0),
            )
            .optional()?;

        if result
            .as_ref()
            .is_some_and(|bytes| bytes.len() != EMBEDDING_DIM * 4)
        {
            return Err(crate::error::Error::other(
                "invalid stored embedding length; run `mdkb update --reembed`",
            ));
        }
        Ok(result.map(|bytes| {
            bytes
                .chunks(4)
                .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
                .collect()
        }))
    })
}

/// Search for similar documents by vector, outside session transcripts.
pub fn vector_search(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    vector_search_for_model(conn, query_embedding, limit, None, Model::MiniLm)
}

/// Document-level KNN in `collection`, or outside session transcripts when it is
/// `None`, nearest first.
pub fn vector_search_for_model(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
    collection: Option<&str>,
    model: Model,
) -> Result<Vec<(i64, f32)>> {
    crate::store::documents::with_savepoint(conn, "vector_search_for_model", || {
        require_model(conn, model.identity(), false)?;
        scoped_knn(conn, limit, collection, |k| {
            Ok(search_vec_documents(conn, query_embedding, k)?
                .into_iter()
                .collect())
        })
    })
}

/// Run `knn(k)` and keep the candidates in scope, widening `k` by 4x steps until
/// the scope holds `limit` documents or `k` reaches the sqlite-vec ceiling.
/// The scope is applied after the KNN, so a pool drawn from out-of-scope vectors
/// comes back short: on a store whose chunks were 58% transcripts, only 24 of
/// 276 recall document candidates kept a cosine (story 065-9a07). Returns the
/// nearest `limit` candidates, nearest first.
fn scoped_knn(
    conn: &Connection,
    limit: usize,
    collection: Option<&str>,
    mut knn: impl FnMut(usize) -> Result<std::collections::HashMap<i64, f32>>,
) -> Result<Vec<(i64, f32)>> {
    let mut k = (limit * 5).clamp(1, VEC0_K_MAX);
    let candidates = loop {
        let mut candidates = knn(k)?;
        if !candidates.is_empty() {
            retain_documents_in_scope(conn, &mut candidates, collection);
        }
        if candidates.len() >= limit || k == VEC0_K_MAX {
            break candidates;
        }
        k = (k * 4).min(VEC0_K_MAX);
    };
    let mut results: Vec<(i64, f32)> = candidates.into_iter().collect();
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);
    Ok(results)
}

/// Document-level KNN without the identity check: the caller has already run
/// `require_model` in the same savepoint.
fn search_vec_documents(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    let embedding_bytes = query_embedding.as_bytes();

    let mut stmt = conn.prepare(
        r#"
        SELECT document_id, distance
        FROM vec_documents
        WHERE embedding MATCH ?1
        ORDER BY distance
        LIMIT ?2
        "#,
    )?;

    let results: std::result::Result<Vec<_>, _> = stmt
        .query_map(params![embedding_bytes, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f32>(1)?))
        })?
        .collect();

    Ok(results?)
}

/// Delete embedding for a document.
pub fn delete_embedding(conn: &Connection, document_id: i64) -> Result<bool> {
    let rows = conn.execute(
        "DELETE FROM embeddings WHERE document_id = ?1",
        params![document_id],
    )?;

    // Also delete from vector index
    conn.execute(
        "DELETE FROM vec_documents WHERE document_id = ?1",
        params![document_id],
    )?;

    Ok(rows > 0)
}

/// Check if document has an embedding.
pub fn has_embedding(conn: &Connection, document_id: i64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM embeddings WHERE document_id = ?1",
        params![document_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Every `document_id` that currently has a stored embedding, as a set — one
/// query instead of a [`has_embedding`] call per document. The auto-embed path
/// runs on every doc-watcher flush and previously issued one query per doc across
/// the whole corpus just to find the few missing an embedding (PERF-1).
pub fn embedded_document_ids(conn: &Connection) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = conn.prepare("SELECT document_id FROM embeddings")?;
    let ids = stmt
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<std::result::Result<std::collections::HashSet<i64>, _>>()?;
    Ok(ids)
}

/// Count total embeddings.
pub fn count_embeddings(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))?;
    Ok(count as usize)
}

// =============================================================================
// Document chunk embeddings
// =============================================================================

/// Store chunks and their embeddings for a document.
///
/// Replaces any existing chunks for the document.
pub fn store_chunk_embeddings(
    conn: &Connection,
    document_id: i64,
    chunks: &[chunks::Chunk],
    embeddings: &[Vec<f32>],
    model: &str,
) -> Result<()> {
    if chunks.len() != embeddings.len() {
        return Err(crate::error::ErrorKind::InvalidQuery(format!(
            "chunks/embeddings length mismatch: {} vs {}",
            chunks.len(),
            embeddings.len()
        ))
        .into());
    }
    let now = chrono::Utc::now().timestamp();

    // Atomic: delete old + insert new in one transaction
    conn.execute("BEGIN IMMEDIATE", [])?;
    let result = (|| -> Result<()> {
        require_model(conn, model, true)?;
        delete_chunk_embeddings(conn, document_id)?;

        for (chunk, embedding) in chunks.iter().zip(embeddings.iter()) {
            conn.execute(
                "INSERT INTO document_chunks (document_id, chunk_index, content, heading_path) VALUES (?1, ?2, ?3, ?4)",
                params![document_id, chunk.index as i64, chunk.content, chunk.heading_path],
            )?;
            let chunk_id = conn.last_insert_rowid();

            let embedding_bytes = embedding.as_bytes();
            conn.execute(
                "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
                params![chunk_id, embedding_bytes],
            )?;
        }

        // Mark doc as having embeddings (for has_embedding() check)
        if let Some(first_emb) = embeddings.first() {
            let embedding_bytes = first_emb.as_bytes();
            conn.execute(
                "INSERT OR REPLACE INTO embeddings (document_id, embedding, model, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![document_id, embedding_bytes, model, now],
            )?;
            conn.execute(
                "DELETE FROM vec_documents WHERE document_id = ?1",
                params![document_id],
            )?;
            conn.execute(
                "INSERT INTO vec_documents (document_id, embedding) VALUES (?1, ?2)",
                params![document_id, embedding_bytes],
            )?;
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            conn.execute("COMMIT", [])?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", []);
            Err(e)
        }
    }
}

/// Search for similar documents via chunk-level vector search.
///
/// Returns (document_id, best_chunk_distance) — aggregated by document,
/// taking the closest chunk per document.
pub fn chunk_vector_search(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
    collection: Option<&str>,
) -> Result<Vec<(i64, f32)>> {
    chunk_vector_search_for_model(conn, query_embedding, limit, collection, Model::MiniLm)
}

pub fn chunk_vector_search_for_model(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
    collection: Option<&str>,
    model: Model,
) -> Result<Vec<(i64, f32)>> {
    crate::store::documents::with_savepoint(conn, "chunk_vector_search_for_model", || {
        require_model(conn, model.identity(), false)?;
        let embedding_bytes = query_embedding.as_bytes();
        // The BM25 leg filters on the scope; this leg must agree.
        scoped_knn(conn, limit, collection, |k| {
            // Best distance per document over the chunk-level and the
            // document-level KNN (small documents have no chunks).
            let mut best_per_doc: std::collections::HashMap<i64, f32> =
                std::collections::HashMap::new();
            let chunk_hits = search_vec_chunks_by_doc(conn, embedding_bytes, k);
            for (doc_id, distance) in
                chunk_hits
                    .into_iter()
                    .chain(search_vec_documents(conn, query_embedding, k)?)
            {
                let entry = best_per_doc.entry(doc_id).or_insert(f32::MAX);
                if distance < *entry {
                    *entry = distance;
                }
            }
            Ok(best_per_doc)
        })
    })
}

/// Delete all chunks and their embeddings for a document.
///
/// The `chunks_delete_vec` trigger automatically cleans up `vec_chunks`
/// when `document_chunks` rows are deleted.
pub fn delete_chunk_embeddings(conn: &Connection, document_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM document_chunks WHERE document_id = ?1",
        params![document_id],
    )?;
    Ok(())
}

/// The sqlite-vec vec0 ceiling on `k` in a KNN query.
const VEC0_K_MAX: usize = 4096;

/// Keep the candidates in `collection`, or outside session transcripts when it is
/// `None`: the same scope as the BM25 leg.
///
/// One query for the whole candidate set: the ids come from a vector scan that
/// has no collection predicate of its own. A query failure leaves the
/// candidates untouched rather than silently emptying a scoped search.
fn retain_documents_in_scope(
    conn: &Connection,
    candidates: &mut std::collections::HashMap<i64, f32>,
    collection: Option<&str>,
) {
    let (op, collection) = match collection {
        Some(collection) => ("=", collection),
        None => ("!=", crate::domain::COLLECTION_CLAUDE_SESSIONS),
    };
    let placeholders: String = candidates
        .keys()
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql =
        format!("SELECT id FROM documents WHERE id IN ({placeholders}) AND collection {op} ?1");
    let kept: std::collections::HashSet<i64> = match conn.prepare(&sql).and_then(|mut stmt| {
        stmt.query_map(params![collection], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()
    }) {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            tracing::warn!("collection filter for the vector leg failed: {e}");
            return;
        }
    };
    candidates.retain(|id, _| kept.contains(id));
}

/// Search vec_chunks, returning (document_id, distance) aggregated by best chunk per doc.
fn search_vec_chunks_by_doc(
    conn: &Connection,
    embedding_bytes: &[u8],
    limit: usize,
) -> Vec<(i64, f32)> {
    // Phase 1: vector search for chunk IDs + distances
    let chunk_hits: Vec<(i64, f32)> = match conn.prepare(
        "SELECT chunk_id, distance FROM vec_chunks WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
    ) {
        Ok(mut stmt) => {
            let rows: std::result::Result<Vec<_>, _> = stmt
                .query_map(params![embedding_bytes, limit as i64], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, f32>(1)?))
                })
                .map(|rows| rows.collect())
                .unwrap_or_else(|e| {
                    tracing::warn!("vec_chunks query failed: {e}");
                    Ok(Vec::new())
                });
            match rows {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("vec_chunks row error: {e}");
                    Vec::new()
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to prepare vec_chunks query: {e}");
            return Vec::new();
        }
    };

    if chunk_hits.is_empty() {
        return Vec::new();
    }

    // Phase 2: batch resolve chunk_id → document_id in single query
    let placeholders: String = chunk_hits
        .iter()
        .map(|(id, _)| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id, document_id FROM document_chunks WHERE id IN ({placeholders})");
    let chunk_to_doc: std::collections::HashMap<i64, i64> = conn
        .prepare(&sql)
        .and_then(|mut stmt| {
            let rows: rusqlite::Result<Vec<(i64, i64)>> = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect();
            Ok(rows?.into_iter().collect())
        })
        .unwrap_or_else(|e| {
            tracing::warn!("chunk_id→document_id resolution failed: {e}");
            std::collections::HashMap::new()
        });

    // Aggregate: best distance per document
    let mut best_per_doc: std::collections::HashMap<i64, f32> = std::collections::HashMap::new();
    for (chunk_id, distance) in &chunk_hits {
        if let Some(&doc_id) = chunk_to_doc.get(chunk_id) {
            let entry = best_per_doc.entry(doc_id).or_insert(f32::MAX);
            if *distance < *entry {
                *entry = *distance;
            }
        }
    }

    best_per_doc.into_iter().collect()
}

/// Check if document has chunk-level embeddings.
pub fn has_chunk_embeddings(conn: &Connection, document_id: i64) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM document_chunks WHERE document_id = ?1",
        params![document_id],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

// =============================================================================
// Memory entry embeddings
// =============================================================================

/// Store embedding for a memory entry (keyed by rowid).
pub fn store_memory_embedding(
    conn: &Connection,
    memory_rowid: i64,
    embedding: &[f32],
    model: &str,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let embedding_bytes = embedding.as_bytes();

    documents::with_savepoint(conn, "store_memory_embedding", || {
        require_model(conn, model, true)?;
        conn.execute(
            "INSERT OR REPLACE INTO memory_embeddings (memory_rowid, embedding, model, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![memory_rowid, embedding_bytes, model, now],
        )?;

        // sqlite-vec doesn't support INSERT OR REPLACE
        conn.execute(
            "DELETE FROM vec_memory WHERE memory_rowid = ?1",
            params![memory_rowid],
        )?;
        conn.execute(
            "INSERT INTO vec_memory (memory_rowid, embedding) VALUES (?1, ?2)",
            params![memory_rowid, embedding_bytes],
        )?;
        Ok(())
    })
}

/// Search for similar memory entries by vector.
pub fn memory_vector_search(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
    entry_type: Option<&str>,
) -> Result<Vec<(i64, f32)>> {
    memory_vector_search_for_model(conn, query_embedding, limit, entry_type, Model::MiniLm)
}

pub fn memory_vector_search_for_model(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
    entry_type: Option<&str>,
    model: Model,
) -> Result<Vec<(i64, f32)>> {
    crate::store::documents::with_savepoint(conn, "memory_vector_search_for_model", || {
        require_model(conn, model.identity(), false)?;
        let embedding_bytes = query_embedding.as_bytes();

        // A typed search over-fetches, because the filter below runs after the KNN
        // and would otherwise return fewer than `limit` rows whenever the nearest
        // neighbours carry another type.
        let fetch_limit = if entry_type.is_some() {
            limit.saturating_mul(5)
        } else {
            limit
        };

        let mut stmt = conn.prepare(
        "SELECT memory_rowid, distance FROM vec_memory WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
    )?;

        let mut results: Vec<(i64, f32)> = stmt
            .query_map(params![embedding_bytes, fetch_limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f32>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // The BM25 leg filters on `m.entry_type`; this leg must agree, or a typed
        // query draws its whole semantic pool from every type and discards it.
        //
        // DEFERRED (2026-09-17) — post-filtering over a KNN result narrows what a
        // typed search returns but cannot guarantee completeness: if the nearest
        // `fetch_limit` entries all carry another type, a farther in-type match is
        // still invisible. A complete answer needs `entry_type` inside the vector
        // index (a vec0 metadata column). Not justified until a typed search is
        // measured coming back short.
        if let Some(entry_type) = entry_type {
            retain_memory_rows_of_type(conn, &mut results, entry_type);
        }
        results.truncate(limit);

        Ok(results)
    })
}

/// Drop the KNN hits whose memory row carries another `entry_type`.
///
/// A failure leaves the candidate set untouched and warns: an unfiltered
/// vector leg still meets the BM25 leg's own filter downstream, whereas an
/// empty one silently answers "nothing found".
fn retain_memory_rows_of_type(
    conn: &Connection,
    candidates: &mut Vec<(i64, f32)>,
    entry_type: &str,
) {
    let placeholders: String = candidates
        .iter()
        .map(|(rowid, _)| rowid.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT rowid FROM memory_entries WHERE rowid IN ({placeholders}) AND entry_type = ?1"
    );
    let kept: std::collections::HashSet<i64> = match conn.prepare(&sql).and_then(|mut stmt| {
        stmt.query_map(params![entry_type], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()
    }) {
        Ok(ids) => ids.into_iter().collect(),
        Err(e) => {
            tracing::warn!("entry_type filter for the memory vector leg failed: {e}");
            return;
        }
    };
    candidates.retain(|(rowid, _)| kept.contains(rowid));
}

/// Delete embedding for a memory entry.
pub fn delete_memory_embedding(conn: &Connection, memory_rowid: i64) -> Result<bool> {
    let rows = conn.execute(
        "DELETE FROM memory_embeddings WHERE memory_rowid = ?1",
        params![memory_rowid],
    )?;
    conn.execute(
        "DELETE FROM vec_memory WHERE memory_rowid = ?1",
        params![memory_rowid],
    )?;
    Ok(rows > 0)
}

/// The stored vectors of the given memory entries under `model`, keyed by entry
/// id. An entry without a vector of that model is absent from the map.
pub fn memory_embeddings_by_id(
    conn: &Connection,
    ids: &[&str],
    model: Model,
) -> Result<std::collections::HashMap<String, Vec<f32>>> {
    stored_vectors(
        conn,
        "SELECT m.id, e.embedding FROM memory_entries m \
         JOIN memory_embeddings e ON e.memory_rowid = m.rowid \
         WHERE e.model = ?1 AND m.id IN",
        ids,
        model,
    )
}

/// The stored document-level vectors of the given documents under `model`,
/// keyed by document id. A document without one is absent from the map.
pub fn document_embeddings_by_id(
    conn: &Connection,
    ids: &[i64],
    model: Model,
) -> Result<std::collections::HashMap<i64, Vec<f32>>> {
    stored_vectors(
        conn,
        "SELECT document_id, embedding FROM embeddings WHERE model = ?1 AND document_id IN",
        ids,
        model,
    )
}

/// Run `select` (ending in `IN`, with `?1` bound to the model identity) over
/// `keys`, and decode each `(key, embedding)` row.
fn stored_vectors<K>(
    conn: &Connection,
    select: &str,
    keys: &[impl rusqlite::ToSql],
    model: Model,
) -> Result<std::collections::HashMap<K, Vec<f32>>>
where
    K: rusqlite::types::FromSql + std::hash::Hash + Eq,
{
    if keys.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let placeholders = (2..keys.len() + 2)
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!("{select} ({placeholders})"))?;
    let identity = model.identity();
    let params = std::iter::once(&identity as &dyn rusqlite::ToSql)
        .chain(keys.iter().map(|k| k as &dyn rusqlite::ToSql));
    let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
        let bytes: Vec<u8> = row.get(1)?;
        let vector = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        Ok((row.get::<_, K>(0)?, vector))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Embedding dimension (384 for AllMiniLML6V2).
pub const EMBEDDING_DIM: usize = 384;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;

    static INIT: Once = Once::new();

    fn setup_db() -> Connection {
        INIT.call_once(|| {
            init_sqlite_vec();
        });

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        init_vector_schema(&conn).unwrap();
        conn
    }

    /// Create a test embedding with the correct dimension.
    fn test_embedding(seed: f32) -> Vec<f32> {
        (0..EMBEDDING_DIM)
            .map(|i| seed + i as f32 * 0.001)
            .collect()
    }

    #[test]
    fn test_init_vector_schema() {
        INIT.call_once(|| {
            init_sqlite_vec();
        });

        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let result = init_vector_schema(&conn);
        assert!(result.is_ok());
    }

    #[test]
    fn test_store_and_get_embedding() {
        let conn = setup_db();

        // Need a document first
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('abc', 'test', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', 'test.md', 'abc', 0, 0)",
            [],
        ).unwrap();

        let embedding = test_embedding(0.1);
        store_embedding(&conn, 1, &embedding, "AllMiniLML6V2").expect("store should succeed");

        let retrieved = get_embedding(&conn, 1)
            .expect("get should succeed")
            .expect("embedding should exist");

        assert_eq!(retrieved.len(), EMBEDDING_DIM);
        assert!((retrieved[0] - 0.1).abs() < 0.001);
    }

    /// Seed one collection with one embedded document, returning its id.
    fn seed_doc(conn: &Connection, collection: &str, path: &str, hash: &str, seed: f32) -> i64 {
        conn.execute(
            "INSERT OR IGNORE INTO collections (name, path, pattern, created_at, updated_at)
             VALUES (?1, '.', '*.md', 0, 0)",
            params![collection],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES (?1, 'body', 0)",
            params![hash],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at)
             VALUES (?1, ?2, ?3, 0, 0)",
            params![collection, path, hash],
        )
        .unwrap();
        let id = conn.last_insert_rowid();
        store_embedding(conn, id, &test_embedding(seed), "AllMiniLML6V2").unwrap();
        id
    }

    #[test]
    fn chunk_vector_search_scoped_to_a_collection_excludes_other_collections() {
        // The BM25 leg of `hybrid_search_fts` filters on `d.collection`; the
        // vector leg must too. Otherwise a scoped query whose wording does not
        // overlap the text lexically draws its whole candidate pool from the
        // corpus, every out-of-collection candidate is discarded after the cap,
        // and the search answers "nothing found" while a match exists.
        let conn = setup_db();
        let wanted = seed_doc(&conn, "docs", "wanted.md", "h-wanted", 0.30);
        // Nearer to the query than the wanted document, and in another collection.
        let _decoy = seed_doc(&conn, "notes", "decoy.md", "h-decoy", 0.10);

        let query = test_embedding(0.10);

        // Unscoped: the nearer decoy leads.
        let all = chunk_vector_search(&conn, &query, 10, None).unwrap();
        assert!(
            all.len() == 2,
            "unscoped search sees both documents, got {all:?}"
        );

        // Scoped: only the wanted document, even though it is the farther one.
        let scoped = chunk_vector_search(&conn, &query, 10, Some("docs")).unwrap();
        assert_eq!(
            scoped.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![wanted],
            "a scoped vector search must return only documents in that collection"
        );
    }

    #[test]
    fn test_has_embedding() {
        let conn = setup_db();

        // Setup document
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('abc', 'test', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', 'test.md', 'abc', 0, 0)",
            [],
        ).unwrap();

        assert!(!has_embedding(&conn, 1).unwrap());

        let embedding = test_embedding(0.5);
        store_embedding(&conn, 1, &embedding, "AllMiniLML6V2").unwrap();

        assert!(has_embedding(&conn, 1).unwrap());
    }

    #[test]
    fn test_delete_embedding() {
        let conn = setup_db();

        // Setup document
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('abc', 'test', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', 'test.md', 'abc', 0, 0)",
            [],
        ).unwrap();

        let embedding = test_embedding(0.5);
        store_embedding(&conn, 1, &embedding, "AllMiniLML6V2").unwrap();
        assert!(has_embedding(&conn, 1).unwrap());

        let deleted = delete_embedding(&conn, 1).unwrap();
        assert!(deleted);
        assert!(!has_embedding(&conn, 1).unwrap());

        // Deleting again should return false
        let deleted_again = delete_embedding(&conn, 1).unwrap();
        assert!(!deleted_again);
    }

    #[test]
    fn test_count_embeddings() {
        let conn = setup_db();

        // Setup documents
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('abc', 'test1', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('def', 'test2', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', 'test1.md', 'abc', 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', 'test2.md', 'def', 0, 0)",
            [],
        ).unwrap();

        assert_eq!(count_embeddings(&conn).unwrap(), 0);

        store_embedding(&conn, 1, &test_embedding(0.1), "AllMiniLML6V2").unwrap();
        assert_eq!(count_embeddings(&conn).unwrap(), 1);

        store_embedding(&conn, 2, &test_embedding(0.2), "AllMiniLML6V2").unwrap();
        assert_eq!(count_embeddings(&conn).unwrap(), 2);
    }

    #[test]
    fn test_vector_search() {
        let conn = setup_db();

        // Setup documents
        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();

        for i in 1..=5 {
            conn.execute(
                "INSERT INTO content (hash, body, created_at) VALUES (?1, ?2, 0)",
                params![format!("hash{}", i), format!("content{}", i)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', ?1, ?2, 0, 0)",
                params![format!("doc{}.md", i), format!("hash{}", i)],
            ).unwrap();
        }

        // Store embeddings with different seeds (simulating different content)
        for i in 1..=5 {
            let embedding = test_embedding(i as f32 * 0.1);
            store_embedding(&conn, i, &embedding, "AllMiniLML6V2").unwrap();
        }

        // Search with a query embedding similar to doc 3
        let query = test_embedding(0.29); // Close to doc 3 (0.3)
        let results = vector_search(&conn, &query, 3).unwrap();

        assert_eq!(results.len(), 3);
        // Doc 3 should be closest (has seed 0.3, closest to 0.29)
        assert_eq!(results[0].0, 3);
    }

    #[test]
    fn test_vector_search_limit() {
        let conn = setup_db();

        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();

        for i in 1..=10 {
            conn.execute(
                "INSERT INTO content (hash, body, created_at) VALUES (?1, ?2, 0)",
                params![format!("hash{}", i), format!("content{}", i)],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', ?1, ?2, 0, 0)",
                params![format!("doc{}.md", i), format!("hash{}", i)],
            ).unwrap();
            store_embedding(&conn, i, &test_embedding(i as f32 * 0.1), "AllMiniLML6V2").unwrap();
        }

        let query = test_embedding(0.5);

        let results_5 = vector_search(&conn, &query, 5).unwrap();
        assert_eq!(results_5.len(), 5);

        let results_3 = vector_search(&conn, &query, 3).unwrap();
        assert_eq!(results_3.len(), 3);
    }

    #[test]
    fn test_vector_search_empty() {
        let conn = setup_db();

        let query = test_embedding(0.5);
        let results = vector_search(&conn, &query, 10).unwrap();
        assert_eq!(results, [] as [(i64, f32); 0]);
    }

    #[test]
    fn test_embedding_update_replaces() {
        let conn = setup_db();

        conn.execute(
            "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('abc', 'test', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, file_modified_at, indexed_at) VALUES ('test', 'test.md', 'abc', 0, 0)",
            [],
        ).unwrap();

        // Store initial embedding
        let embedding1 = test_embedding(0.1);
        store_embedding(&conn, 1, &embedding1, "AllMiniLML6V2").unwrap();

        // Update with new embedding
        let embedding2 = test_embedding(0.9);
        store_embedding(&conn, 1, &embedding2, "AllMiniLML6V2").unwrap();

        // Should still have only one embedding
        assert_eq!(count_embeddings(&conn).unwrap(), 1);

        // Retrieved embedding should be the updated one
        let retrieved = get_embedding(&conn, 1).unwrap().unwrap();
        assert!((retrieved[0] - 0.9).abs() < 0.001);
    }

    // ==================== Memory Embedding Tests ====================

    fn insert_memory_entry(conn: &Connection, id: &str, title: &str, content: &str) -> i64 {
        let now = chrono::Utc::now().timestamp();
        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, tags, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'topic', '[]', 'active', ?4, ?4)",
            params![id, title, content, now],
        ).unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn test_store_and_search_memory_embedding() {
        let conn = setup_db();

        let rowid1 = insert_memory_entry(
            &conn,
            "auth-flow",
            "OAuth PKCE Flow",
            "How we handle authentication with PKCE",
        );
        let rowid2 = insert_memory_entry(
            &conn,
            "db-perf",
            "Database Performance",
            "SQLite tuning and WAL mode configuration",
        );
        let rowid3 = insert_memory_entry(
            &conn,
            "jwt-refresh",
            "JWT Refresh Strategy",
            "Token expiration and refresh flow design",
        );

        // Embeddings: auth entries (1,3) get similar vectors, db entry (2) gets different
        let auth_embedding = test_embedding(0.1);
        let db_embedding = test_embedding(0.5);
        let jwt_embedding = test_embedding(0.12); // close to auth

        store_memory_embedding(&conn, rowid1, &auth_embedding, "AllMiniLML6V2").unwrap();
        store_memory_embedding(&conn, rowid2, &db_embedding, "AllMiniLML6V2").unwrap();
        store_memory_embedding(&conn, rowid3, &jwt_embedding, "AllMiniLML6V2").unwrap();

        // Search with query close to auth topics
        let query = test_embedding(0.11);
        let results = memory_vector_search(&conn, &query, 3, None).unwrap();

        assert_eq!(results.len(), 3);
        // Auth entries should be closest
        let top_2_rowids: Vec<i64> = results.iter().take(2).map(|(id, _)| *id).collect();
        assert!(
            top_2_rowids.contains(&rowid1),
            "auth-flow should be in top 2"
        );
        assert!(
            top_2_rowids.contains(&rowid3),
            "jwt-refresh should be in top 2"
        );
    }

    #[test]
    fn test_delete_memory_embedding() {
        let conn = setup_db();

        let rowid = insert_memory_entry(&conn, "test-entry", "Test", "Test content");
        store_memory_embedding(&conn, rowid, &test_embedding(0.1), "AllMiniLML6V2").unwrap();

        let deleted = delete_memory_embedding(&conn, rowid).unwrap();
        assert!(deleted);

        // Search should return empty
        let results = memory_vector_search(&conn, &test_embedding(0.1), 10, None).unwrap();
        assert_eq!(results, [] as [(i64, f32); 0]);

        // Deleting again should return false
        assert!(!delete_memory_embedding(&conn, rowid).unwrap());
    }

    #[test]
    fn test_memory_embedding_update_replaces() {
        let conn = setup_db();

        let rowid = insert_memory_entry(&conn, "evolving", "Evolving Entry", "Content v1");
        store_memory_embedding(&conn, rowid, &test_embedding(0.1), "AllMiniLML6V2").unwrap();
        store_memory_embedding(&conn, rowid, &test_embedding(0.9), "AllMiniLML6V2").unwrap();

        // Search should find entry once, with updated embedding
        let query = test_embedding(0.89);
        let results = memory_vector_search(&conn, &query, 10, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, rowid);
    }

    /// A file-backed connection with the pragmas of the daemon's long-lived
    /// connection (`Context::configure_connection`). WAL mode is what makes a
    /// leaked write transaction block checkpoints and other writers.
    fn setup_wal_db(dir: &tempfile::TempDir) -> Connection {
        INIT.call_once(|| {
            init_sqlite_vec();
        });

        let conn = Connection::open(dir.path().join("index.sqlite")).unwrap();
        conn.execute_batch(
            "
            PRAGMA busy_timeout = 5000;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            ",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        init_vector_schema(&conn).unwrap();
        conn
    }

    /// Run the real `store_memory_embedding` into a failure: a vector of the
    /// wrong dimension passes the plain `memory_embeddings` insert and fails
    /// on the `vec_memory` insert, so the savepoint has work to undo.
    fn fail_memory_embedding(conn: &Connection) -> i64 {
        let rowid = insert_memory_entry(conn, "bad-dim", "Bad dimension", "wrong vector size");
        let result = store_memory_embedding(conn, rowid, &[0.1, 0.2, 0.3], "AllMiniLML6V2");
        assert!(result.is_err(), "a 3-float vector must fail on FLOAT[384]");
        rowid
    }

    #[test]
    fn failed_vec_memory_insert_leaves_connection_in_autocommit() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_wal_db(&dir);

        let rowid = fail_memory_embedding(&conn);

        assert!(
            conn.is_autocommit(),
            "a failed savepoint must not leave a write transaction open"
        );
        let leaked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_embeddings WHERE memory_rowid = ?1",
                params![rowid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leaked, 0, "the memory_embeddings row must be rolled back");

        // A second connection must be able to write immediately. A short
        // busy_timeout turns a leaked write lock into a failure, not a stall.
        let other = Connection::open(dir.path().join("index.sqlite")).unwrap();
        other.execute_batch("PRAGMA busy_timeout = 100;").unwrap();
        insert_memory_entry(&other, "second-writer", "Second", "writes without waiting");
    }

    #[test]
    fn failed_store_memory_embedding_does_not_block_wal_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let conn = setup_wal_db(&dir);

        fail_memory_embedding(&conn);

        // With a transaction open on this connection the pragma fails with
        // "database table is locked"; with a blocked checkpoint it reports busy.
        let busy: i64 = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))
            .expect("checkpoint must run after a failed embedding store");
        assert_eq!(busy, 0, "checkpoint must not be blocked");
    }

    /// Catches a lookup that returns another model's vector, which recall would
    /// then compare against a vector of the selected model.
    #[test]
    fn document_embeddings_by_id_returns_only_the_given_models_vectors() {
        let conn = setup_db();
        setup_doc(&conn, 1, "a.md");
        setup_doc(&conn, 2, "b.md");
        for (id, model) in [(1, Model::Granite107M), (2, Model::MiniLm)] {
            conn.execute(
                "INSERT INTO embeddings (document_id, embedding, model, created_at) \
                 VALUES (?1, ?2, ?3, 0)",
                params![id, [0.5_f32, 0.25].as_bytes(), model.identity()],
            )
            .unwrap();
        }
        let found = document_embeddings_by_id(&conn, &[1, 2, 3], Model::Granite107M).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[&1], vec![0.5, 0.25]);
    }

    // ==================== Chunk Embedding Tests ====================

    fn setup_doc(conn: &Connection, id: i64, path: &str) {
        conn.execute(
            "INSERT OR IGNORE INTO collections (name, path, pattern, created_at, updated_at) VALUES ('test', '.', '*.md', 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO content (hash, body, created_at) VALUES (?1, ?2, 0)",
            params![format!("hash{id}"), format!("content for {path}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO documents (id, collection, relative_path, hash, file_modified_at, indexed_at) VALUES (?1, 'test', ?2, ?3, 0, 0)",
            params![id, path, format!("hash{id}")],
        ).unwrap();
    }

    #[test]
    fn test_store_and_search_chunk_embeddings() {
        let conn = setup_db();
        setup_doc(&conn, 1, "big-doc.md");
        setup_doc(&conn, 2, "small-doc.md");

        // Doc 1 has 3 chunks with different embeddings
        let chunks = vec![
            crate::store::chunks::Chunk {
                index: 0,
                content: "Introduction section".into(),
                heading_path: Some("Introduction".into()),
            },
            crate::store::chunks::Chunk {
                index: 1,
                content: "Auth section about tokens".into(),
                heading_path: Some("Authentication".into()),
            },
            crate::store::chunks::Chunk {
                index: 2,
                content: "Database section".into(),
                heading_path: Some("Database".into()),
            },
        ];
        let embeddings = vec![
            test_embedding(0.1), // intro
            test_embedding(0.5), // auth (distinct)
            test_embedding(0.9), // database (distinct)
        ];
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "AllMiniLML6V2").unwrap();

        // Doc 2 has a single whole-doc embedding (no chunks)
        store_embedding(&conn, 2, &test_embedding(0.51), "AllMiniLML6V2").unwrap();

        // Search near auth section (0.5) — should find doc 1 via chunk, doc 2 via whole-doc
        let query = test_embedding(0.49);
        let results = chunk_vector_search(&conn, &query, 10, None).unwrap();

        assert!(
            results.len() >= 2,
            "Should find both docs, got {}",
            results.len()
        );

        // Doc 1 should be found because chunk 1 (auth, 0.5) is close to query (0.49)
        assert!(
            results.iter().any(|(id, _)| *id == 1),
            "Doc 1 should be found via chunk match"
        );
        // Doc 2 also close (0.51)
        assert!(
            results.iter().any(|(id, _)| *id == 2),
            "Doc 2 should be found via whole-doc"
        );
    }

    #[test]
    fn test_chunk_search_aggregates_to_document() {
        let conn = setup_db();
        setup_doc(&conn, 1, "multi-chunk.md");

        // 3 chunks, all for doc 1
        let chunks = vec![
            crate::store::chunks::Chunk {
                index: 0,
                content: "A".into(),
                heading_path: None,
            },
            crate::store::chunks::Chunk {
                index: 1,
                content: "B".into(),
                heading_path: None,
            },
            crate::store::chunks::Chunk {
                index: 2,
                content: "C".into(),
                heading_path: None,
            },
        ];
        let embeddings = vec![
            test_embedding(0.1),
            test_embedding(0.2),
            test_embedding(0.3),
        ];
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "AllMiniLML6V2").unwrap();

        let query = test_embedding(0.15);
        let results = chunk_vector_search(&conn, &query, 10, None).unwrap();

        // Should return doc 1 only once (aggregated from 3 chunks)
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_delete_chunk_embeddings() {
        let conn = setup_db();
        setup_doc(&conn, 1, "doc.md");

        let chunks = vec![
            crate::store::chunks::Chunk {
                index: 0,
                content: "A".into(),
                heading_path: None,
            },
            crate::store::chunks::Chunk {
                index: 1,
                content: "B".into(),
                heading_path: None,
            },
        ];
        let embeddings = vec![test_embedding(0.1), test_embedding(0.2)];
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "AllMiniLML6V2").unwrap();

        assert!(has_chunk_embeddings(&conn, 1).unwrap());

        delete_chunk_embeddings(&conn, 1).unwrap();

        assert!(!has_chunk_embeddings(&conn, 1).unwrap());

        // Chunk vector search should fall back to doc-level (which was also stored)
        let results = chunk_vector_search(&conn, &test_embedding(0.1), 10, None).unwrap();
        // Doc-level embedding was stored by store_chunk_embeddings, still exists
        assert!(results.len() <= 1);
    }

    #[test]
    fn test_chunk_search_fallback_to_doc_level() {
        let conn = setup_db();
        setup_doc(&conn, 1, "old-doc.md");

        // Only whole-doc embedding, no chunks
        store_embedding(&conn, 1, &test_embedding(0.5), "AllMiniLML6V2").unwrap();

        let results = chunk_vector_search(&conn, &test_embedding(0.49), 10, None).unwrap();
        assert_eq!(results.len(), 1, "Should fall back to doc-level search");
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_delete_document_cascades_to_vec_documents() {
        let conn = setup_db();
        setup_doc(&conn, 1, "doc.md");

        store_embedding(&conn, 1, &test_embedding(0.5), "AllMiniLML6V2").unwrap();

        // Verify embedding exists
        let count: i64 = conn
            .query_row("SELECT count(*) FROM vec_documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "vec_documents should have 1 entry");

        // Delete the document — should cascade to vec_documents
        conn.execute("DELETE FROM documents WHERE id = 1", [])
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT count(*) FROM vec_documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 0,
            "vec_documents should be empty after document delete"
        );
    }

    #[test]
    fn test_delete_document_cascades_to_vec_chunks() {
        let conn = setup_db();
        setup_doc(&conn, 1, "doc.md");

        let chunks = vec![
            crate::store::chunks::Chunk {
                index: 0,
                content: "A".into(),
                heading_path: None,
            },
            crate::store::chunks::Chunk {
                index: 1,
                content: "B".into(),
                heading_path: None,
            },
        ];
        let embeddings = vec![test_embedding(0.1), test_embedding(0.2)];
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "AllMiniLML6V2").unwrap();

        // Verify chunks exist in vec_chunks
        let count: i64 = conn
            .query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "vec_chunks should have 2 entries");

        // Delete the document — should cascade to vec_chunks via document_chunks
        conn.execute("DELETE FROM documents WHERE id = 1", [])
            .unwrap();

        let count: i64 = conn
            .query_row("SELECT count(*) FROM vec_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "vec_chunks should be empty after document delete");
    }
}
