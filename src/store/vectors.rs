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

/// Initialize vector storage tables.
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

    // Create virtual table for vector search
    // Using vec0 for approximate nearest neighbor search
    conn.execute(
        r#"
        CREATE VIRTUAL TABLE IF NOT EXISTS vec_documents USING vec0(
            document_id INTEGER PRIMARY KEY,
            embedding FLOAT[768]
        )
        "#,
        [],
    )?;

    Ok(())
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

/// Embedding dimension (768 for nomic-embed-text).
pub const EMBEDDING_DIM: usize = 768;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;
    use std::sync::Once;

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
}
