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
    Context, EmbedResult, EvolutionHistoryEntry, StatsResult, handle_collection_add, handle_collection_list,
    handle_collection_remove, handle_collection_rename, handle_current, handle_embed, handle_evolve_corrects,
    handle_evolve_extends, handle_evolve_retracts, handle_evolve_supersedes, handle_evolve_updates,
    handle_get, handle_history, handle_hybrid_search, handle_init, handle_memory_add, handle_memory_list,
    handle_memory_prune, handle_memory_rm, handle_memory_search, handle_memory_show, handle_memory_warmup,
    handle_metrics_export, handle_metrics_latency, handle_metrics_show,
    handle_mget, handle_stats, handle_status, handle_superseded_by, handle_update,
};
use mdkb::cli::{Cli, CollectionCommand, Command, EvolveCommand, MemoryCommand, MetricsCommand, OutputFormat};
use mdkb::store::evolution::Evolution;
use mdkb::store::memory::MemoryEntry;
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
            include_superseded,
        } => {
            let ctx = Context::open(&cwd)?;
            let results = handle_hybrid_search(&ctx, &query, limit, collection.as_deref(), include_superseded)?;
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
        Command::Metrics(cmd) => {
            let ctx = Context::open(&cwd)?;
            match cmd {
                MetricsCommand::Show { period } => {
                    let metrics = handle_metrics_show(&ctx, period)?;
                    format_metrics_summary(&metrics, period, cli.format);
                }
                MetricsCommand::Latency { period } => {
                    let stats = handle_metrics_latency(&ctx)?;
                    format_latency_stats(&stats, period, cli.format);
                }
                MetricsCommand::Quality { period } => {
                    let metrics = handle_metrics_show(&ctx, period)?;
                    format_quality_metrics(&metrics, period, cli.format);
                }
                MetricsCommand::Export { period } => {
                    let events = handle_metrics_export(&ctx, period)?;
                    format_metrics_export(&events, cli.format);
                }
            }
        }
        Command::Memory(cmd) => {
            let ctx = Context::open(&cwd)?;
            match cmd {
                MemoryCommand::Add {
                    id,
                    title,
                    entry_type,
                    tags,
                    content,
                } => {
                    let content = content.unwrap_or_else(|| {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin().read_to_string(&mut buf).unwrap_or_default();
                        buf
                    });
                    handle_memory_add(&ctx, &id, &title, &entry_type, tags.as_deref(), &content)?;
                    println!("Added memory entry '{id}'");
                }
                MemoryCommand::Show { id } => {
                    if let Some(entry) = handle_memory_show(&ctx, &id)? {
                        format_memory_entry(&entry, cli.format);
                    } else {
                        println!("Memory entry '{id}' not found");
                    }
                }
                MemoryCommand::List { limit, status } => {
                    let entries = handle_memory_list(&ctx, limit, status.as_deref())?;
                    format_memory_list(&entries, cli.format);
                }
                MemoryCommand::Search { query, limit } => {
                    let entries = handle_memory_search(&ctx, &query, limit)?;
                    format_memory_list(&entries, cli.format);
                }
                MemoryCommand::Warmup { limit } => {
                    let index = handle_memory_warmup(&ctx, limit)?;
                    format_warmup_index(&index, cli.format);
                }
                MemoryCommand::Rm { id } => {
                    if handle_memory_rm(&ctx, &id)? {
                        println!("Deleted memory entry '{id}'");
                    } else {
                        println!("Memory entry '{id}' not found");
                    }
                }
                MemoryCommand::Prune { days, dry_run } => {
                    let pruned = handle_memory_prune(&ctx, days, dry_run)?;
                    format_prune_result(&pruned, days, dry_run, cli.format);
                }
            }
        }
        Command::Evolve(cmd) => {
            let ctx = Context::open(&cwd)?;
            match cmd {
                EvolveCommand::Supersedes { new, old, reason } => {
                    let id = handle_evolve_supersedes(&ctx, &new, &old, reason.as_deref())?;
                    println!("Created evolution relationship #{id}: {new} supersedes {old}");
                }
                EvolveCommand::Updates { new, old, scope, reason } => {
                    let id = handle_evolve_updates(&ctx, &new, &old, scope.as_deref(), reason.as_deref())?;
                    println!("Created evolution relationship #{id}: {new} updates {old}");
                }
                EvolveCommand::Corrects { new, old, reason } => {
                    let id = handle_evolve_corrects(&ctx, &new, &old, reason.as_deref())?;
                    println!("Created evolution relationship #{id}: {new} corrects {old}");
                }
                EvolveCommand::Retracts { new, old, reason } => {
                    let id = handle_evolve_retracts(&ctx, &new, &old, reason.as_deref())?;
                    println!("Created evolution relationship #{id}: {new} retracts {old}");
                }
                EvolveCommand::Extends { new, old, reason } => {
                    let id = handle_evolve_extends(&ctx, &new, &old, reason.as_deref())?;
                    println!("Created evolution relationship #{id}: {new} extends {old}");
                }
            }
        }
        Command::History { path } => {
            let ctx = Context::open(&cwd)?;
            let history = handle_history(&ctx, &path)?;
            format_evolution_history(&history, cli.format);
        }
        Command::Current { path } => {
            let ctx = Context::open(&cwd)?;
            if let Some(doc) = handle_current(&ctx, &path)? {
                format_current_document(&doc, cli.format);
            } else {
                println!("No current version found for '{path}'");
            }
        }
        Command::SupersededBy { path } => {
            let ctx = Context::open(&cwd)?;
            let evolutions = handle_superseded_by(&ctx, &path)?;
            format_superseded_by(&evolutions, cli.format);
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

fn format_memory_entry(entry: &MemoryEntry, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(entry).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,title,type,status,tags,access_count");
            println!(
                "{},{},{},{},{},{}",
                entry.id,
                entry.title,
                entry.entry_type,
                entry.status,
                entry.tags.join(";"),
                entry.access_count
            );
        }
        OutputFormat::Markdown => {
            println!("# {}\n", entry.title);
            println!("**ID:** {}", entry.id);
            println!("**Type:** {}", entry.entry_type);
            println!("**Tags:** {}", entry.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
            println!("**Access count:** {}\n", entry.access_count);
            println!("---\n");
            println!("{}", entry.content);
        }
        OutputFormat::Text => {
            println!("[{}] {} ({})", entry.id, entry.title, entry.entry_type);
            if !entry.tags.is_empty() {
                println!("Tags: {}", entry.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" "));
            }
            println!("Access count: {}", entry.access_count);
            println!("\n{}", entry.content);
        }
    }
}

