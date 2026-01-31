//! Vector storage for semantic search.
//!
//! Uses sqlite-vec for vector similarity search.

use std::sync::Once;

use rusqlite::{Connection, ffi::sqlite3_auto_extension, params};
use sqlite_vec::sqlite3_vec_init;
use zerocopy::AsBytes;

use crate::error::Result;
use crate::store::chunks;

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

/// Initialize vector storage tables, migrating from old dimensions if needed.
pub fn init_vector_schema(conn: &Connection) -> Result<()> {
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

    // Check if vec_documents exists with wrong dimensions and migrate if needed
    migrate_vector_table_if_needed(conn)?;

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

    Ok(())
}

/// Detect dimension mismatch in existing vec_documents table and recreate if needed.
///
/// sqlite-vec doesn't support ALTER, so we must drop and recreate.
/// Old embeddings in the `embeddings` table are also cleared since they
/// have the wrong dimension and must be regenerated.
fn migrate_vector_table_if_needed(conn: &Connection) -> Result<()> {
    // Check if the table exists by querying sqlite_master
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='vec_documents'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !table_exists {
        return Ok(());
    }

    // Try to detect dimension mismatch by checking if an existing embedding
    // has the wrong size. We probe the embeddings table since that stores raw blobs.
    let old_dim = detect_embedding_dimension(conn);

    if let Some(dim) = old_dim {
        if dim != EMBEDDING_DIM {
            tracing::info!(
                "Embedding dimension changed ({} -> {}), migrating vector tables",
                dim,
                EMBEDDING_DIM
            );

            // Drop old vector table (sqlite-vec doesn't support ALTER)
            conn.execute("DROP TABLE IF EXISTS vec_documents", [])?;

            // Clear old embeddings since they have wrong dimensions
            conn.execute("DELETE FROM embeddings", [])?;

            tracing::info!("Migration complete. Documents will be re-embedded on next update.");
        }
    }

    Ok(())
}

/// Detect the dimension of existing embeddings by checking blob size.
/// Returns None if no embeddings exist.
fn detect_embedding_dimension(conn: &Connection) -> Option<usize> {
    conn.query_row(
        "SELECT LENGTH(embedding) FROM embeddings LIMIT 1",
        [],
        |row| {
            let byte_len: usize = row.get(0)?;
            // Each f32 is 4 bytes
            Ok(byte_len / 4)
        },
    )
    .ok()
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

    conn.execute("SAVEPOINT store_embedding", [])?;
    let result = (|| -> Result<()> {
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
    })();

    match result {
        Ok(()) => {
            conn.execute("RELEASE store_embedding", [])?;
            Ok(())
        }
        Err(e) => {
            if let Err(rb) = conn.execute("ROLLBACK TO store_embedding", []) {
                tracing::error!("Savepoint rollback failed: {rb}; original: {e}");
            }
            Err(e)
        }
    }
}

