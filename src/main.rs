//! mdkb - Local markdown knowledge base CLI.

use mimalloc::MiMalloc;
use std::env;

/// Use mimalloc as the global allocator for improved performance.
/// Per Pragmatic Rust Guidelines (M-USE-ALLOCATOR-OPTIMIZED), mimalloc provides
/// better performance than the default allocator, especially for multi-threaded
/// workloads and frequent small allocations.
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use tracing::Level;
use tracing_subscriber::EnvFilter;

use mdkb::Result;
use mdkb::cli::handlers::{
    Context, EmbedResult, StatsResult, handle_collection_add, handle_collection_list, handle_collection_remove,
    handle_collection_rename, handle_embed, handle_get, handle_hybrid_search, handle_init,
    handle_mget, handle_stats, handle_status, handle_update,
};
use mdkb::cli::{Cli, CollectionCommand, Command, OutputFormat};
use mdkb::mcp::server::run_server;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse_args();

    // Set up tracing based on verbosity
    let level = match cli.verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(level.into()))
        .with_writer(std::io::stderr)
        .init();

    tracing::debug!("mdkb starting with verbosity level {}", cli.verbose);

    let cwd = env::current_dir()?;

    match cli.command {
        Command::Init => {
            tracing::info!("Initializing mdkb...");
            handle_init(&cwd)?;
            println!("Initialized .mdkb/ in {}", cwd.display());
        }
        Command::Collection(cmd) => {
            let ctx = Context::open(&cwd)?;
            match cmd {
                CollectionCommand::Add {
                    name,
                    path,
                    pattern,
                } => {
                    handle_collection_add(&ctx, &name, &path, &pattern)?;
                    println!("Added collection '{name}'");
                }
                CollectionCommand::Remove { name } => {
                    if handle_collection_remove(&ctx, &name)? {
                        println!("Removed collection '{name}'");
                    } else {
                        println!("Collection '{name}' not found");
                    }
                }
                CollectionCommand::List => {
                    let collections = handle_collection_list(&ctx)?;
                    format_collections(&collections, cli.format);
                }
                CollectionCommand::Rename { old_name, new_name } => {
                    handle_collection_rename(&ctx, &old_name, &new_name)?;
                    println!("Renamed collection '{old_name}' to '{new_name}'");
                }
            }
        }
        Command::Search {
            query,
            limit,
            collection,
        } => {
            let ctx = Context::open(&cwd)?;
            let results = handle_hybrid_search(&ctx, &query, limit, collection.as_deref())?;
            format_search_results(&results, cli.format);
        }
        Command::Get { id, lines } => {
            let ctx = Context::open(&cwd)?;
            let (doc, content) = handle_get(&ctx, &id, lines.as_deref())?;
            format_document(&doc, &content, cli.format);
        }
        Command::Mget {
            pattern,
            collection,
        } => {
            let ctx = Context::open(&cwd)?;
            let results = handle_mget(&ctx, &pattern, collection.as_deref())?;
            format_mget_results(&results, cli.format);
        }
        Command::Status => {
            let ctx = Context::open(&cwd)?;
            let status = handle_status(&ctx)?;
            format_status(&status, cli.format);
        }
        Command::Update => {
            let ctx = Context::open(&cwd)?;
            let result = handle_update(&ctx, &cwd)?;
            format_update_result(&result, cli.format);
        }
        Command::Embed => {
            let ctx = Context::open(&cwd)?;
            let result = handle_embed(&ctx)?;
            format_embed_result(&result, cli.format);
        }
        Command::Serve => {
            run_server(cwd).await?;
        }
        Command::Stats { sessions, aggregate } => {
            let ctx = Context::open(&cwd)?;
            let result = handle_stats(&ctx, sessions, aggregate)?;
            format_stats_result(&result, cli.format);
        }
    }

    Ok(())
}

fn format_collections(collections: &[mdkb::domain::Collection], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(collections).unwrap());
        }
        OutputFormat::Csv => {
            println!("name,path,pattern,created_at,updated_at");
            for c in collections {
                println!(
                    "{},{},{},{},{}",
                    c.name, c.path, c.pattern, c.created_at, c.updated_at
                );
            }
        }
        OutputFormat::Markdown => {
            println!("| Name | Path | Pattern |");
            println!("|------|------|---------|");
            for c in collections {
                println!("| {} | {} | {} |", c.name, c.path, c.pattern);
            }
        }
        OutputFormat::Text => {
            if collections.is_empty() {
                println!("No collections found.");
            } else {
                for c in collections {
                    println!("{}: {} ({})", c.name, c.path, c.pattern);
                }
            }
        }
    }
}

fn format_search_results(results: &[mdkb::domain::SearchResult], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(results).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,collection,path,title,score");
            for r in results {
                println!(
                    "{},{},{},{},{}",
                    r.id,
                    r.collection,
                    r.path,
                    r.title.as_deref().unwrap_or(""),
                    r.score
                );
            }
        }
        OutputFormat::Markdown => {
            println!("| ID | Collection | Path | Title | Score |");
            println!("|----|------------|------|-------|-------|");
            for r in results {
                println!(
                    "| {} | {} | {} | {} | {:.2} |",
                    r.id,
                    r.collection,
                    r.path,
                    r.title.as_deref().unwrap_or("-"),
                    r.score
                );
            }
        }
        OutputFormat::Text => {
            if results.is_empty() {
                println!("No results found.");
            } else {
                for r in results {
                    let title = r.title.as_deref().unwrap_or("(untitled)");
                    println!("[{}] {}:{} - {} (score: {:.2})", r.id, r.collection, r.path, title, r.score);
                }
            }
        }
    }
}

