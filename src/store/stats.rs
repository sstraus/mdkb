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

/// Query metrics summary for a period.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMetricsSummary {
    pub total_queries: i64,
    pub zero_result_rate: f64,
    pub re_search_rate: f64,
    pub latency_p50: i64,
    pub latency_p95: i64,
    pub latency_p99: i64,
    pub score_above_80: f64,  // % of queries with top_score > 0.8
    pub score_50_to_80: f64,  // % with 0.5 <= top_score <= 0.8
    pub score_below_50: f64,  // % with top_score < 0.5
}

/// Get comprehensive query metrics for a period.
pub fn get_query_metrics(conn: &Connection, days: u32) -> Result<QueryMetricsSummary> {
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 24 * 60 * 60);

    // Total queries
    let total_queries: i64 = conn.query_row(
        "SELECT COUNT(*) FROM query_events WHERE created_at >= ?1",
        params![cutoff],
        |row| row.get(0),
    )?;

    if total_queries == 0 {
        return Ok(QueryMetricsSummary {
            total_queries: 0,
            zero_result_rate: 0.0,
            re_search_rate: 0.0,
            latency_p50: 0,
            latency_p95: 0,
            latency_p99: 0,
            score_above_80: 0.0,
            score_50_to_80: 0.0,
            score_below_50: 0.0,
        });
    }

    // Zero result rate
    let zero_results: i64 = conn.query_row(
        "SELECT COUNT(*) FROM query_events WHERE created_at >= ?1 AND result_count = 0",
        params![cutoff],
        |row| row.get(0),
    )?;
    let zero_result_rate = (zero_results as f64 / total_queries as f64) * 100.0;

    // Re-search rate (same query hash within 30 seconds)
    let re_searches: i64 = conn.query_row(
        r#"
        SELECT COUNT(*) FROM query_events e1
        WHERE e1.created_at >= ?1
        AND EXISTS (
            SELECT 1 FROM query_events e2
            WHERE e2.query_hash = e1.query_hash
            AND e2.id != e1.id
            AND e2.created_at BETWEEN e1.created_at - 30 AND e1.created_at + 30
        )
        "#,
        params![cutoff],
        |row| row.get(0),
    )?;
    let re_search_rate = (re_searches as f64 / total_queries as f64) * 100.0;

    // Latency percentiles
    let latency_p50 = get_percentile(conn, cutoff, 50)?;
    let latency_p95 = get_percentile(conn, cutoff, 95)?;
    let latency_p99 = get_percentile(conn, cutoff, 99)?;

    // Score distribution (only for queries with results)
    let with_scores: i64 = conn.query_row(
        "SELECT COUNT(*) FROM query_events WHERE created_at >= ?1 AND top_score IS NOT NULL",
        params![cutoff],
        |row| row.get(0),
    )?;

    let (score_above_80, score_50_to_80, score_below_50) = if with_scores > 0 {
        let above_80: i64 = conn.query_row(
            "SELECT COUNT(*) FROM query_events WHERE created_at >= ?1 AND top_score > 0.8",
            params![cutoff],
            |row| row.get(0),
        )?;

        let between: i64 = conn.query_row(
            "SELECT COUNT(*) FROM query_events WHERE created_at >= ?1 AND top_score >= 0.5 AND top_score <= 0.8",
            params![cutoff],
            |row| row.get(0),
        )?;

        let below_50: i64 = conn.query_row(
            "SELECT COUNT(*) FROM query_events WHERE created_at >= ?1 AND top_score < 0.5",
            params![cutoff],
            |row| row.get(0),
        )?;

        (
            (above_80 as f64 / with_scores as f64) * 100.0,
            (between as f64 / with_scores as f64) * 100.0,
            (below_50 as f64 / with_scores as f64) * 100.0,
        )
    } else {
        (0.0, 0.0, 0.0)
    };

    Ok(QueryMetricsSummary {
        total_queries,
        zero_result_rate,
        re_search_rate,
        latency_p50,
        latency_p95,
        latency_p99,
        score_above_80,
        score_50_to_80,
        score_below_50,
    })
}