/// Get embedding for a document.
pub fn get_embedding(conn: &Connection, document_id: i64) -> Result<Option<Vec<f32>>> {
    use rusqlite::OptionalExtension;
    let result: Option<Vec<u8>> = conn
        .query_row(
            "SELECT embedding FROM embeddings WHERE document_id = ?1",
            params![document_id],
            |row| row.get(0),
        )
        .optional()?;

    Ok(result.map(|bytes| {
        bytes
            .chunks(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect()
    }))
}

/// Search for similar documents by vector.
pub fn vector_search(
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
) -> Result<Vec<(i64, f32)>> {
    let embedding_bytes = query_embedding.as_bytes();
    let fetch_limit = limit * 5;

    // Combine results from chunk-level and doc-level vector search.
    let mut best_per_doc: std::collections::HashMap<i64, f32> = std::collections::HashMap::new();

    // 1. Chunk-level search (batch-resolved to document_id)
    for (doc_id, distance) in search_vec_chunks_by_doc(conn, embedding_bytes, fetch_limit) {
        let entry = best_per_doc.entry(doc_id).or_insert(f32::MAX);
        if distance < *entry {
            *entry = distance;
        }
    }

    // 2. Doc-level search (for small docs without chunks)
    let doc_results = vector_search(conn, query_embedding, fetch_limit)?;
    for (doc_id, distance) in doc_results {
        let entry = best_per_doc.entry(doc_id).or_insert(f32::MAX);
        if distance < *entry {
            *entry = distance;
        }
    }

    if best_per_doc.is_empty() {
        return Ok(Vec::new());
    }

    let mut results: Vec<(i64, f32)> = best_per_doc.into_iter().collect();
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.truncate(limit);

    Ok(results)
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

    conn.execute("SAVEPOINT store_memory_embedding", [])?;
    let result = (|| -> Result<()> {
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
    })();

    match result {
        Ok(()) => {
            conn.execute("RELEASE store_memory_embedding", [])?;
            Ok(())
        }
        Err(e) => {
            if let Err(rb) = conn.execute("ROLLBACK TO store_memory_embedding", []) {
                tracing::error!("Savepoint rollback failed: {rb}; original: {e}");
            }
            Err(e)
        }
    }
}

/// Search for similar memory entries by vector.
pub fn memory_vector_search(
    conn: &Connection,
    query_embedding: &[f32],
    limit: usize,
) -> Result<Vec<(i64, f32)>> {
    let embedding_bytes = query_embedding.as_bytes();

    let mut stmt = conn.prepare(
        "SELECT memory_rowid, distance FROM vec_memory WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2",
    )?;

    let results: std::result::Result<Vec<_>, _> = stmt
        .query_map(params![embedding_bytes, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f32>(1)?))
        })?
        .collect();

    Ok(results?)
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
        store_embedding(&conn, 1, &embedding, "test-model").expect("store should succeed");

        let retrieved = get_embedding(&conn, 1)
            .expect("get should succeed")
            .expect("embedding should exist");

        assert_eq!(retrieved.len(), EMBEDDING_DIM);
        assert!((retrieved[0] - 0.1).abs() < 0.001);
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
        store_embedding(&conn, 1, &embedding, "test").unwrap();

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
        store_embedding(&conn, 1, &embedding, "test").unwrap();
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

        store_embedding(&conn, 1, &test_embedding(0.1), "test").unwrap();
        assert_eq!(count_embeddings(&conn).unwrap(), 1);

        store_embedding(&conn, 2, &test_embedding(0.2), "test").unwrap();
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
            store_embedding(&conn, i, &embedding, "test").unwrap();
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
            store_embedding(&conn, i, &test_embedding(i as f32 * 0.1), "test").unwrap();
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
        assert!(results.is_empty());
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
        store_embedding(&conn, 1, &embedding1, "model-v1").unwrap();

        // Update with new embedding
        let embedding2 = test_embedding(0.9);
        store_embedding(&conn, 1, &embedding2, "model-v2").unwrap();

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

        store_memory_embedding(&conn, rowid1, &auth_embedding, "test").unwrap();
        store_memory_embedding(&conn, rowid2, &db_embedding, "test").unwrap();
        store_memory_embedding(&conn, rowid3, &jwt_embedding, "test").unwrap();

        // Search with query close to auth topics
        let query = test_embedding(0.11);
        let results = memory_vector_search(&conn, &query, 3).unwrap();

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
        store_memory_embedding(&conn, rowid, &test_embedding(0.1), "test").unwrap();

        let deleted = delete_memory_embedding(&conn, rowid).unwrap();
        assert!(deleted);

        // Search should return empty
        let results = memory_vector_search(&conn, &test_embedding(0.1), 10).unwrap();
        assert!(results.is_empty());

        // Deleting again should return false
        assert!(!delete_memory_embedding(&conn, rowid).unwrap());
    }

    #[test]
    fn test_memory_embedding_update_replaces() {
        let conn = setup_db();

        let rowid = insert_memory_entry(&conn, "evolving", "Evolving Entry", "Content v1");
        store_memory_embedding(&conn, rowid, &test_embedding(0.1), "test").unwrap();
        store_memory_embedding(&conn, rowid, &test_embedding(0.9), "test").unwrap();

        // Search should find entry once, with updated embedding
        let query = test_embedding(0.89);
        let results = memory_vector_search(&conn, &query, 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, rowid);
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
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "test").unwrap();

        // Doc 2 has a single whole-doc embedding (no chunks)
        store_embedding(&conn, 2, &test_embedding(0.51), "test").unwrap();

        // Search near auth section (0.5) — should find doc 1 via chunk, doc 2 via whole-doc
        let query = test_embedding(0.49);
        let results = chunk_vector_search(&conn, &query, 10).unwrap();

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
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "test").unwrap();

        let query = test_embedding(0.15);
        let results = chunk_vector_search(&conn, &query, 10).unwrap();

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
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "test").unwrap();

        assert!(has_chunk_embeddings(&conn, 1).unwrap());

        delete_chunk_embeddings(&conn, 1).unwrap();

        assert!(!has_chunk_embeddings(&conn, 1).unwrap());

        // Chunk vector search should fall back to doc-level (which was also stored)
        let results = chunk_vector_search(&conn, &test_embedding(0.1), 10).unwrap();
        // Doc-level embedding was stored by store_chunk_embeddings, still exists
        assert!(results.len() <= 1);
    }

    #[test]
    fn test_chunk_search_fallback_to_doc_level() {
        let conn = setup_db();
        setup_doc(&conn, 1, "old-doc.md");

        // Only whole-doc embedding, no chunks
        store_embedding(&conn, 1, &test_embedding(0.5), "test").unwrap();

        let results = chunk_vector_search(&conn, &test_embedding(0.49), 10).unwrap();
        assert_eq!(results.len(), 1, "Should fall back to doc-level search");
        assert_eq!(results[0].0, 1);
    }

    #[test]
    fn test_delete_document_cascades_to_vec_documents() {
        let conn = setup_db();
        setup_doc(&conn, 1, "doc.md");

        store_embedding(&conn, 1, &test_embedding(0.5), "test").unwrap();

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
        store_chunk_embeddings(&conn, 1, &chunks, &embeddings, "test").unwrap();

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
