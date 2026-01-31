//! Usage statistics persistence.
//!
//! Stores MCP tool usage statistics in SQLite for analysis and debugging.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Initialize the statistics schema.
pub fn init_stats_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Session tracking
        CREATE TABLE IF NOT EXISTS sessions (
            id INTEGER PRIMARY KEY,
            started_at INTEGER NOT NULL,
            ended_at INTEGER,
            total_calls INTEGER DEFAULT 0,
            total_tokens INTEGER DEFAULT 0,
            truncation_count INTEGER DEFAULT 0
        );

        -- Per-tool usage within a session
        CREATE TABLE IF NOT EXISTS tool_usage (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL,
            tool_name TEXT NOT NULL,
            call_count INTEGER DEFAULT 0,
            total_tokens INTEGER DEFAULT 0,
            total_results INTEGER DEFAULT 0,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
            UNIQUE(session_id, tool_name)
        );

        -- Individual call log (optional, for detailed debugging)
        CREATE TABLE IF NOT EXISTS call_log (
            id INTEGER PRIMARY KEY,
            session_id INTEGER NOT NULL,
            tool_name TEXT NOT NULL,
            tokens INTEGER NOT NULL,
            results INTEGER NOT NULL,
            truncated INTEGER NOT NULL DEFAULT 0,
            called_at INTEGER NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE
        );

        -- Query events for latency/quality analysis
        CREATE TABLE IF NOT EXISTS query_events (
            id INTEGER PRIMARY KEY,
            query_hash TEXT NOT NULL,       -- SHA256 of normalized query
            query_text TEXT NOT NULL,       -- Original query text
            search_type TEXT NOT NULL,      -- bm25, semantic, hybrid
            result_count INTEGER NOT NULL,
            latency_ms INTEGER NOT NULL,
            top_score REAL,
            session_id INTEGER,
            created_at INTEGER NOT NULL
        );

        -- Indexes
        CREATE INDEX IF NOT EXISTS idx_tool_usage_session ON tool_usage(session_id);
        CREATE INDEX IF NOT EXISTS idx_call_log_session ON call_log(session_id);
        CREATE INDEX IF NOT EXISTS idx_call_log_tool ON call_log(tool_name);
        CREATE INDEX IF NOT EXISTS idx_query_events_hash ON query_events(query_hash);
        CREATE INDEX IF NOT EXISTS idx_query_events_type ON query_events(search_type);
        CREATE INDEX IF NOT EXISTS idx_query_events_session ON query_events(session_id);
        "#,
    )?;
    Ok(())
}

/// Session record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: i64,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub total_calls: i64,
    pub total_tokens: i64,
    pub truncation_count: i64,
}

/// Tool usage record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolUsageRecord {
    pub tool_name: String,
    pub call_count: i64,
    pub total_tokens: i64,
    pub total_results: i64,
}

/// Create a new session and return its ID.
pub fn create_session(conn: &Connection) -> Result<i64> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO sessions (started_at) VALUES (?1)",
        params![now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// End a session by setting its ended_at timestamp.
pub fn end_session(conn: &Connection, session_id: i64) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
        params![now, session_id],
    )?;
    Ok(())
}

