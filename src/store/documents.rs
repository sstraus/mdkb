//! Document storage operations.

use crate::domain::Document;
use crate::error::Result;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

/// Compute SHA256 hash of content.
pub fn compute_hash(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Store content if not already present (deduplication).
pub fn store_content(conn: &Connection, content: &str) -> Result<String> {
    let hash = compute_hash(content);
    let now = chrono::Utc::now().timestamp();

    // INSERT OR IGNORE for content-addressable deduplication
    conn.execute(
        "INSERT OR IGNORE INTO content (hash, body, created_at) VALUES (?1, ?2, ?3)",
        params![hash, content, now],
    )?;

    Ok(hash)
}

/// Get content by hash.
pub fn get_content(conn: &Connection, hash: &str) -> Result<Option<String>> {
    let result = conn
        .query_row(
            "SELECT body FROM content WHERE hash = ?1",
            params![hash],
            |row| row.get(0),
        )
        .optional()?;
    Ok(result)
}

/// Index a document (insert or update) within a transaction.
///
/// This function wraps all database operations in a transaction to ensure
/// atomicity. If the process crashes mid-operation, the database will
/// remain in a consistent state.
pub fn index_document(conn: &Connection, doc: &Document, content: &str) -> Result<i64> {
    // Use IMMEDIATE transaction to get write lock early and avoid deadlocks
    conn.execute("BEGIN IMMEDIATE", [])?;

    match index_document_inner(conn, doc, content) {
        Ok(id) => {
            conn.execute("COMMIT", [])?;
            Ok(id)
        }
        Err(e) => {
            // Rollback on error - ignore rollback errors since we're already failing
            let _ = conn.execute("ROLLBACK", []);
            Err(e)
        }
    }
}

/// Inner implementation of index_document without transaction handling.
fn index_document_inner(conn: &Connection, doc: &Document, content: &str) -> Result<i64> {
    // Store content first (deduplication)
    let hash = store_content_inner(conn, content)?;

    // Use INSERT OR REPLACE for atomic upsert (fixes race condition P2-DATA-003)
    let metadata_json = doc
        .metadata
        .as_ref()
        .map(|m| serde_json::to_string(m).map_err(|e| {
            crate::error::Error::other(format!("Failed to serialize metadata: {}", e))
        }))
        .transpose()?;

    // Check if document already exists
    let existing_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM documents WHERE collection = ?1 AND relative_path = ?2",
            params![doc.collection, doc.relative_path],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(id) = existing_id {
        // Update existing document
        conn.execute(
            "UPDATE documents SET hash = ?1, title = ?2, metadata = ?3, file_modified_at = ?4, indexed_at = ?5 WHERE id = ?6",
            params![
                hash,
                doc.title,
                metadata_json,
                doc.file_modified_at,
                doc.indexed_at,
                id,
            ],
        )?;
        Ok(id)
    } else {
        // Insert new document
        conn.execute(
            "INSERT INTO documents (collection, relative_path, hash, title, metadata, file_modified_at, indexed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                doc.collection,
                doc.relative_path,
                hash,
                doc.title,
                metadata_json,
                doc.file_modified_at,
                doc.indexed_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }
}

/// Store content without transaction (for use within existing transactions).
fn store_content_inner(conn: &Connection, content: &str) -> Result<String> {
    let hash = compute_hash(content);
    let now = chrono::Utc::now().timestamp();

    conn.execute(
        "INSERT OR IGNORE INTO content (hash, body, created_at) VALUES (?1, ?2, ?3)",
        params![hash, content, now],
    )?;

    Ok(hash)
}

/// Get a document by ID.
pub fn get_document(conn: &Connection, id: i64) -> Result<Option<Document>> {
    let result = conn
        .query_row(
            "SELECT id, collection, relative_path, hash, title, metadata, file_modified_at, indexed_at FROM documents WHERE id = ?1",
            params![id],
            |row| {
                let metadata_str: Option<String> = row.get(5)?;
                let metadata = metadata_str
                    .and_then(|s| serde_json::from_str(&s).ok());
                Ok(Document {
                    id: row.get(0)?,
                    collection: row.get(1)?,
                    relative_path: row.get(2)?,
                    hash: row.get(3)?,
                    title: row.get(4)?,
                    metadata,
                    file_modified_at: row.get(6)?,
                    indexed_at: row.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

/// Get a document by collection and path.
pub fn get_document_by_path(
    conn: &Connection,
    collection: &str,
    path: &str,
) -> Result<Option<Document>> {
    let result = conn
        .query_row(
            "SELECT id, collection, relative_path, hash, title, metadata, file_modified_at, indexed_at FROM documents WHERE collection = ?1 AND relative_path = ?2",
            params![collection, path],
            |row| {
                let metadata_str: Option<String> = row.get(5)?;
                let metadata = metadata_str
                    .and_then(|s| serde_json::from_str(&s).ok());
                Ok(Document {
                    id: row.get(0)?,
                    collection: row.get(1)?,
                    relative_path: row.get(2)?,
                    hash: row.get(3)?,
                    title: row.get(4)?,
                    metadata,
                    file_modified_at: row.get(6)?,
                    indexed_at: row.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

/// Delete a document by ID.
pub fn delete_document(conn: &Connection, id: i64) -> Result<bool> {
    let rows_affected = conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
    Ok(rows_affected > 0)
}

/// List all documents in a collection.
pub fn list_documents(conn: &Connection, collection: &str) -> Result<Vec<Document>> {
    let mut stmt = conn.prepare(
        "SELECT id, collection, relative_path, hash, title, metadata, file_modified_at, indexed_at FROM documents WHERE collection = ?1",
    )?;

    let rows = stmt.query_map(params![collection], |row| {
        let metadata_str: Option<String> = row.get(5)?;
        let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
        Ok(Document {
            id: row.get(0)?,
            collection: row.get(1)?,
            relative_path: row.get(2)?,
            hash: row.get(3)?,
            title: row.get(4)?,
            metadata,
            file_modified_at: row.get(6)?,
            indexed_at: row.get(7)?,
        })
    })?;

    let mut documents = Vec::new();
    for row in rows {
        documents.push(row?);
    }
    Ok(documents)
}

/// Get multiple documents by their IDs in a single query (fixes N+1 query pattern).
pub fn get_documents_batch(conn: &Connection, ids: &[i64]) -> Result<Vec<Document>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    // Build parameterized query with placeholders
    let placeholders: Vec<String> = (1..=ids.len()).map(|i| format!("?{i}")).collect();
    let query = format!(
        "SELECT id, collection, relative_path, hash, title, metadata, file_modified_at, indexed_at \
         FROM documents WHERE id IN ({})",
        placeholders.join(", ")
    );

    let mut stmt = conn.prepare(&query)?;
    let params: Vec<&dyn rusqlite::ToSql> = ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();

    let rows = stmt.query_map(params.as_slice(), |row| {
        let metadata_str: Option<String> = row.get(5)?;
        let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
        Ok(Document {
            id: row.get(0)?,
            collection: row.get(1)?,
            relative_path: row.get(2)?,
            hash: row.get(3)?,
            title: row.get(4)?,
            metadata,
            file_modified_at: row.get(6)?,
            indexed_at: row.get(7)?,
        })
    })?;

    let mut documents = Vec::new();
    for row in rows {
        documents.push(row?);
    }
    Ok(documents)
}

/// Get multiple content entries by their hashes in a single query (fixes N+1 query pattern).
pub fn get_content_batch(conn: &Connection, hashes: &[&str]) -> Result<std::collections::HashMap<String, String>> {
    use std::collections::HashMap;

    if hashes.is_empty() {
        return Ok(HashMap::new());
    }

    // Build parameterized query with placeholders
    let placeholders: Vec<String> = (1..=hashes.len()).map(|i| format!("?{i}")).collect();
    let query = format!(
        "SELECT hash, body FROM content WHERE hash IN ({})",
        placeholders.join(", ")
    );

    let mut stmt = conn.prepare(&query)?;
    let params: Vec<&dyn rusqlite::ToSql> = hashes.iter().map(|h| h as &dyn rusqlite::ToSql).collect();

    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut result = HashMap::new();
    for row in rows {
        let (hash, body) = row?;
        result.insert(hash, body);
    }
    Ok(result)
}

/// Index a document within an existing transaction (no transaction management).
/// Use this when you're managing the transaction externally.
pub fn index_document_in_tx(conn: &Connection, doc: &Document, content: &str) -> Result<i64> {
    index_document_inner(conn, doc, content)
}

/// Begin a transaction for batch operations.
pub fn begin_transaction(conn: &Connection) -> Result<()> {
    conn.execute("BEGIN IMMEDIATE", [])?;
    Ok(())
}

/// Commit a transaction.
pub fn commit_transaction(conn: &Connection) -> Result<()> {
    conn.execute("COMMIT", [])?;
    Ok(())
}

/// Rollback a transaction.
pub fn rollback_transaction(conn: &Connection) -> Result<()> {
    conn.execute("ROLLBACK", [])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Collection;
    use crate::store::collections::add_collection;
    use crate::store::schema::init_schema;
    use chrono::Utc;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn setup_db_with_collection(name: &str) -> Connection {
        let conn = setup_db();
        let now = Utc::now().timestamp();
        let coll = Collection {
            name: name.to_string(),
            path: format!("./{name}"),
            pattern: "**/*.md".to_string(),
            source: "manual".to_string(),
            created_at: now,
            updated_at: now,
        };
        add_collection(&conn, &coll).unwrap();
        conn
    }

    fn make_doc(collection: &str, path: &str, title: Option<&str>) -> Document {
        let now = Utc::now().timestamp();
        Document {
            id: 0,
            collection: collection.to_string(),
            relative_path: path.to_string(),
            hash: String::new(),
            title: title.map(String::from),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
        }
    }

    // ==================== Content Tests ====================

    #[test]
    fn test_store_and_get_content() {
        let conn = setup_db();
        let content = "# Hello World\n\nThis is content.";

        let hash = store_content(&conn, content).expect("store should succeed");
        assert!(!hash.is_empty());

        let retrieved = get_content(&conn, &hash)
            .expect("get should succeed")
            .expect("content should exist");
        assert_eq!(retrieved, content);
    }

    #[test]
    fn test_content_deduplication() {
        let conn = setup_db();
        let content = "Same content";

        let hash1 = store_content(&conn, content).unwrap();
        let hash2 = store_content(&conn, content).unwrap();

        // Same content should produce same hash
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_get_nonexistent_content() {
        let conn = setup_db();

        let result = get_content(&conn, "nonexistent_hash").expect("get should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn test_compute_hash_consistency() {
        let content = "test content";
        let hash1 = compute_hash(content);
        let hash2 = compute_hash(content);
        assert_eq!(hash1, hash2);
    }

    // ==================== Document Tests ====================

    #[test]
    fn test_index_and_get_document() {
        let conn = setup_db_with_collection("docs");
        let doc = make_doc("docs", "readme.md", Some("README"));
        let content = "# README\n\nProject documentation.";

        let id = index_document(&conn, &doc, content).expect("index should succeed");
        assert!(id > 0);

        let retrieved = get_document(&conn, id)
            .expect("get should succeed")
            .expect("document should exist");

        assert_eq!(retrieved.collection, "docs");
        assert_eq!(retrieved.relative_path, "readme.md");
        assert_eq!(retrieved.title, Some("README".to_string()));
        assert!(!retrieved.hash.is_empty());
    }

    #[test]
    fn test_index_updates_existing_document() {
        let conn = setup_db_with_collection("docs");
        let doc = make_doc("docs", "readme.md", Some("Version 1"));
        let content1 = "# Version 1";

        let id1 = index_document(&conn, &doc, content1).unwrap();

        // Index again with updated content
        let mut doc2 = doc.clone();
        doc2.title = Some("Version 2".to_string());
        let content2 = "# Version 2";

        let id2 = index_document(&conn, &doc2, content2).unwrap();

        // Should have same ID (updated, not inserted)
        assert_eq!(id1, id2);

        let retrieved = get_document(&conn, id1).unwrap().unwrap();
        assert_eq!(retrieved.title, Some("Version 2".to_string()));
    }

    #[test]
    fn test_get_document_by_path() {
        let conn = setup_db_with_collection("docs");
        let doc = make_doc("docs", "readme.md", Some("README"));

        index_document(&conn, &doc, "content").unwrap();

        let retrieved = get_document_by_path(&conn, "docs", "readme.md")
            .expect("get should succeed")
            .expect("document should exist");

        assert_eq!(retrieved.relative_path, "readme.md");
    }

    #[test]
    fn test_get_nonexistent_document() {
        let conn = setup_db();

        let result = get_document(&conn, 99999).expect("get should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_document() {
        let conn = setup_db_with_collection("docs");
        let doc = make_doc("docs", "readme.md", None);

        let id = index_document(&conn, &doc, "content").unwrap();

        let deleted = delete_document(&conn, id).expect("delete should succeed");
        assert!(deleted);

        let result = get_document(&conn, id).expect("get should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn test_delete_nonexistent_document() {
        let conn = setup_db();

        let deleted = delete_document(&conn, 99999).expect("delete should succeed");
        assert!(!deleted);
    }

    #[test]
    fn test_list_documents() {
        let conn = setup_db_with_collection("docs");

        for i in 0..3 {
            let doc = make_doc("docs", &format!("doc{i}.md"), Some(&format!("Doc {i}")));
            index_document(&conn, &doc, &format!("content {i}")).unwrap();
        }

        let docs = list_documents(&conn, "docs").expect("list should succeed");
        assert_eq!(docs.len(), 3);
    }

    #[test]
    fn test_list_documents_empty_collection() {
        let conn = setup_db_with_collection("empty");

        let docs = list_documents(&conn, "empty").expect("list should succeed");
        assert!(docs.is_empty());
    }

    #[test]
    fn test_list_documents_filters_by_collection() {
        let conn = setup_db_with_collection("docs");

        // Add another collection
        let now = Utc::now().timestamp();
        add_collection(
            &conn,
            &Collection {
                name: "notes".to_string(),
                path: "./notes".to_string(),
                pattern: "**/*.md".to_string(),
                source: "manual".to_string(),
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        index_document(&conn, &make_doc("docs", "doc.md", None), "docs content").unwrap();
        index_document(&conn, &make_doc("notes", "note.md", None), "notes content").unwrap();

        let docs = list_documents(&conn, "docs").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].collection, "docs");
    }
}
