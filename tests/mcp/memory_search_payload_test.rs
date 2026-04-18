//! Integration test: search(scope="memory") surfaces confidence + counters.
//!
//! wiz-agents needs confidence, access_count, confirmations, last_confirmed_at
//! to drive Bayesian updates and prior ranking — the MCP payload must expose
//! these fields, not just the id/title/type the formatter used to emit.

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, RawContent};

use mdkb::cli::handlers::{Context, handle_init};
use mdkb::mcp::server::McpServer;
use mdkb::mcp::tools::SearchParams;
use mdkb::store::memory::{self, EntryStatus, EntryType, MemoryEntry, SourceType};

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

#[tokio::test]
async fn memory_search_payload_exposes_confidence_and_counters() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().to_path_buf();
    handle_init(&root).expect("init mdkb");

    let now = chrono::Utc::now().timestamp();
    let confirmed_at = now - 3600; // 1h ago

    {
        let ctx = Context::open(&root).expect("open ctx");
        let entry = MemoryEntry {
            id: "payload-signals".to_string(),
            title: "Payload signals for wiz priors".to_string(),
            content: "Signals: confidence, access, confirmations, last_confirmed_at.".to_string(),
            entry_type: EntryType::Decision,
            tags: vec!["wiz".to_string(), "priors".to_string()],
            status: EntryStatus::Active,
            created_at: now - 86_400,
            updated_at: now,
            superseded_by: None,
            access_count: 7,
            last_accessed: Some(now),
            source_path: None,
            confirmations: 3,
            last_confirmed_at: Some(confirmed_at),
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        memory::add_entry(&ctx.conn, &entry).expect("add entry");
    }

    let server = McpServer::new(root);
    let result = server
        .search(Parameters(SearchParams {
            query: "payload signals".to_string(),
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: Some("memory".to_string()),
            kind: None,
            threshold: 0.5,
            file: None,
        }))
        .await
        .expect("search scope=memory");

    let text = first_text(&result);

    assert!(
        text.contains("payload-signals"),
        "payload must name the entry id, got: {text}"
    );
    assert!(
        text.contains("conf:"),
        "payload must include confidence field `conf:`, got: {text}"
    );
    assert!(
        text.contains("confirms:3"),
        "payload must include confirmations counter `confirms:3`, got: {text}"
    );
    assert!(
        text.contains("access:7"),
        "payload must include access_count `access:7`, got: {text}"
    );
    assert!(
        text.contains("confirmed:"),
        "payload must include last_confirmed_at marker `confirmed:`, got: {text}"
    );
}

#[tokio::test]
async fn memory_search_payload_marks_never_confirmed_entries() {
    let temp_dir = tempfile::tempdir().expect("tempdir");
    let root = temp_dir.path().to_path_buf();
    handle_init(&root).expect("init mdkb");

    let now = chrono::Utc::now().timestamp();

    {
        let ctx = Context::open(&root).expect("open ctx");
        let entry = MemoryEntry {
            id: "never-confirmed".to_string(),
            title: "Freshly written, not yet confirmed".to_string(),
            content: "A brand-new entry with zero confirmations.".to_string(),
            entry_type: EntryType::Topic,
            tags: vec![],
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            last_confirmed_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        memory::add_entry(&ctx.conn, &entry).expect("add entry");
    }

    let server = McpServer::new(root);
    let result = server
        .search(Parameters(SearchParams {
            query: "freshly written".to_string(),
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: Some("memory".to_string()),
            kind: None,
            threshold: 0.5,
            file: None,
        }))
        .await
        .expect("search scope=memory");

    let text = first_text(&result);
    assert!(
        text.contains("never-confirmed"),
        "entry id must appear: {text}"
    );
    assert!(
        text.contains("confirms:0"),
        "must show zero confirms: {text}"
    );
    assert!(text.contains("access:0"), "must show zero access: {text}");
    assert!(
        !text.contains("confirmed:"),
        "must omit confirmed marker when never confirmed, got: {text}"
    );
}