/// Record a tool call.
pub fn record_call(
    conn: &Connection,
    session_id: i64,
    tool_name: &str,
    tokens: usize,
    results: usize,
    truncated: bool,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();

    // Update session totals
    conn.execute(
        r#"
        UPDATE sessions
        SET total_calls = total_calls + 1,
            total_tokens = total_tokens + ?1,
            truncation_count = truncation_count + ?2
        WHERE id = ?3
        "#,
        params![tokens as i64, i32::from(truncated), session_id],
    )?;

    // Upsert tool usage
    conn.execute(
        r#"
        INSERT INTO tool_usage (session_id, tool_name, call_count, total_tokens, total_results)
        VALUES (?1, ?2, 1, ?3, ?4)
        ON CONFLICT(session_id, tool_name) DO UPDATE SET
            call_count = call_count + 1,
            total_tokens = total_tokens + excluded.total_tokens,
            total_results = total_results + excluded.total_results
        "#,
        params![session_id, tool_name, tokens as i64, results as i64],
    )?;

    // Log individual call
    conn.execute(
        r#"
        INSERT INTO call_log (session_id, tool_name, tokens, results, truncated, called_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        "#,
        params![
            session_id,
            tool_name,
            tokens as i64,
            results as i64,
            i32::from(truncated),
            now
        ],
    )?;

    Ok(())
}

/// Get the current session (most recent active session).
pub fn get_current_session(conn: &Connection) -> Result<Option<Session>> {
    let result = conn
        .query_row(
            r#"
            SELECT id, started_at, ended_at, total_calls, total_tokens, truncation_count
            FROM sessions
            WHERE ended_at IS NULL
            ORDER BY id DESC
            LIMIT 1
            "#,
            [],
            |row| {
                Ok(Session {
                    id: row.get(0)?,
                    started_at: row.get(1)?,
                    ended_at: row.get(2)?,
                    total_calls: row.get(3)?,
                    total_tokens: row.get(4)?,
                    truncation_count: row.get(5)?,
                })
            },
        )
        .ok();
    Ok(result)
}

/// Get a session by ID.
pub fn get_session(conn: &Connection, session_id: i64) -> Result<Option<Session>> {
    let result = conn
        .query_row(
            r#"
            SELECT id, started_at, ended_at, total_calls, total_tokens, truncation_count
            FROM sessions
            WHERE id = ?1
            "#,
            params![session_id],
            |row| {
                Ok(Session {
                    id: row.get(0)?,
                    started_at: row.get(1)?,
                    ended_at: row.get(2)?,
                    total_calls: row.get(3)?,
                    total_tokens: row.get(4)?,
                    truncation_count: row.get(5)?,
                })
            },
        )
        .ok();
    Ok(result)
}

/// Get tool usage for a session.
pub fn get_tool_usage(conn: &Connection, session_id: i64) -> Result<Vec<ToolUsageRecord>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT tool_name, call_count, total_tokens, total_results
        FROM tool_usage
        WHERE session_id = ?1
        ORDER BY total_tokens DESC
        "#,
    )?;

    let results = stmt
        .query_map(params![session_id], |row| {
            Ok(ToolUsageRecord {
                tool_name: row.get(0)?,
                call_count: row.get(1)?,
                total_tokens: row.get(2)?,
                total_results: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}

/// Get aggregate statistics across all sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateStats {
    pub total_sessions: i64,
    pub total_calls: i64,
    pub total_tokens: i64,
    pub total_truncations: i64,
    pub avg_tokens_per_call: f64,
}

pub fn get_aggregate_stats(conn: &Connection) -> Result<AggregateStats> {
    let result = conn.query_row(
        r#"
        SELECT
            COUNT(*) as sessions,
            COALESCE(SUM(total_calls), 0) as calls,
            COALESCE(SUM(total_tokens), 0) as tokens,
            COALESCE(SUM(truncation_count), 0) as truncations
        FROM sessions
        "#,
        [],
        |row| {
            let sessions: i64 = row.get(0)?;
            let calls: i64 = row.get(1)?;
            let tokens: i64 = row.get(2)?;
            let truncations: i64 = row.get(3)?;
            Ok(AggregateStats {
                total_sessions: sessions,
                total_calls: calls,
                total_tokens: tokens,
                total_truncations: truncations,
                avg_tokens_per_call: if calls > 0 {
                    tokens as f64 / calls as f64
                } else {
                    0.0
                },
            })
        },
    )?;

    Ok(result)
}

/// Get recent sessions with their stats.
pub fn get_recent_sessions(conn: &Connection, limit: usize) -> Result<Vec<Session>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, started_at, ended_at, total_calls, total_tokens, truncation_count
        FROM sessions
        ORDER BY id DESC
        LIMIT ?1
        "#,
    )?;

    let results = stmt
        .query_map(params![limit as i64], |row| {
            Ok(Session {
                id: row.get(0)?,
                started_at: row.get(1)?,
                ended_at: row.get(2)?,
                total_calls: row.get(3)?,
                total_tokens: row.get(4)?,
                truncation_count: row.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}

/// Get aggregate tool usage across all sessions.
pub fn get_aggregate_tool_usage(conn: &Connection) -> Result<Vec<ToolUsageRecord>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            tool_name,
            SUM(call_count) as calls,
            SUM(total_tokens) as tokens,
            SUM(total_results) as results
        FROM tool_usage
        GROUP BY tool_name
        ORDER BY calls DESC
        "#,
    )?;

    let results = stmt
        .query_map([], |row| {
            Ok(ToolUsageRecord {
                tool_name: row.get(0)?,
                call_count: row.get(1)?,
                total_tokens: row.get(2)?,
                total_results: row.get(3)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}

// ==================== Query Events ====================

/// Query event for latency and quality analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryEvent {
    pub query_hash: String,
    pub query_text: String,
    pub search_type: String,
    pub result_count: i64,
    pub latency_ms: i64,
    pub top_score: Option<f64>,
    pub session_id: Option<i64>,
}

/// Compute hash of normalized query for de-duplication.
pub fn hash_query(query: &str) -> String {
    use sha2::{Digest, Sha256};

    // Normalize: lowercase, trim, collapse whitespace
    let normalized = query
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    let result = hasher.finalize();
    format!("{:x}", result)
}

/// Record a query event.
pub fn record_query_event(conn: &Connection, event: &QueryEvent) -> Result<i64> {
    let now = chrono::Utc::now().timestamp();

    conn.execute(
        r#"
        INSERT INTO query_events (query_hash, query_text, search_type, result_count, latency_ms, top_score, session_id, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        "#,
        params![
            event.query_hash,
            event.query_text,
            event.search_type,
            event.result_count,
            event.latency_ms,
            event.top_score,
            event.session_id,
            now,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Get query latency statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryLatencyStats {
    pub search_type: String,
    pub count: i64,
    pub avg_latency_ms: f64,
    pub max_latency_ms: i64,
    pub zero_result_count: i64,
}

/// Get query latency statistics by search type.
pub fn get_query_latency_stats(conn: &Connection) -> Result<Vec<QueryLatencyStats>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT
            search_type,
            COUNT(*) as count,
            AVG(latency_ms) as avg_latency,
            MAX(latency_ms) as max_latency,
            SUM(CASE WHEN result_count = 0 THEN 1 ELSE 0 END) as zero_results
        FROM query_events
        GROUP BY search_type
        ORDER BY count DESC
        "#,
    )?;

    let results = stmt
        .query_map([], |row| {
            Ok(QueryLatencyStats {
                search_type: row.get(0)?,
                count: row.get(1)?,
                avg_latency_ms: row.get(2)?,
                max_latency_ms: row.get(3)?,
                zero_result_count: row.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}

/// Clear old sessions (keep last N).
pub fn prune_sessions(conn: &Connection, keep_count: usize) -> Result<usize> {
    // Get the ID threshold - we want to delete sessions older than the Nth most recent
    let threshold: Option<i64> = conn
        .query_row(
            r#"
            SELECT id FROM sessions
            ORDER BY id DESC
            LIMIT 1 OFFSET ?1
            "#,
            params![keep_count as i64],
            |row| row.get(0),
        )
        .ok();

    if let Some(threshold) = threshold {
        // Delete sessions with id <= threshold (including the threshold itself)
        let deleted = conn.execute(
            "DELETE FROM sessions WHERE id <= ?1",
            params![threshold],
        )?;
        Ok(deleted)
    } else {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_stats_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn test_create_and_get_session() {
        let conn = setup_db();

        let session_id = create_session(&conn).unwrap();
        assert!(session_id > 0);

        let session = get_session(&conn, session_id).unwrap().unwrap();
        assert_eq!(session.id, session_id);
        assert!(session.ended_at.is_none());
        assert_eq!(session.total_calls, 0);
    }

    #[test]
    fn test_record_call() {
        let conn = setup_db();

        let session_id = create_session(&conn).unwrap();
        record_call(&conn, session_id, "search", 100, 5, false).unwrap();
        record_call(&conn, session_id, "search", 200, 10, true).unwrap();
        record_call(&conn, session_id, "get", 500, 1, false).unwrap();

        let session = get_session(&conn, session_id).unwrap().unwrap();
        assert_eq!(session.total_calls, 3);
        assert_eq!(session.total_tokens, 800);
        assert_eq!(session.truncation_count, 1);

        let usage = get_tool_usage(&conn, session_id).unwrap();
        assert_eq!(usage.len(), 2);

        let search_usage = usage.iter().find(|u| u.tool_name == "search").unwrap();
        assert_eq!(search_usage.call_count, 2);
        assert_eq!(search_usage.total_tokens, 300);
        assert_eq!(search_usage.total_results, 15);
    }

    #[test]
    fn test_end_session() {
        let conn = setup_db();

        let session_id = create_session(&conn).unwrap();
        end_session(&conn, session_id).unwrap();

        let session = get_session(&conn, session_id).unwrap().unwrap();
        assert!(session.ended_at.is_some());
    }

    #[test]
    fn test_get_current_session() {
        let conn = setup_db();

        // No session yet
        assert!(get_current_session(&conn).unwrap().is_none());

        let session_id = create_session(&conn).unwrap();
        let current = get_current_session(&conn).unwrap().unwrap();
        assert_eq!(current.id, session_id);

        // End it
        end_session(&conn, session_id).unwrap();
        assert!(get_current_session(&conn).unwrap().is_none());
    }

    #[test]
    fn test_aggregate_stats() {
        let conn = setup_db();

        let s1 = create_session(&conn).unwrap();
        record_call(&conn, s1, "search", 100, 5, false).unwrap();
        end_session(&conn, s1).unwrap();

        let s2 = create_session(&conn).unwrap();
        record_call(&conn, s2, "get", 200, 1, true).unwrap();
        record_call(&conn, s2, "search", 150, 3, false).unwrap();

        let stats = get_aggregate_stats(&conn).unwrap();
        assert_eq!(stats.total_sessions, 2);
        assert_eq!(stats.total_calls, 3);
        assert_eq!(stats.total_tokens, 450);
        assert_eq!(stats.total_truncations, 1);
    }

    #[test]
    fn test_prune_sessions() {
        let conn = setup_db();

        for _ in 0..5 {
            let s = create_session(&conn).unwrap();
            record_call(&conn, s, "test", 10, 1, false).unwrap();
            end_session(&conn, s).unwrap();
        }

        let stats_before = get_aggregate_stats(&conn).unwrap();
        assert_eq!(stats_before.total_sessions, 5);

        let pruned = prune_sessions(&conn, 2).unwrap();
        assert_eq!(pruned, 3);

        let stats_after = get_aggregate_stats(&conn).unwrap();
        assert_eq!(stats_after.total_sessions, 2);
    }

    #[test]
    fn test_recent_sessions() {
        let conn = setup_db();

        for i in 0..5 {
            let s = create_session(&conn).unwrap();
            record_call(&conn, s, "test", (i + 1) * 100, 1, false).unwrap();
        }

        let recent = get_recent_sessions(&conn, 3).unwrap();
        assert_eq!(recent.len(), 3);
        // Most recent first
        assert!(recent[0].id > recent[1].id);
    }

    #[test]
    fn test_aggregate_tool_usage() {
        let conn = setup_db();

        // Create sessions with various tool usage including memory tools
        let s1 = create_session(&conn).unwrap();
        record_call(&conn, s1, "search", 100, 5, false).unwrap();
        record_call(&conn, s1, "memory_get", 50, 1, false).unwrap();
        record_call(&conn, s1, "memory_write", 30, 1, false).unwrap();
        end_session(&conn, s1).unwrap();

        let s2 = create_session(&conn).unwrap();
        record_call(&conn, s2, "search", 150, 3, false).unwrap();
        record_call(&conn, s2, "memory_get", 75, 1, false).unwrap();
        record_call(&conn, s2, "memory_search", 60, 2, false).unwrap();

        let tool_stats = get_aggregate_tool_usage(&conn).unwrap();

        // Verify we get all tools
        assert!(tool_stats.len() >= 4);

        // Verify search stats (should be highest)
        let search = tool_stats.iter().find(|t| t.tool_name == "search").unwrap();
        assert_eq!(search.call_count, 2);
        assert_eq!(search.total_tokens, 250);

        // Verify memory_get stats
        let memory_get = tool_stats.iter().find(|t| t.tool_name == "memory_get").unwrap();
        assert_eq!(memory_get.call_count, 2);
        assert_eq!(memory_get.total_tokens, 125);

        // Verify memory_write stats
        let memory_write = tool_stats.iter().find(|t| t.tool_name == "memory_write").unwrap();
        assert_eq!(memory_write.call_count, 1);
        assert_eq!(memory_write.total_tokens, 30);
    }

    // ==================== Query Event Tests ====================

    #[test]
    fn test_hash_query_normalization() {
        // Same query with different whitespace/case should hash the same
        let h1 = hash_query("  How do I   configure  AUTH  ");
        let h2 = hash_query("how do i configure auth");
        let h3 = hash_query("HOW DO I CONFIGURE AUTH");

        assert_eq!(h1, h2);
        assert_eq!(h2, h3);

        // Different queries should hash differently
        let h4 = hash_query("different query");
        assert_ne!(h1, h4);
    }

    #[test]
    fn test_record_query_event() {
        let conn = setup_db();

        let event = QueryEvent {
            query_hash: hash_query("test query"),
            query_text: "test query".to_string(),
            search_type: "hybrid".to_string(),
            result_count: 5,
            latency_ms: 42,
            top_score: Some(0.85),
            session_id: None,
        };

        let id = record_query_event(&conn, &event).unwrap();
        assert!(id > 0);
    }

    #[test]
    fn test_query_latency_stats() {
        let conn = setup_db();

        // Record several query events of different types
        for i in 0..5 {
            let event = QueryEvent {
                query_hash: hash_query(&format!("query {i}")),
                query_text: format!("query {i}"),
                search_type: "hybrid".to_string(),
                result_count: i as i64,
                latency_ms: 10 + i as i64 * 5,
                top_score: Some(0.8),
                session_id: None,
            };
            record_query_event(&conn, &event).unwrap();
        }

        for i in 0..3 {
            let event = QueryEvent {
                query_hash: hash_query(&format!("bm25 query {i}")),
                query_text: format!("bm25 query {i}"),
                search_type: "bm25".to_string(),
                result_count: 1,
                latency_ms: 5 + i as i64 * 2,
                top_score: Some(0.9),
                session_id: None,
            };
            record_query_event(&conn, &event).unwrap();
        }

        let stats = get_query_latency_stats(&conn).unwrap();

        // Should have stats for both types
        assert_eq!(stats.len(), 2);

        let hybrid = stats.iter().find(|s| s.search_type == "hybrid").unwrap();
        assert_eq!(hybrid.count, 5);
        assert!(hybrid.avg_latency_ms > 0.0);
        assert_eq!(hybrid.zero_result_count, 1); // query 0 had 0 results

        let bm25 = stats.iter().find(|s| s.search_type == "bm25").unwrap();
        assert_eq!(bm25.count, 3);
    }
}
