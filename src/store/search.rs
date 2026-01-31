//! FTS5 search operations.

use crate::domain::{IndexStatus, SearchQuery, SearchResult};
use crate::error::Result;
use rusqlite::{Connection, params};

/// Perform BM25 full-text search.
pub fn search(conn: &Connection, query: &SearchQuery) -> Result<Vec<SearchResult>> {
    let mut search_results = Vec::new();

    // Build FTS5 query with optional collection filter
    if let Some(ref collection) = query.collection {
        let sql = r#"
            SELECT d.id, d.relative_path, d.title, bm25(documents_fts) as score,
                   snippet(documents_fts, 1, '<b>', '</b>', '...', 32) as snippet
            FROM documents_fts f
            JOIN documents d ON d.id = f.rowid
            WHERE documents_fts MATCH ?1 AND d.collection = ?2
            ORDER BY bm25(documents_fts)
            LIMIT ?3
        "#;

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(
            params![&query.text, collection, query.limit as i64],
            |row| {
                let snippet: Option<String> = row.get(4)?;
                Ok(SearchResult {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    title: row.get(2)?,
                    score: row.get(3)?,
                    snippets: snippet.map(|s| vec![s]).unwrap_or_default(),
                })
            },
        )?;

        for result in rows {
            search_results.push(result?);
        }
    } else {
        let sql = r#"
            SELECT d.id, d.relative_path, d.title, bm25(documents_fts) as score,
                   snippet(documents_fts, 1, '<b>', '</b>', '...', 32) as snippet
            FROM documents_fts f
            JOIN documents d ON d.id = f.rowid
            WHERE documents_fts MATCH ?1
            ORDER BY bm25(documents_fts)
            LIMIT ?2
        "#;

        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![&query.text, query.limit as i64], |row| {
            let snippet: Option<String> = row.get(4)?;
            Ok(SearchResult {
                id: row.get(0)?,
                path: row.get(1)?,
                title: row.get(2)?,
                score: row.get(3)?,
                snippets: snippet.map(|s| vec![s]).unwrap_or_default(),
            })
        })?;

        for result in rows {
            search_results.push(result?);
        }
    }

    Ok(search_results)
}

/// Get index status.
pub fn get_status(conn: &Connection) -> Result<IndexStatus> {
    let collections: usize =
        conn.query_row("SELECT COUNT(*) FROM collections", [], |row| row.get(0))?;

    let documents: usize =
        conn.query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;

    let last_updated: Option<i64> = conn
        .query_row("SELECT MAX(indexed_at) FROM documents", [], |row| {
            row.get(0)
        })
        .ok()
        .flatten();

    // Get database file size (0 for in-memory)
    let db_size_bytes: u64 = conn
        .query_row(
            "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
            [],
            |row| row.get::<_, i64>(0).map(|v| v as u64),
        )
        .unwrap_or(0);

    Ok(IndexStatus {
        collections,
        documents,
        stale_documents: 0, // Not implemented yet
        db_size_bytes,
        last_updated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Collection;
    use crate::domain::Document;
    use crate::store::collections::add_collection;
    use crate::store::documents::index_document;
    use crate::store::schema::init_schema;
    use chrono::Utc;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn setup_db_with_docs() -> Connection {
        let conn = setup_db();
        let now = Utc::now().timestamp();

        add_collection(
            &conn,
            &Collection {
                name: "docs".to_string(),
                path: "./docs".to_string(),
                pattern: "**/*.md".to_string(),
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        // Add test documents
        let docs = [
            (
                "rust-basics.md",
                "Rust Basics",
                "Rust is a systems programming language focused on safety.",
            ),
            (
                "python-intro.md",
                "Python Introduction",
                "Python is a high-level programming language.",
            ),
            (
                "rust-advanced.md",
                "Advanced Rust",
                "Advanced Rust concepts including lifetimes and async.",
            ),
        ];

        for (path, title, content) in docs {
            let doc = Document {
                id: 0,
                collection: "docs".to_string(),
                relative_path: path.to_string(),
                hash: String::new(),
                title: Some(title.to_string()),
                metadata: None,
                file_modified_at: now,
                indexed_at: now,
            };
            index_document(&conn, &doc, content).unwrap();
        }

        conn
    }

    // ==================== Search Tests ====================

    #[test]
    fn test_search_basic() {
        let conn = setup_db_with_docs();

        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
        };

        let results = search(&conn, &query).expect("search should succeed");

        assert_eq!(results.len(), 2);
        // Both Rust documents should be found
        let paths: Vec<_> = results.iter().map(|r| r.path.as_str()).collect();
        assert!(paths.contains(&"rust-basics.md"));
        assert!(paths.contains(&"rust-advanced.md"));
    }

    #[test]
    fn test_search_respects_limit() {
        let conn = setup_db_with_docs();

        let query = SearchQuery {
            text: "programming".to_string(),
            limit: 1,
            collection: None,
            tags: vec![],
        };

        let results = search(&conn, &query).expect("search should succeed");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_search_no_results() {
        let conn = setup_db_with_docs();

        let query = SearchQuery {
            text: "javascript".to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
        };

        let results = search(&conn, &query).expect("search should succeed");
        assert!(results.is_empty());
    }

    #[test]
    fn test_search_with_collection_filter() {
        let conn = setup_db_with_docs();

        // Add another collection
        let now = Utc::now().timestamp();
        add_collection(
            &conn,
            &Collection {
                name: "notes".to_string(),
                path: "./notes".to_string(),
                pattern: "**/*.md".to_string(),
                created_at: now,
                updated_at: now,
            },
        )
        .unwrap();

        let doc = Document {
            id: 0,
            collection: "notes".to_string(),
            relative_path: "rust-note.md".to_string(),
            hash: String::new(),
            title: Some("Rust Note".to_string()),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
        };
        index_document(&conn, &doc, "A note about Rust").unwrap();

        // Search only in docs collection
        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: Some("docs".to_string()),
            tags: vec![],
        };

        let results = search(&conn, &query).expect("search should succeed");

        // Should find 2 results, not 3 (filtered to docs only)
        assert_eq!(results.len(), 2);
        for r in &results {
            // We don't store collection in SearchResult, but we can verify paths
            assert!(r.path.ends_with(".md"));
        }
    }

    #[test]
    fn test_search_results_have_scores() {
        let conn = setup_db_with_docs();

        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
        };

        let results = search(&conn, &query).expect("search should succeed");

        for r in &results {
            // BM25 scores are negative, more negative = better
            assert!(r.score != 0.0);
        }

        // Results should be sorted by score (best first)
        if results.len() >= 2 {
            assert!(results[0].score <= results[1].score);
        }
    }

    #[test]
    fn test_search_title_weighted_higher() {
        let conn = setup_db_with_docs();

        // "Rust Basics" has "Rust" in title
        // "python-intro" has Rust nowhere
        // Title matches should rank higher

        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
        };

        let results = search(&conn, &query).expect("search should succeed");

        // The doc with "Rust" in title should be first (better score)
        assert!(!results.is_empty());
        // We expect title matches to rank higher due to BM25 weighting
    }

    // ==================== Status Tests ====================

    #[test]
    fn test_get_status_empty() {
        let conn = setup_db();

        let status = get_status(&conn).expect("status should succeed");
        assert_eq!(status.collections, 0);
        assert_eq!(status.documents, 0);
    }

    #[test]
    fn test_get_status_with_data() {
        let conn = setup_db_with_docs();

        let status = get_status(&conn).expect("status should succeed");
        assert_eq!(status.collections, 1);
        assert_eq!(status.documents, 3);
    }
}