/// Get a specific latency percentile.
fn get_percentile(conn: &Connection, cutoff: i64, percentile: u8) -> Result<i64> {
    // SQLite doesn't have built-in percentile, so we calculate manually
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM query_events WHERE created_at >= ?1",
        params![cutoff],
        |row| row.get(0),
    )?;

    if total == 0 {
        return Ok(0);
    }

    let offset = (total as f64 * (percentile as f64 / 100.0)).floor() as i64;

    let result: i64 = conn.query_row(
        r#"
        SELECT latency_ms FROM query_events
        WHERE created_at >= ?1
        ORDER BY latency_ms
        LIMIT 1 OFFSET ?2
        "#,
        params![cutoff, offset.max(0).min(total - 1)],
        |row| row.get(0),
    )?;

    Ok(result)
}

/// Export query events for a period.
pub fn export_query_events(conn: &Connection, days: u32) -> Result<Vec<QueryEvent>> {
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 24 * 60 * 60);

    let mut stmt = conn.prepare(
        r#"
        SELECT query_hash, query_text, search_type, result_count, latency_ms, top_score, session_id
        FROM query_events
        WHERE created_at >= ?1
        ORDER BY created_at DESC
        "#,
    )?;

    let results = stmt
        .query_map(params![cutoff], |row| {
            Ok(QueryEvent {
                query_hash: row.get(0)?,
                query_text: row.get(1)?,
                search_type: row.get(2)?,
                result_count: row.get(3)?,
                latency_ms: row.get(4)?,
                top_score: row.get(5)?,
                session_id: row.get(6)?,
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

// ==================== A/B Experiments ====================

/// Experiment status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExperimentStatus {
    Running,
    Completed,
    Cancelled,
}

impl std::fmt::Display for ExperimentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => write!(f, "running"),
            Self::Completed => write!(f, "completed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A/B experiment definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Experiment {
    pub id: i64,
    pub name: String,
    pub description: Option<String>,
    pub config_a: String,  // JSON config for variant A
    pub config_b: String,  // JSON config for variant B
    pub traffic_split: f64,  // Fraction to variant A (0.0-1.0, default 0.5)
    pub status: ExperimentStatus,
    pub min_sample_size: i64,  // Minimum samples before significance calculation
    pub created_at: i64,
    pub ended_at: Option<i64>,
    pub winner: Option<String>,  // "A", "B", or None
}

/// Experiment result for a single query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentResult {
    pub experiment_id: i64,
    pub variant: String,  // "A" or "B"
    pub query_hash: String,
    pub latency_ms: i64,
    pub score: f64,
    pub result_count: i64,
    pub created_at: i64,
}

/// Initialize the experiments schema.
pub fn init_experiments_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- A/B Experiments
        CREATE TABLE IF NOT EXISTS experiments (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE,
            description TEXT,
            config_a TEXT NOT NULL,       -- JSON config for variant A
            config_b TEXT NOT NULL,       -- JSON config for variant B
            traffic_split REAL NOT NULL DEFAULT 0.5,  -- Fraction to A
            status TEXT NOT NULL DEFAULT 'running',
            min_sample_size INTEGER NOT NULL DEFAULT 100,
            created_at INTEGER NOT NULL,
            ended_at INTEGER,
            winner TEXT                   -- 'A', 'B', or NULL
        );

        -- Experiment results per query
        CREATE TABLE IF NOT EXISTS experiment_results (
            id INTEGER PRIMARY KEY,
            experiment_id INTEGER NOT NULL,
            variant TEXT NOT NULL,        -- 'A' or 'B'
            query_hash TEXT NOT NULL,
            latency_ms INTEGER NOT NULL,
            score REAL NOT NULL,
            result_count INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            FOREIGN KEY(experiment_id) REFERENCES experiments(id) ON DELETE CASCADE
        );

        -- Indexes
        CREATE INDEX IF NOT EXISTS idx_experiment_results_exp ON experiment_results(experiment_id);
        CREATE INDEX IF NOT EXISTS idx_experiment_results_variant ON experiment_results(experiment_id, variant);
        "#,
    )?;
    Ok(())
}

