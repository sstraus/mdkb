//! Vector storage for semantic search.
//!
//! Uses sqlite-vec for vector similarity search.

use std::sync::Once;

use rusqlite::{Connection, ffi::sqlite3_auto_extension, params};
use sqlite_vec::sqlite3_vec_init;
use zerocopy::AsBytes;

use crate::error::Result;

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
pub fn store_embedding(
    conn: &Connection,
    document_id: i64,
    embedding: &[f32],
    model: &str,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();

    // Convert to bytes for storage using zerocopy
    let embedding_bytes = embedding.as_bytes();

    // Store in embeddings table
    conn.execute(
        r#"
        INSERT OR REPLACE INTO embeddings (document_id, embedding, model, created_at)
        VALUES (?1, ?2, ?3, ?4)
        "#,
        params![document_id, embedding_bytes, model, now],
    )?;

    // Store in vector index
    // Note: sqlite-vec virtual tables don't support INSERT OR REPLACE,
    // so we need to delete first then insert
    conn.execute(
        "DELETE FROM vec_documents WHERE document_id = ?1",
        params![document_id],
    )?;
    conn.execute(
        r#"
        INSERT INTO vec_documents (document_id, embedding)
        VALUES (?1, ?2)
        "#,
        params![document_id, embedding_bytes],
    )?;

    Ok(())
}

/// Get embedding for a document.
pub fn get_embedding(conn: &Connection, document_id: i64) -> Result<Option<Vec<f32>>> {
    let result: Option<Vec<u8>> = conn
        .query_row(
            "SELECT embedding FROM embeddings WHERE document_id = ?1",
            params![document_id],
            |row| row.get(0),
        )
        .ok();

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

    let results = stmt
        .query_map(params![embedding_bytes, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f32>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
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

    let results = stmt
        .query_map(params![embedding_bytes, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f32>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
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
        ).unwrap();
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
        ).unwrap();
        conn.execute(
            "INSERT INTO content (hash, body, created_at) VALUES ('def', 'test2', 0)",
            [],
        ).unwrap();
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
            ).unwrap();
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
            ).unwrap();
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
        ).unwrap();
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

        let rowid1 = insert_memory_entry(&conn, "auth-flow", "OAuth PKCE Flow", "How we handle authentication with PKCE");
        let rowid2 = insert_memory_entry(&conn, "db-perf", "Database Performance", "SQLite tuning and WAL mode configuration");
        let rowid3 = insert_memory_entry(&conn, "jwt-refresh", "JWT Refresh Strategy", "Token expiration and refresh flow design");

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
        assert!(top_2_rowids.contains(&rowid1), "auth-flow should be in top 2");
        assert!(top_2_rowids.contains(&rowid3), "jwt-refresh should be in top 2");
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
}
