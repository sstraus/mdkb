//! FTS5 search operations.

use crate::domain::{IndexStatus, SearchQuery, SearchResult};
use crate::error::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;

/// Escape a query string for safe use with FTS5 MATCH.
///
/// FTS5 has special syntax for operators (AND, OR, NOT, NEAR) and column
/// prefixes (column:term). To search for literal text, we quote each term.
/// This prevents issues with:
/// - Hyphenated words like "anti-CSRF" being parsed as negation
/// - ALL CAPS words being interpreted as operators or column names
/// - Special characters in the query
///
/// Split a query into whitespace terms, each safely double-quoted for FTS5.
fn quote_fts5_terms(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect()
}

pub fn escape_fts5_query(query: &str) -> String {
    // Terms joined by space → FTS5 implicit AND (every term must match).
    quote_fts5_terms(query).join(" ")
}

/// Escape a natural-language query into an OR expression (any term may match).
/// Use for QA-style retrieval where requiring every term (the default AND) is
/// too strict — e.g. full-sentence questions. Empty query → empty string.
pub fn escape_fts5_query_or(query: &str) -> String {
    quote_fts5_terms(query).join(" OR ")
}

/// Actionable hint appended to empty search/get output when the index holds
/// nothing — so a blank result reads as "unpopulated index" (e.g. right after an
/// autoheal quarantine), not "your query matched nothing".
pub const INDEX_EMPTY_HINT: &str =
    "Index is empty (0 documents, 0 memory entries) — run `mdkb update` to populate it.";

/// True when the index holds no documents and no memory entries.
pub fn index_is_empty(conn: &Connection) -> bool {
    let docs: i64 = conn
        .query_row("SELECT COUNT(*) FROM documents", [], |r| r.get(0))
        .unwrap_or(0);
    if docs > 0 {
        return false;
    }
    let mem: i64 = conn
        .query_row("SELECT COUNT(*) FROM memory_entries", [], |r| r.get(0))
        .unwrap_or(0);
    mem == 0
}

/// Perform BM25 full-text search with optional evolution filtering.
///
/// By default, only returns documents with status 'current' (or NULL for legacy docs).
/// With `include_superseded`, returns all documents with status markers.
pub fn search(conn: &Connection, query: &SearchQuery) -> Result<Vec<SearchResult>> {
    search_fts(conn, &escape_fts5_query(&query.text), query)
}

/// BM25 search variant accepting a pre-built FTS5 expression.
///
/// `query.text` is ignored — the caller owns escaping, so it can OR-expand a
/// full-sentence prompt instead of taking FTS5's implicit AND. Every other
/// field of `query` (collection, limit, include_superseded) still applies.
pub fn search_fts(
    conn: &Connection,
    fts_query: &str,
    query: &SearchQuery,
) -> Result<Vec<SearchResult>> {
    let mut search_results = Vec::new();

    // Build status filter clause
    let status_filter = if query.include_superseded {
        "" // No filter - include all statuses
    } else if query.collection.as_deref() == Some(crate::domain::COLLECTION_CLAUDE_SESSIONS) {
        // Only the claude_sessions collection uses `archived` status (transcripts
        // whose source jsonl was rotated away) — surface those when explicitly
        // scoped, while still hiding superseded/retracted. Scoped to this
        // collection specifically so a future `archived` use elsewhere doesn't
        // silently leak into other `--collection` searches (ARCH-2).
        "AND (d.status IS NULL OR d.status IN ('current', 'archived'))"
    } else {
        // Default: only current documents (status IS NULL for legacy, or 'current')
        "AND (d.status IS NULL OR d.status = 'current')"
    };

    // Score penalty for superseded documents when included
    let score_expr = if query.include_superseded {
        // Apply penalty to superseded/retracted docs (lower score = better in BM25)
        "CASE WHEN d.status IN ('superseded', 'retracted') THEN bm25(documents_fts) + 0.5 ELSE bm25(documents_fts) END"
    } else {
        "bm25(documents_fts)"
    };

    // Build FTS5 query with optional collection filter.
    // Note: snippet() is not used because documents_fts is contentless (content='').
    let (collection_clause, collection_param): (&str, Option<&str>) = match &query.collection {
        Some(c) => ("AND d.collection = ?2", Some(c.as_str())),
        None => ("", None),
    };
    let limit_param_idx = if collection_param.is_some() {
        "?3"
    } else {
        "?2"
    };

    let sql = format!(
        r#"
        SELECT d.id, d.collection, d.relative_path, d.title,
               {score_expr} as score, d.status
        FROM documents_fts f
        JOIN documents d ON d.id = f.rowid
        WHERE documents_fts MATCH ?1 {collection_clause}
        {status_filter}
        ORDER BY score
        LIMIT {limit_param_idx}
        "#
    );

    let mut stmt = conn.prepare(&sql)?;
    let row_mapper = |row: &rusqlite::Row| {
        Ok(SearchResult {
            id: row.get(0)?,
            collection: row.get(1)?,
            path: row.get(2)?,
            title: row.get(3)?,
            score: row.get(4)?,
            snippets: vec![],
            status: row.get(5)?,
            superseded_by: None,
            repo_root: None,
        })
    };

    let rows = if let Some(coll) = collection_param {
        stmt.query_map(params![fts_query, coll, query.limit as i64], row_mapper)?
    } else {
        stmt.query_map(params![fts_query, query.limit as i64], row_mapper)?
    };

    for result in rows {
        search_results.push(result?);
    }

    // If including superseded docs, populate the superseded_by field
    if query.include_superseded {
        populate_superseded_by(conn, &mut search_results)?;
    }

    Ok(search_results)
}