/// Create a new experiment.
pub fn create_experiment(
    conn: &Connection,
    name: &str,
    description: Option<&str>,
    config_a: &str,
    config_b: &str,
    traffic_split: f64,
    min_sample_size: i64,
) -> Result<i64> {
    let now = chrono::Utc::now().timestamp();

    conn.execute(
        r#"
        INSERT INTO experiments (name, description, config_a, config_b, traffic_split, min_sample_size, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        "#,
        params![name, description, config_a, config_b, traffic_split, min_sample_size, now],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Get an experiment by name.
pub fn get_experiment(conn: &Connection, name: &str) -> Result<Option<Experiment>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, name, description, config_a, config_b, traffic_split, status, min_sample_size, created_at, ended_at, winner
        FROM experiments
        WHERE name = ?1
        "#,
    )?;

    let result = stmt.query_row(params![name], |row| {
        let status_str: String = row.get(6)?;
        let status = match status_str.as_str() {
            "running" => ExperimentStatus::Running,
            "completed" => ExperimentStatus::Completed,
            "cancelled" => ExperimentStatus::Cancelled,
            _ => ExperimentStatus::Running,
        };

        Ok(Experiment {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            config_a: row.get(3)?,
            config_b: row.get(4)?,
            traffic_split: row.get(5)?,
            status,
            min_sample_size: row.get(7)?,
            created_at: row.get(8)?,
            ended_at: row.get(9)?,
            winner: row.get(10)?,
        })
    });

    match result {
        Ok(exp) => Ok(Some(exp)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Get all active (running) experiments.
pub fn get_active_experiments(conn: &Connection) -> Result<Vec<Experiment>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, name, description, config_a, config_b, traffic_split, status, min_sample_size, created_at, ended_at, winner
        FROM experiments
        WHERE status = 'running'
        ORDER BY created_at DESC
        "#,
    )?;

    let results = stmt
        .query_map([], |row| {
            Ok(Experiment {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                config_a: row.get(3)?,
                config_b: row.get(4)?,
                traffic_split: row.get(5)?,
                status: ExperimentStatus::Running,
                min_sample_size: row.get(7)?,
                created_at: row.get(8)?,
                ended_at: row.get(9)?,
                winner: row.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}

/// Determine which variant to use for a query based on consistent hashing.
pub fn get_experiment_variant(experiment: &Experiment, query_hash: &str) -> String {
    use sha2::{Digest, Sha256};

    // Hash experiment name + query hash for consistent routing
    let mut hasher = Sha256::new();
    hasher.update(experiment.name.as_bytes());
    hasher.update(query_hash.as_bytes());
    let result = hasher.finalize();

    // Use first 8 bytes as u64, normalize to [0, 1)
    let bytes: [u8; 8] = result[..8].try_into().unwrap_or([0; 8]);
    let hash_value = u64::from_le_bytes(bytes);
    let normalized = (hash_value as f64) / (u64::MAX as f64);

    if normalized < experiment.traffic_split {
        "A".to_string()
    } else {
        "B".to_string()
    }
}

/// Record an experiment result.
pub fn record_experiment_result(conn: &Connection, result: &ExperimentResult) -> Result<i64> {
    let now = chrono::Utc::now().timestamp();

    conn.execute(
        r#"
        INSERT INTO experiment_results (experiment_id, variant, query_hash, latency_ms, score, result_count, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        "#,
        params![
            result.experiment_id,
            result.variant,
            result.query_hash,
            result.latency_ms,
            result.score,
            result.result_count,
            now,
        ],
    )?;

    Ok(conn.last_insert_rowid())
}

/// Variant statistics for an experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantStats {
    pub variant: String,
    pub sample_count: i64,
    pub avg_score: f64,
    pub avg_latency_ms: f64,
    pub p95_latency_ms: i64,
    pub zero_result_rate: f64,
    pub score_variance: f64,
}

/// Get statistics for a variant in an experiment.
pub fn get_variant_stats(conn: &Connection, experiment_id: i64, variant: &str) -> Result<VariantStats> {
    let sample_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM experiment_results WHERE experiment_id = ?1 AND variant = ?2",
        params![experiment_id, variant],
        |row| row.get(0),
    )?;

    if sample_count == 0 {
        return Ok(VariantStats {
            variant: variant.to_string(),
            sample_count: 0,
            avg_score: 0.0,
            avg_latency_ms: 0.0,
            p95_latency_ms: 0,
            zero_result_rate: 0.0,
            score_variance: 0.0,
        });
    }

    let avg_score: f64 = conn.query_row(
        "SELECT AVG(score) FROM experiment_results WHERE experiment_id = ?1 AND variant = ?2",
        params![experiment_id, variant],
        |row| row.get(0),
    )?;

    let avg_latency: f64 = conn.query_row(
        "SELECT AVG(latency_ms) FROM experiment_results WHERE experiment_id = ?1 AND variant = ?2",
        params![experiment_id, variant],
        |row| row.get(0),
    )?;

    // P95 latency
    let p95_idx = ((sample_count as f64 * 0.95).ceil() as i64).max(1) - 1;
    let p95_latency: i64 = conn.query_row(
        r#"
        SELECT latency_ms FROM experiment_results
        WHERE experiment_id = ?1 AND variant = ?2
        ORDER BY latency_ms
        LIMIT 1 OFFSET ?3
        "#,
        params![experiment_id, variant, p95_idx],
        |row| row.get(0),
    ).unwrap_or(0);

    // Zero result rate
    let zero_results: i64 = conn.query_row(
        "SELECT COUNT(*) FROM experiment_results WHERE experiment_id = ?1 AND variant = ?2 AND result_count = 0",
        params![experiment_id, variant],
        |row| row.get(0),
    )?;
    let zero_result_rate = (zero_results as f64 / sample_count as f64) * 100.0;

    // Score variance for significance testing
    let score_variance: f64 = conn.query_row(
        r#"
        SELECT AVG((score - ?1) * (score - ?1))
        FROM experiment_results
        WHERE experiment_id = ?2 AND variant = ?3
        "#,
        params![avg_score, experiment_id, variant],
        |row| row.get::<_, f64>(0),
    ).unwrap_or(0.0);

    Ok(VariantStats {
        variant: variant.to_string(),
        sample_count,
        avg_score,
        avg_latency_ms: avg_latency,
        p95_latency_ms: p95_latency,
        zero_result_rate,
        score_variance,
    })
}

/// Experiment status with variant statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentStatusReport {
    pub experiment: Experiment,
    pub variant_a: VariantStats,
    pub variant_b: VariantStats,
    pub has_min_samples: bool,
    pub significance: Option<SignificanceResult>,
}

/// Statistical significance result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignificanceResult {
    pub t_statistic: f64,
    pub p_value: f64,
    pub confidence_level: f64,  // e.g., 95.0
    pub significant: bool,
    pub winner: Option<String>,  // "A" or "B" if significant
    pub effect_size: f64,  // Cohen's d
}

/// Calculate two-sample t-test for experiment significance.
pub fn calculate_significance(stats_a: &VariantStats, stats_b: &VariantStats) -> Option<SignificanceResult> {
    if stats_a.sample_count < 2 || stats_b.sample_count < 2 {
        return None;
    }

    let n_a = stats_a.sample_count as f64;
    let n_b = stats_b.sample_count as f64;

    // Pooled variance estimate
    let var_a = stats_a.score_variance;
    let var_b = stats_b.score_variance;

    // Standard error of the difference
    let se = ((var_a / n_a) + (var_b / n_b)).sqrt();

    if se == 0.0 {
        return None;
    }

    // t-statistic
    let t_stat = (stats_a.avg_score - stats_b.avg_score) / se;

    // Degrees of freedom (Welch's approximation)
    let df_num = ((var_a / n_a) + (var_b / n_b)).powi(2);
    let df_den = ((var_a / n_a).powi(2) / (n_a - 1.0)) + ((var_b / n_b).powi(2) / (n_b - 1.0));
    let _df = if df_den > 0.0 { df_num / df_den } else { n_a + n_b - 2.0 };

    // Approximate p-value using normal distribution for large samples
    // For proper implementation, would use Student's t-distribution
    let p_value = 2.0 * (1.0 - normal_cdf(t_stat.abs()));

    // Effect size (Cohen's d)
    let pooled_sd = ((var_a + var_b) / 2.0).sqrt();
    let effect_size = if pooled_sd > 0.0 {
        (stats_a.avg_score - stats_b.avg_score).abs() / pooled_sd
    } else {
        0.0
    };

    // Determine winner at 95% confidence
    let significant = p_value < 0.05;
    let winner = if significant {
        if stats_a.avg_score > stats_b.avg_score {
            Some("A".to_string())
        } else {
            Some("B".to_string())
        }
    } else {
        None
    };

    Some(SignificanceResult {
        t_statistic: t_stat,
        p_value,
        confidence_level: 95.0,
        significant,
        winner,
        effect_size,
    })
}

/// Approximate normal CDF for p-value calculation.
fn normal_cdf(x: f64) -> f64 {
    // Using the approximation from Abramowitz and Stegun
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs() / std::f64::consts::SQRT_2;

    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();

    0.5 * (1.0 + sign * y)
}

/// Get full experiment status report.
pub fn get_experiment_status(conn: &Connection, name: &str) -> Result<Option<ExperimentStatusReport>> {
    let experiment = match get_experiment(conn, name)? {
        Some(e) => e,
        None => return Ok(None),
    };

    let variant_a = get_variant_stats(conn, experiment.id, "A")?;
    let variant_b = get_variant_stats(conn, experiment.id, "B")?;

    let has_min_samples = variant_a.sample_count >= experiment.min_sample_size
        && variant_b.sample_count >= experiment.min_sample_size;

    let significance = if has_min_samples {
        calculate_significance(&variant_a, &variant_b)
    } else {
        None
    };

    Ok(Some(ExperimentStatusReport {
        experiment,
        variant_a,
        variant_b,
        has_min_samples,
        significance,
    }))
}

/// End an experiment and record the winner.
pub fn end_experiment(conn: &Connection, name: &str, winner: Option<&str>) -> Result<()> {
    let now = chrono::Utc::now().timestamp();

    conn.execute(
        "UPDATE experiments SET status = 'completed', ended_at = ?1, winner = ?2 WHERE name = ?3",
        params![now, winner, name],
    )?;

    Ok(())
}

/// Cancel an experiment without a winner.
pub fn cancel_experiment(conn: &Connection, name: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp();

    conn.execute(
        "UPDATE experiments SET status = 'cancelled', ended_at = ?1 WHERE name = ?2",
        params![now, name],
    )?;

    Ok(())
}

/// List all experiments.
pub fn list_experiments(conn: &Connection) -> Result<Vec<Experiment>> {
    let mut stmt = conn.prepare(
        r#"
        SELECT id, name, description, config_a, config_b, traffic_split, status, min_sample_size, created_at, ended_at, winner
        FROM experiments
        ORDER BY created_at DESC
        "#,
    )?;

    let results = stmt
        .query_map([], |row| {
            let status_str: String = row.get(6)?;
            let status = match status_str.as_str() {
                "running" => ExperimentStatus::Running,
                "completed" => ExperimentStatus::Completed,
                "cancelled" => ExperimentStatus::Cancelled,
                _ => ExperimentStatus::Running,
            };

            Ok(Experiment {
                id: row.get(0)?,
                name: row.get(1)?,
                description: row.get(2)?,
                config_a: row.get(3)?,
                config_b: row.get(4)?,
                traffic_split: row.get(5)?,
                status,
                min_sample_size: row.get(7)?,
                created_at: row.get(8)?,
                ended_at: row.get(9)?,
                winner: row.get(10)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(results)
}

#[cfg(test)]
mod experiment_tests {
    use super::*;

    fn setup_experiment_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_stats_schema(&conn).unwrap();
        init_experiments_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn test_create_experiment() {
        let conn = setup_experiment_db();

        let exp_id = create_experiment(
            &conn,
            "test-exp",
            Some("Test experiment"),
            r#"{"strategy":"fixed"}"#,
            r#"{"strategy":"markdown"}"#,
            0.5,
            100,
        ).unwrap();

        assert!(exp_id > 0);

        let exp = get_experiment(&conn, "test-exp").unwrap().unwrap();
        assert_eq!(exp.name, "test-exp");
        assert_eq!(exp.status, ExperimentStatus::Running);
        assert_eq!(exp.traffic_split, 0.5);
    }

    #[test]
    fn test_consistent_variant_assignment() {
        let conn = setup_experiment_db();

        create_experiment(
            &conn,
            "test-exp",
            None,
            "{}",
            "{}",
            0.5,
            10,
        ).unwrap();

        let exp = get_experiment(&conn, "test-exp").unwrap().unwrap();

        // Same query hash should always get same variant
        let hash = "abc123";
        let variant1 = get_experiment_variant(&exp, hash);
        let variant2 = get_experiment_variant(&exp, hash);
        let variant3 = get_experiment_variant(&exp, hash);

        assert_eq!(variant1, variant2);
        assert_eq!(variant2, variant3);
    }

    #[test]
    fn test_record_and_stats() {
        let conn = setup_experiment_db();

        let exp_id = create_experiment(
            &conn,
            "test-exp",
            None,
            "{}",
            "{}",
            0.5,
            5,
        ).unwrap();

        // Record some results for variant A
        for i in 0..10 {
            let result = ExperimentResult {
                experiment_id: exp_id,
                variant: "A".to_string(),
                query_hash: format!("hash-a-{i}"),
                latency_ms: 10 + i as i64,
                score: 0.7 + (i as f64 * 0.01),
                result_count: 5,
                created_at: 0,
            };
            record_experiment_result(&conn, &result).unwrap();
        }

        // Record some results for variant B
        for i in 0..10 {
            let result = ExperimentResult {
                experiment_id: exp_id,
                variant: "B".to_string(),
                query_hash: format!("hash-b-{i}"),
                latency_ms: 15 + i as i64,
                score: 0.8 + (i as f64 * 0.01),
                result_count: 6,
                created_at: 0,
            };
            record_experiment_result(&conn, &result).unwrap();
        }

        let stats_a = get_variant_stats(&conn, exp_id, "A").unwrap();
        assert_eq!(stats_a.sample_count, 10);
        assert!(stats_a.avg_score > 0.7 && stats_a.avg_score < 0.8);

        let stats_b = get_variant_stats(&conn, exp_id, "B").unwrap();
        assert_eq!(stats_b.sample_count, 10);
        assert!(stats_b.avg_score > 0.8 && stats_b.avg_score < 0.9);
    }

    #[test]
    fn test_significance_calculation() {
        let stats_a = VariantStats {
            variant: "A".to_string(),
            sample_count: 100,
            avg_score: 0.7,
            avg_latency_ms: 20.0,
            p95_latency_ms: 35,
            zero_result_rate: 5.0,
            score_variance: 0.02,
        };

        let stats_b = VariantStats {
            variant: "B".to_string(),
            sample_count: 100,
            avg_score: 0.85,
            avg_latency_ms: 22.0,
            p95_latency_ms: 40,
            zero_result_rate: 3.0,
            score_variance: 0.02,
        };

        let sig = calculate_significance(&stats_a, &stats_b).unwrap();

        // With a significant difference in means, should be significant
        assert!(sig.significant);
        assert_eq!(sig.winner.as_deref(), Some("B"));
        assert!(sig.effect_size > 0.0);
    }

    #[test]
    fn test_end_experiment() {
        let conn = setup_experiment_db();

        create_experiment(&conn, "test-exp", None, "{}", "{}", 0.5, 10).unwrap();

        end_experiment(&conn, "test-exp", Some("B")).unwrap();

        let exp = get_experiment(&conn, "test-exp").unwrap().unwrap();
        assert_eq!(exp.status, ExperimentStatus::Completed);
        assert_eq!(exp.winner.as_deref(), Some("B"));
        assert!(exp.ended_at.is_some());
    }

    #[test]
    fn test_experiment_status_report() {
        let conn = setup_experiment_db();

        let exp_id = create_experiment(&conn, "test-exp", None, "{}", "{}", 0.5, 5).unwrap();

        // Add enough samples for both variants
        for i in 0..6 {
            record_experiment_result(&conn, &ExperimentResult {
                experiment_id: exp_id,
                variant: "A".to_string(),
                query_hash: format!("a-{i}"),
                latency_ms: 10,
                score: 0.7,
                result_count: 5,
                created_at: 0,
            }).unwrap();

            record_experiment_result(&conn, &ExperimentResult {
                experiment_id: exp_id,
                variant: "B".to_string(),
                query_hash: format!("b-{i}"),
                latency_ms: 12,
                score: 0.75,
                result_count: 5,
                created_at: 0,
            }).unwrap();
        }

        let status = get_experiment_status(&conn, "test-exp").unwrap().unwrap();

        assert!(status.has_min_samples);
        assert_eq!(status.variant_a.sample_count, 6);
        assert_eq!(status.variant_b.sample_count, 6);
    }
}