fn format_memory_list(entries: &[MemoryEntry], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(entries).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,title,type,status,tags,access_count");
            for e in entries {
                println!(
                    "{},{},{},{},{},{}",
                    e.id,
                    e.title,
                    e.entry_type,
                    e.status,
                    e.tags.join(";"),
                    e.access_count
                );
            }
        }
        OutputFormat::Markdown => {
            println!("| ID | Title | Type | Tags | Access |");
            println!("|----|-------|------|------|--------|");
            for e in entries {
                let tags = e.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ");
                println!("| {} | {} | {} | {} | {} |", e.id, e.title, e.entry_type, tags, e.access_count);
            }
        }
        OutputFormat::Text => {
            if entries.is_empty() {
                println!("No memory entries found.");
            } else {
                for e in entries {
                    let tags = e.tags.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(" ");
                    println!("[{}] {} ({}) {} - {} accesses", e.id, e.title, e.entry_type, tags, e.access_count);
                }
            }
        }
    }
}

fn format_warmup_index(index: &[String], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "count": index.len(),
                "entries": index,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv | OutputFormat::Text => {
            if index.is_empty() {
                println!("Memory ({} entries):", index.len());
            } else {
                println!("Memory ({} entries):", index.len());
                for line in index {
                    println!("{line}");
                }
            }
        }
        OutputFormat::Markdown => {
            println!("## Memory Index ({} entries)\n", index.len());
            for line in index {
                println!("- {line}");
            }
        }
    }
}

fn format_prune_result(pruned: &[String], days: u32, dry_run: bool, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "dry_run": dry_run,
                "days": days,
                "count": pruned.len(),
                "entries": pruned,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("id");
            for id in pruned {
                println!("{}", id);
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            if pruned.is_empty() {
                println!("No entries to prune (all entries accessed within {} days).", days);
            } else if dry_run {
                println!("Would archive {} entries not accessed in {} days:", pruned.len(), days);
                for id in pruned {
                    println!("  - {}", id);
                }
            } else {
                println!("Archived {} entries not accessed in {} days:", pruned.len(), days);
                for id in pruned {
                    println!("  - {}", id);
                }
            }
        }
    }
}