fn format_document(doc: &mdkb::domain::Document, content: &str, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "document": doc,
                "content": content,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        _ => {
            println!("{}", content);
        }
    }
}

fn format_status(status: &mdkb::domain::IndexStatus, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(status).unwrap());
        }
        OutputFormat::Csv => {
            println!("collections,documents,stale,db_size_bytes");
            println!(
                "{},{},{},{}",
                status.collections, status.documents, status.stale_documents, status.db_size_bytes
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Collections: {}", status.collections);
            println!("Documents:   {}", status.documents);
            println!("Stale:       {}", status.stale_documents);
            println!("DB Size:     {} bytes", status.db_size_bytes);
            if let Some(ts) = status.last_updated {
                println!("Last Update: {}", ts);
            }
        }
    }
}

fn format_update_result(result: &mdkb::domain::UpdateResult, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(result).unwrap());
        }
        OutputFormat::Csv => {
            println!("added,updated,removed,unchanged,errors");
            println!(
                "{},{},{},{},{}",
                result.added,
                result.updated,
                result.removed,
                result.unchanged,
                result.errors.len()
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Added:     {}", result.added);
            println!("Updated:   {}", result.updated);
            println!("Removed:   {}", result.removed);
            println!("Unchanged: {}", result.unchanged);
            if !result.errors.is_empty() {
                println!("Errors:    {}", result.errors.len());
                for err in &result.errors {
                    println!("  - {}", err);
                }
            }
        }
    }
}

fn format_mget_results(results: &[(mdkb::domain::Document, String)], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output: Vec<_> = results
                .iter()
                .map(|(doc, content)| {
                    serde_json::json!({
                        "document": doc,
                        "content": content,
                    })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,collection,path,title");
            for (doc, _) in results {
                println!(
                    "{},{},{},{}",
                    doc.id,
                    doc.collection,
                    doc.relative_path,
                    doc.title.as_deref().unwrap_or("")
                );
            }
        }
        OutputFormat::Markdown => {
            for (doc, content) in results {
                let title = doc.title.as_deref().unwrap_or(&doc.relative_path);
                println!("## {} ({})\n", title, doc.relative_path);
                println!("{}\n", content);
                println!("---\n");
            }
        }
        OutputFormat::Text => {
            if results.is_empty() {
                println!("No documents found.");
            } else {
                println!("Found {} documents:\n", results.len());
                for (doc, content) in results {
                    let title = doc.title.as_deref().unwrap_or("(untitled)");
                    println!("=== [{}] {} - {} ===", doc.id, doc.relative_path, title);
                    println!("{}\n", content);
                }
            }
        }
    }
}

fn format_embed_result(result: &EmbedResult, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "generated": result.generated,
                "skipped": result.skipped,
                "errors": result.errors,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("generated,skipped,errors");
            println!(
                "{},{},{}",
                result.generated,
                result.skipped,
                result.errors.len()
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Generated: {}", result.generated);
            println!("Skipped:   {}", result.skipped);
            if !result.errors.is_empty() {
                println!("Errors:    {}", result.errors.len());
                for err in &result.errors {
                    println!("  - {}", err);
                }
            }
        }
    }
}

fn format_stats_result(result: &StatsResult, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(result).unwrap());
        }
        OutputFormat::Csv => {
            println!("total_sessions,total_calls,total_tokens,total_truncations,avg_tokens_per_call");
            println!(
                "{},{},{},{},{:.1}",
                result.aggregate.total_sessions,
                result.aggregate.total_calls,
                result.aggregate.total_tokens,
                result.aggregate.total_truncations,
                result.aggregate.avg_tokens_per_call,
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("=== Aggregate Stats ===");
            println!("Total sessions:    {}", result.aggregate.total_sessions);
            println!("Total calls:       {}", result.aggregate.total_calls);
            println!("Total tokens:      {}", result.aggregate.total_tokens);
            println!("Total truncations: {}", result.aggregate.total_truncations);
            println!(
                "Avg tokens/call:   {:.1}",
                result.aggregate.avg_tokens_per_call
            );

            if !result.sessions.is_empty() {
                println!("\n=== Recent Sessions ===");
                for session in &result.sessions {
                    let status = if session.ended_at.is_some() {
                        "ended"
                    } else {
                        "active"
                    };
                    // Format timestamp as human-readable
                    let started = chrono::DateTime::from_timestamp(session.started_at, 0)
                        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    println!(
                        "\nSession {} - {} ({}):",
                        session.id, started, status
                    );
                    println!("  Calls:       {}", session.total_calls);
                    println!("  Tokens:      {}", session.total_tokens);
                    println!("  Truncations: {}", session.truncation_count);

                    if !session.tool_usage.is_empty() {
                        println!("  Tools:");
                        for tool in &session.tool_usage {
                            println!(
                                "    - {}: {} calls, {} tokens",
                                tool.tool_name, tool.call_count, tool.total_tokens
                            );
                        }
                    }
                }
            }
        }
    }
}
