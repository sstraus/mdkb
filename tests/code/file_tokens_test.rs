//! Integration test: file token estimates in the code index.
//!
//! Verifies that:
//! 1. The indexing pipeline populates `token_estimate` for every indexed file.
//! 2. MCP `search(scope="symbols")` output annotates each result with the
//!    containing file's token count (`(file: ~Ntok)`).

use std::fs;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::RawContent;

use mdkb::cli::handlers::{handle_code_index, handle_init};
use mdkb::code::indexing::IndexFacade;
use mdkb::mcp::server::McpServer;
use mdkb::mcp::tools::SearchParams;

#[tokio::test]
async fn indexing_populates_token_estimate_and_symbol_search_reports_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().to_path_buf();

    handle_init(&root).expect("init mdkb");

    // Seed a source file whose tokens are non-trivially countable.
    let src = root.join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    fs::write(
        src.join("lib.rs"),
        r#"
/// A greeter function.
pub fn unique_greeter_symbol() -> &'static str {
    "hello from unique_greeter_symbol"
}

pub struct UniqueGreeterStruct {
    pub name: String,
}
"#,
    )
    .expect("write lib.rs");

    // Index the project.
    let stats = handle_code_index(&root, &[]).expect("code index");
    assert!(
        stats.files_indexed >= 1,
        "expected at least one file indexed"
    );
    assert!(
        stats.symbols_indexed >= 2,
        "expected unique_greeter_symbol + UniqueGreeterStruct"
    );

    // Every indexed file must have a positive token_estimate.
    let index_path = root.join(".mdkb/code.sqlite");
    let facade = IndexFacade::open_or_create(&index_path).expect("open facade");
    let conn = facade.db().conn();
    let (total, missing, max_est): (i64, i64, i64) = conn
        .query_row(
            "SELECT COUNT(*), \
                    SUM(CASE WHEN token_estimate IS NULL OR token_estimate <= 0 THEN 1 ELSE 0 END), \
                    COALESCE(MAX(token_estimate), 0) \
             FROM code_files",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("query code_files");
    assert!(total >= 1, "expected at least one code_files row");
    assert_eq!(
        missing, 0,
        "every code_files row must have a positive token_estimate (total={total})"
    );
    assert!(max_est > 0, "max token_estimate must be > 0");

    // Search via MCP with scope="symbols" and verify annotation.
    let server = McpServer::new(root.clone());
    let result = server
        .search(Parameters(SearchParams {
            query: "unique_greeter_symbol".to_string(),
            root: None,
            limit: 10,
            collection: None,
            include_superseded: false,
            scope: Some("symbols".to_string()),
            kind: None,
            threshold: 0.5,
            file: None,
            min_confidence: None,
        }))
        .await
        .expect("search scope=symbols");

    let text = result
        .content
        .iter()
        .filter_map(|c| match &c.raw {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        text.contains("unique_greeter_symbol"),
        "search must mention the symbol. Got:\n{text}"
    );
    assert!(
        text.contains("(file: ~") && text.contains("tok)"),
        "symbol output must include file token count. Got:\n{text}"
    );
}