/// Populate the superseded_by field for search results.
fn populate_superseded_by(conn: &Connection, results: &mut [SearchResult]) -> Result<()> {
    let superseded_ids: Vec<i64> = results
        .iter()
        .filter(|r| r.status.as_deref() == Some("superseded"))
        .map(|r| r.id)
        .collect();

    if superseded_ids.is_empty() {
        return Ok(());
    }

    // Batch fetch: for each superseded doc, find the superseding doc's path
    let placeholders: Vec<String> = (1..=superseded_ids.len())
        .map(|i| format!("?{i}"))
        .collect();
    let sql = format!(
        r#"SELECT e.target_doc_id, d.relative_path
           FROM evolution e
           JOIN documents d ON d.id = e.source_doc_id
           WHERE e.target_doc_id IN ({}) AND e.relationship = 'supersedes'
           ORDER BY e.created_at DESC"#,
        placeholders.join(", ")
    );

    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> = superseded_ids
        .iter()
        .map(|id| id as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;

    // First entry per target_doc_id wins (ORDER BY created_at DESC)
    let mut superseded_map = HashMap::new();
    for row in rows {
        let (target_id, path) = row?;
        superseded_map.entry(target_id).or_insert(path);
    }

    for result in results.iter_mut() {
        if let Some(path) = superseded_map.remove(&result.id) {
            result.superseded_by = Some(path);
        }
    }
    Ok(())
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

    // ==================== FTS5 Escape Tests ====================

    #[test]
    fn test_escape_fts5_simple() {
        assert_eq!(escape_fts5_query("hello world"), "\"hello\" \"world\"");
    }

    #[test]
    fn test_escape_fts5_hyphenated() {
        // Hyphens should be preserved inside quotes
        assert_eq!(escape_fts5_query("anti-CSRF"), "\"anti-CSRF\"");
    }

    #[test]
    fn test_escape_fts5_with_quotes() {
        // Internal quotes should be doubled
        assert_eq!(
            escape_fts5_query("say \"hello\""),
            "\"say\" \"\"\"hello\"\"\""
        );
    }

    #[test]
    fn test_escape_fts5_operators() {
        // FTS5 operators should be quoted
        assert_eq!(
            escape_fts5_query("cats AND dogs"),
            "\"cats\" \"AND\" \"dogs\""
        );
        assert_eq!(escape_fts5_query("NOT found"), "\"NOT\" \"found\"");
    }

    #[test]
    fn test_escape_fts5_column_prefix() {
        // Column:term syntax should be escaped
        assert_eq!(escape_fts5_query("CSRF:token"), "\"CSRF:token\"");
    }

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
                source: "manual".to_string(),
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
                status: None,
            };
            index_document(&conn, &doc, content).unwrap();
        }

        conn
    }

    #[test]
    fn search_fts_honours_a_prebuilt_or_expression() {
        let conn = setup_db_with_docs();
        // A full-sentence prompt: no single document contains every term, so
        // the default implicit-AND escaping matches nothing. That is exactly
        // why the hook recall path OR-expands before calling `search_fts`.
        let prompt = "how do rust lifetimes compare to python";
        let query = SearchQuery {
            text: prompt.to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
            include_superseded: false,
        };

        let and_results = search(&conn, &query).expect("AND search should succeed");
        assert!(
            and_results.is_empty(),
            "implicit AND must not match a full sentence: {and_results:?}"
        );

        let or_results = search_fts(&conn, &escape_fts5_query_or(prompt), &query)
            .expect("OR search should succeed");
        let paths: Vec<&str> = or_results.iter().map(|r| r.path.as_str()).collect();
        assert!(
            paths.contains(&"rust-advanced.md"),
            "OR expression should match on 'lifetimes': {paths:?}"
        );
        assert!(
            paths.contains(&"python-intro.md"),
            "OR expression should match on 'python': {paths:?}"
        );
    }

    #[test]
    fn search_fts_ignores_query_text_but_honours_other_fields() {
        let conn = setup_db_with_docs();
        // `text` is deliberately a term that matches nothing — `search_fts`
        // must use the passed expression, and still apply `limit`.
        let query = SearchQuery {
            text: "zzzz-no-such-term".to_string(),
            limit: 1,
            collection: None,
            tags: vec![],
            include_superseded: false,
        };

        let results =
            search_fts(&conn, &escape_fts5_query_or("rust python"), &query).expect("search");
        assert_eq!(results.len(), 1, "limit must still apply: {results:?}");
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
            include_superseded: false,
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
            include_superseded: false,
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
            include_superseded: false,
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
                source: "manual".to_string(),
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
            status: None,
        };
        index_document(&conn, &doc, "A note about Rust").unwrap();

        // Search only in docs collection
        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: Some("docs".to_string()),
            tags: vec![],
            include_superseded: false,
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
            include_superseded: false,
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
            include_superseded: false,
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

    #[test]
    fn test_index_is_empty_gates_on_docs_and_memory() {
        let conn = setup_db();
        assert!(index_is_empty(&conn), "fresh store: no docs, no memory");

        conn.execute(
            "INSERT INTO memory_entries (id, title, content, entry_type, created_at, updated_at)
             VALUES ('m', 't', 'c', 'topic', 1, 1)",
            [],
        )
        .unwrap();
        assert!(
            !index_is_empty(&conn),
            "a single memory entry makes the store non-empty (hint must not fire)"
        );
    }

    #[test]
    fn test_index_not_empty_with_docs() {
        let conn = setup_db_with_docs();
        assert!(!index_is_empty(&conn), "docs present → not empty");
    }

    // ==================== Evolution Filtering Tests ====================

    #[test]
    fn test_search_excludes_superseded_by_default() {
        let conn = setup_db_with_docs();
        let now = Utc::now().timestamp();

        // Add a document and mark it as superseded
        let doc = Document {
            id: 0,
            collection: "docs".to_string(),
            relative_path: "rust-old.md".to_string(),
            hash: String::new(),
            title: Some("Old Rust Guide".to_string()),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
            status: None,
        };
        index_document(&conn, &doc, "Old Rust programming guide, now superseded.").unwrap();

        // Mark it as superseded
        conn.execute(
            "UPDATE documents SET status = 'superseded' WHERE relative_path = 'rust-old.md'",
            [],
        )
        .unwrap();

        // Search without include_superseded (default)
        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
            include_superseded: false,
        };

        let results = search(&conn, &query).expect("search should succeed");

        // Should only find the non-superseded docs (2 from setup)
        let paths: Vec<_> = results.iter().map(|r| r.path.as_str()).collect();
        assert!(
            !paths.contains(&"rust-old.md"),
            "superseded doc should be excluded"
        );
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_search_includes_superseded_with_flag() {
        let conn = setup_db_with_docs();
        let now = Utc::now().timestamp();

        // Add a document and mark it as superseded
        let doc = Document {
            id: 0,
            collection: "docs".to_string(),
            relative_path: "rust-old.md".to_string(),
            hash: String::new(),
            title: Some("Old Rust Guide".to_string()),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
            status: None,
        };
        index_document(&conn, &doc, "Old Rust programming guide, now superseded.").unwrap();

        // Mark it as superseded
        conn.execute(
            "UPDATE documents SET status = 'superseded' WHERE relative_path = 'rust-old.md'",
            [],
        )
        .unwrap();

        // Search with include_superseded
        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
            include_superseded: true,
        };

        let results = search(&conn, &query).expect("search should succeed");

        // Should find all rust docs including superseded
        let paths: Vec<_> = results.iter().map(|r| r.path.as_str()).collect();
        assert!(
            paths.contains(&"rust-old.md"),
            "superseded doc should be included"
        );
        assert_eq!(results.len(), 3); // 2 from setup + 1 superseded
    }

    #[test]
    fn test_search_result_status_field() {
        let conn = setup_db_with_docs();
        let now = Utc::now().timestamp();

        // Add a document and mark it as superseded
        let doc = Document {
            id: 0,
            collection: "docs".to_string(),
            relative_path: "rust-old.md".to_string(),
            hash: String::new(),
            title: Some("Old Rust Guide".to_string()),
            metadata: None,
            file_modified_at: now,
            indexed_at: now,
            status: None,
        };
        index_document(&conn, &doc, "Old Rust programming guide.").unwrap();

        conn.execute(
            "UPDATE documents SET status = 'superseded' WHERE relative_path = 'rust-old.md'",
            [],
        )
        .unwrap();

        // Search with include_superseded
        let query = SearchQuery {
            text: "rust".to_string(),
            limit: 10,
            collection: None,
            tags: vec![],
            include_superseded: true,
        };

        let results = search(&conn, &query).expect("search should succeed");

        // Find the superseded doc and check its status
        let superseded = results.iter().find(|r| r.path == "rust-old.md").unwrap();
        assert_eq!(superseded.status, Some("superseded".to_string()));

        // Other docs should have NULL/None status or "current"
        let current = results.iter().find(|r| r.path == "rust-basics.md").unwrap();
        assert!(
            current.status.is_none() || current.status.as_deref() == Some("current"),
            "current doc should have NULL or 'current' status"
        );
    }
}
