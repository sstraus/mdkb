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

        -- Indexes
        CREATE INDEX IF NOT EXISTS idx_tool_usage_session ON tool_usage(session_id);
        CREATE INDEX IF NOT EXISTS idx_call_log_session ON call_log(session_id);
        CREATE INDEX IF NOT EXISTS idx_call_log_tool ON call_log(tool_name);
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
}
