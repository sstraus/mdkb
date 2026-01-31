//! Integration tests for the `usage` MCP tool — surfaces the token ledger.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, EmptyObject, RawContent};

use mdkb::mcp::server::McpServer;
use mdkb::mcp::tools::{MemoryWriteParams, SearchParams, UsageParams};

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

async fn server_with_activity() -> (tempfile::TempDir, McpServer) {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().to_path_buf();
    mdkb::cli::handlers::handle_init(&root).expect("init mdkb");

    let server = McpServer::new(root);

    // Generate some activity so the ledger is non-empty.
    let _ = server
        .status(Parameters(EmptyObject {}))
        .await
        .expect("status");

    server
        .memory_write(Parameters(MemoryWriteParams {
            id: "usage-test-memory".to_string(),
            root: None,
            title: "usage test".to_string(),
            content: "A tiny memory entry to populate the ledger.".to_string(),
            source_file: None,
            entry_type: "topic".to_string(),
            tags: vec!["test".to_string()],
            source_type: "user_statement".to_string(),
            ttl: None,
            due_in: None,
        }))
        .await
        .expect("memory_write");

    let _ = server
        .search(Parameters(SearchParams {
            query: "usage".to_string(),
            root: None,
            limit: 5,
            collection: None,
            include_superseded: false,
            scope: Some("memory".to_string()),
            kind: None,
            threshold: 0.5,
            file: None,
            min_confidence: None,
        }))
        .await
        .expect("search");

    (temp_dir, server)
}

#[tokio::test]
async fn usage_returns_json_with_session_and_lifetime_sections() {
    let (_tmp, server) = server_with_activity().await;

    let result = server
        .usage(Parameters(UsageParams {
            session_only: false,
            root: None,
        }))
        .await
        .expect("usage call");

    let text = first_text(&result);
    let json: serde_json::Value =
        serde_json::from_str(&text).expect("usage must return parseable JSON");

    let session = json.get("session").expect("session block present");
    assert!(
        session
            .get("total_calls")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            >= 3,
        "session.total_calls should reflect status+memory_write+search, got: {session}"
    );
    assert!(
        session
            .get("total_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            > 0,
        "session.total_tokens must be non-zero after activity: {session}"
    );
    assert!(session.get("truncations").is_some());

    let per_tool = json
        .get("per_tool")
        .and_then(|v| v.as_array())
        .expect("per_tool array present");
    assert!(
        !per_tool.is_empty(),
        "per_tool must have at least one entry after activity"
    );
    for entry in per_tool {
        assert!(entry.get("tool_name").and_then(|v| v.as_str()).is_some());
        assert!(entry.get("call_count").and_then(|v| v.as_u64()).is_some());
        assert!(entry.get("total_tokens").and_then(|v| v.as_u64()).is_some());
    }

    let top5 = json
        .get("top_5_most_called")
        .and_then(|v| v.as_array())
        .expect("top_5_most_called array present");
    assert!(top5.len() <= 5, "top_5 must be capped at 5");

    let lifetime = json.get("lifetime").expect("lifetime block present");
    assert!(lifetime.get("total_sessions").is_some());
    assert!(lifetime.get("total_calls").is_some());
    assert!(lifetime.get("total_tokens").is_some());
    assert!(lifetime.get("avg_tokens_per_call").is_some());
}

#[tokio::test]
async fn usage_session_only_omits_lifetime() {
    let (_tmp, server) = server_with_activity().await;

    let result = server
        .usage(Parameters(UsageParams {
            session_only: true,
            root: None,
        }))
        .await
        .expect("usage session_only");

    let text = first_text(&result);
    let json: serde_json::Value = serde_json::from_str(&text).expect("parseable JSON");

    assert!(json.get("session").is_some(), "session must remain present");
    assert!(
        json.get("lifetime").is_none(),
        "session_only=true must exclude lifetime block, got: {json}"
    );
}