fn format_evolution_history(history: &[EvolutionHistoryEntry], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(history).unwrap());
        }
        OutputFormat::Csv => {
            println!("doc_id,path,title,relationship,scope,reason");
            for h in history {
                println!(
                    "{},{},{},{},{},{}",
                    h.doc_id,
                    h.path,
                    h.title.as_deref().unwrap_or(""),
                    h.relationship,
                    h.scope.as_deref().unwrap_or(""),
                    h.reason.as_deref().unwrap_or(""),
                );
            }
        }
        OutputFormat::Markdown => {
            if history.is_empty() {
                println!("No evolution history found.");
            } else {
                println!("| ID | Path | Title | Relationship | Scope | Reason |");
                println!("|----|------|-------|--------------|-------|--------|");
                for h in history {
                    println!(
                        "| {} | {} | {} | {} | {} | {} |",
                        h.doc_id,
                        h.path,
                        h.title.as_deref().unwrap_or("-"),
                        h.relationship,
                        h.scope.as_deref().unwrap_or("-"),
                        h.reason.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        OutputFormat::Text => {
            if history.is_empty() {
                println!("No evolution history found.");
            } else {
                println!("Evolution history ({} relationships):", history.len());
                for h in history {
                    let title = h.title.as_deref().unwrap_or("(untitled)");
                    println!(
                        "  [{}] {} - {} ({})",
                        h.doc_id, h.path, title, h.relationship
                    );
                    if let Some(scope) = &h.scope {
                        println!("      Scope: {}", scope);
                    }
                    if let Some(reason) = &h.reason {
                        println!("      Reason: {}", reason);
                    }
                }
            }
        }
    }
}

fn format_current_document(doc: &mdkb::domain::Document, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(doc).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,collection,path,title");
            println!(
                "{},{},{},{}",
                doc.id,
                doc.collection,
                doc.relative_path,
                doc.title.as_deref().unwrap_or(""),
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            let title = doc.title.as_deref().unwrap_or("(untitled)");
            println!("Current version: [{}] {}:{} - {}", doc.id, doc.collection, doc.relative_path, title);
        }
    }
}

fn format_superseded_by(evolutions: &[Evolution], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(evolutions).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,source_doc_id,relationship,scope,reason");
            for e in evolutions {
                println!(
                    "{},{},{},{},{}",
                    e.id,
                    e.source_doc_id,
                    e.relationship,
                    e.scope.as_deref().unwrap_or(""),
                    e.reason.as_deref().unwrap_or(""),
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            if evolutions.is_empty() {
                println!("This document has not been superseded.");
            } else {
                println!("Superseded by ({} relationships):", evolutions.len());
                for e in evolutions {
                    println!("  - Doc #{} ({}", e.source_doc_id, e.relationship);
                    if let Some(scope) = &e.scope {
                        println!("    Scope: {}", scope);
                    }
                    if let Some(reason) = &e.reason {
                        println!("    Reason: {}", reason);
                    }
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

fn format_metrics_summary(metrics: &mdkb::store::stats::QueryMetricsSummary, period: u32, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(metrics).unwrap_or_default());
        }
        OutputFormat::Csv => {
            println!("total_queries,zero_result_rate,re_search_rate,latency_p50,latency_p95,latency_p99");
            println!(
                "{},{:.1},{:.1},{},{},{}",
                metrics.total_queries, metrics.zero_result_rate, metrics.re_search_rate,
                metrics.latency_p50, metrics.latency_p95, metrics.latency_p99
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("=== Query Metrics (last {} days) ===\n", period);
            println!("Total queries: {}", metrics.total_queries);
            println!("Zero-result rate: {:.1}%{}", metrics.zero_result_rate,
                if metrics.zero_result_rate > 10.0 { " ⚠️ High - queries not finding results" } else { "" });
            println!("Re-search rate: {:.1}%{}", metrics.re_search_rate,
                if metrics.re_search_rate > 15.0 { " ⚠️ High - initial results may be poor" } else { "" });
            println!();
            println!("Latency:");
            println!("  p50: {}ms", metrics.latency_p50);
            println!("  p95: {}ms", metrics.latency_p95);
            println!("  p99: {}ms{}", metrics.latency_p99,
                if metrics.latency_p99 > 500 { " ⚠️ Slow - performance issue" } else { "" });
            println!();
            println!("Score distribution:");
            println!("  > 0.8: {:.1}%", metrics.score_above_80);
            println!("  0.5-0.8: {:.1}%", metrics.score_50_to_80);
            println!("  < 0.5: {:.1}%", metrics.score_below_50);
        }
    }
}

fn format_latency_stats(stats: &[mdkb::store::stats::QueryLatencyStats], period: u32, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(stats).unwrap_or_default());
        }
        OutputFormat::Csv => {
            println!("search_type,count,avg_latency_ms,max_latency_ms,zero_result_count");
            for s in stats {
                println!(
                    "{},{},{:.1},{},{}",
                    s.search_type, s.count, s.avg_latency_ms, s.max_latency_ms, s.zero_result_count
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("=== Latency Breakdown (last {} days) ===\n", period);
            if stats.is_empty() {
                println!("No query data available.");
            } else {
                for s in stats {
                    println!("{} search:", s.search_type);
                    println!("  Total queries: {}", s.count);
                    println!("  Avg latency: {:.1}ms", s.avg_latency_ms);
                    println!("  Max latency: {}ms", s.max_latency_ms);
                    println!("  Zero results: {} ({:.1}%)",
                        s.zero_result_count,
                        if s.count > 0 { (s.zero_result_count as f64 / s.count as f64) * 100.0 } else { 0.0 });
                    println!();
                }
            }
        }
    }
}

fn format_quality_metrics(metrics: &mdkb::store::stats::QueryMetricsSummary, period: u32, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let quality = serde_json::json!({
                "period_days": period,
                "total_queries": metrics.total_queries,
                "zero_result_rate": metrics.zero_result_rate,
                "re_search_rate": metrics.re_search_rate,
                "score_distribution": {
                    "above_80": metrics.score_above_80,
                    "50_to_80": metrics.score_50_to_80,
                    "below_50": metrics.score_below_50
                }
            });
            println!("{}", serde_json::to_string_pretty(&quality).unwrap_or_default());
        }
        OutputFormat::Csv => {
            println!("period_days,total_queries,zero_result_rate,re_search_rate,score_above_80,score_50_to_80,score_below_50");
            println!("{},{},{:.1},{:.1},{:.1},{:.1},{:.1}",
                period, metrics.total_queries, metrics.zero_result_rate, metrics.re_search_rate,
                metrics.score_above_80, metrics.score_50_to_80, metrics.score_below_50);
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("=== Search Quality Analysis (last {} days) ===\n", period);
            println!("Total queries analyzed: {}\n", metrics.total_queries);

            // Zero result analysis
            println!("Zero-result rate: {:.1}%", metrics.zero_result_rate);
            if metrics.zero_result_rate > 10.0 {
                println!("  ⚠️  Consider:");
                println!("    - Adding more content to the knowledge base");
                println!("    - Checking query terms for typos");
                println!("    - Expanding synonyms in indexed content");
            }
            println!();

            // Re-search analysis
            println!("Re-search rate: {:.1}%", metrics.re_search_rate);
            if metrics.re_search_rate > 15.0 {
                println!("  ⚠️  Consider:");
                println!("    - Improving document titles");
                println!("    - Adding better keywords to content");
                println!("    - Reviewing search result ordering");
            }
            println!();

            // Score distribution
            println!("Score distribution (queries with results):");
            println!("  Excellent (> 0.8): {:.1}%", metrics.score_above_80);
            println!("  Good (0.5-0.8):    {:.1}%", metrics.score_50_to_80);
            println!("  Poor (< 0.5):      {:.1}%", metrics.score_below_50);
            if metrics.score_below_50 > 20.0 {
                println!("\n  ⚠️  High percentage of poor-scoring results.");
                println!("    Consider improving content quality or relevance.");
            }
        }
    }
}

fn format_metrics_export(events: &[mdkb::store::stats::QueryEvent], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(events).unwrap_or_default());
        }
        OutputFormat::Csv => {
            println!("query_hash,query_text,search_type,result_count,latency_ms,top_score");
            for e in events {
                println!("{},{},{},{},{},{}",
                    e.query_hash,
                    e.query_text.replace(',', ";"),
                    e.search_type,
                    e.result_count,
                    e.latency_ms,
                    e.top_score.map(|s| format!("{:.3}", s)).unwrap_or_default());
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Query events ({} total):\n", events.len());
            for (i, e) in events.iter().take(20).enumerate() {
                println!("{}. \"{}\"", i + 1, e.query_text);
                println!("   Type: {} | Results: {} | Latency: {}ms | Score: {}",
                    e.search_type, e.result_count, e.latency_ms,
                    e.top_score.map(|s| format!("{:.2}", s)).unwrap_or_else(|| "N/A".to_string()));
            }
            if events.len() > 20 {
                println!("\n... and {} more events (use --json or --csv for full export)", events.len() - 20);
            }
        }
    }
}
