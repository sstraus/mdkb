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
    Context, EmbedResult, EvolutionHistoryEntry, StatsResult, handle_collection_add,
    handle_collection_remove, handle_collection_rename, handle_current, handle_embed, handle_evolve_corrects,
    handle_evolve_extends, handle_evolve_retracts, handle_evolve_supersedes, handle_evolve_updates,
    handle_experiment_cancel, handle_experiment_create, handle_experiment_end, handle_experiment_list,
    handle_experiment_status, handle_get, handle_history, handle_hybrid_search, handle_init, handle_memory_add,
    handle_memory_import, handle_memory_list, handle_memory_prune, handle_memory_rm, handle_memory_search, handle_memory_show,
    handle_memory_warmup, handle_metrics_export, handle_metrics_latency, handle_metrics_show,
    handle_mget, handle_session_index, handle_stats, handle_status, handle_superseded_by, handle_update,
};
use mdkb::cli::{Cli, CollectionCommand, Command, EvolveCommand, ExperimentCommand, JournalCommand, MemoryCommand, MetricsCommand, OutputFormat, SessionCommand, SetupCommand, SetupMcpCommand};
use mdkb::cli::CodeCommand;
use mdkb::cli::journal::JournalImportResult;
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
            scope,
            kind,
            file,
        } => {
            let ctx = Context::open(&cwd)?;
            match scope.as_deref() {
                Some("docs") => {
                    let results = handle_hybrid_search(&ctx, &query, limit, collection.as_deref(), include_superseded)?;
                    format_search_results(&results, cli.format);
                }
                Some("memory") => {
                    let entries = handle_memory_search(&ctx, &query, limit)?;
                    format_memory_list(&entries, cli.format);
                }
                None => {
                    // Default: search docs + memory
                    let results = handle_hybrid_search(&ctx, &query, limit, collection.as_deref(), include_superseded)?;
                    let entries = handle_memory_search(&ctx, &query, limit)?;
                    if !results.is_empty() {
                        println!("## Documents\n");
                        format_search_results(&results, cli.format);
                    }
                    if !entries.is_empty() {
                        println!("## Memory Entries\n");
                        format_memory_list(&entries, cli.format);
                    }
                    if results.is_empty() && entries.is_empty() {
                        println!("No results found.");
                    }
                }
                Some("code") => {
                    let symbols = mdkb::cli::handlers::handle_code_search(
                        &cwd,
                        &query,
                        limit,
                        kind.as_deref(),
                    )?;
                    format_code_symbols(&symbols, cli.format);
                }
                Some("symbols") => {
                    let symbols = mdkb::cli::handlers::handle_code_find(
                        &cwd,
                        &query,
                        kind.as_deref(),
                        file.as_deref(),
                    )?;
                    format_code_symbols(&symbols, cli.format);
                }
                Some(invalid) => {
                    eprintln!("Invalid scope: '{}'. Valid values: docs, memory, code, symbols. Omit for docs+memory.", invalid);
                    std::process::exit(1);
                }
            }
        }
        Command::Get { id, lines } => {
            use mdkb::cli::handlers::GetResult;
            let ctx = Context::open(&cwd)?;

            // Detect glob pattern (contains * or ?)
            if id.contains('*') || id.contains('?') {
                let results = handle_mget(&ctx, &id, None)?;
                format_mget_results(&results, cli.format);
            }
            // Detect comma-separated list (e.g., "42,43,44")
            else if id.contains(',') {
                let ids: Vec<&str> = id.split(',').map(|s| s.trim()).collect();
                for single_id in ids {
                    match handle_get(&ctx, single_id, lines.as_deref()) {
                        Ok(GetResult::Document(doc, content)) => {
                            format_document(&doc, &content, cli.format);
                        }
                        Ok(GetResult::Memory(entry)) => {
                            format_memory_entry(&entry, cli.format);
                        }
                        Err(e) => {
                            eprintln!("Error getting '{}': {}", single_id, e);
                        }
                    }
                }
            }
            // Single ID/path/slug
            else {
                match handle_get(&ctx, &id, lines.as_deref())? {
                    GetResult::Document(doc, content) => {
                        format_document(&doc, &content, cli.format);
                    }
                    GetResult::Memory(entry) => {
                        format_memory_entry(&entry, cli.format);
                    }
                }
            }
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
            let result = handle_status(&ctx)?;
            format_status(&result, cli.format);
        }
        Command::Update => {
            let ctx = Context::open(&cwd)?;
            let result = handle_update(&ctx, &cwd)?;
            format_update_result(&result, cli.format);

            // Also reindex code (matching MCP update behavior)
            match mdkb::cli::handlers::handle_code_index(&cwd, &[]) {
                Ok(stats) => {
                    println!("\nCode index:");
                    format_code_index_stats(&stats, cli.format);
                }
                Err(e) => {
                    tracing::warn!("Code reindexing failed: {:?}", e);
                    eprintln!("Warning: code reindexing failed: {}", e);
                }
            }

            // Also reindex Claude Code sessions
            let sessions_base = std::path::PathBuf::from(
                std::env::var("HOME").unwrap_or_default(),
            )
            .join(".claude/projects");
            let project_root = cwd.to_string_lossy().to_string();
            match handle_session_index(&ctx, &sessions_base, &project_root) {
                Ok(sr) if sr.added > 0 || sr.updated > 0 => {
                    println!(
                        "\nSessions: {} added, {} updated, {} unchanged",
                        sr.added, sr.updated, sr.unchanged
                    );
                }
                Ok(_) => {} // no sessions or nothing changed — silent
                Err(e) => {
                    tracing::warn!("Session indexing failed: {:?}", e);
                }
            }
        }
        Command::Embed => {
            let ctx = Context::open(&cwd)?;
            let result = handle_embed(&ctx)?;
            format_embed_result(&result, cli.format);
        }
        Command::Serve { http, https, bind, token } => {
            let transport = if https {
                mdkb::mcp::server::TransportMode::Https {
                    bind: bind.unwrap_or_else(|| "127.0.0.1:8443".to_string()),
                    token,
                }
            } else if http {
                mdkb::mcp::server::TransportMode::Http {
                    bind: bind.unwrap_or_else(|| "127.0.0.1:8080".to_string()),
                    token,
                }
            } else {
                if bind.is_some() || token.is_some() {
                    eprintln!("Warning: --bind and --token are only used with --http or --https");
                }
                mdkb::mcp::server::TransportMode::Stdio
            };
            run_server(cwd, transport).await?;
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
                    const MAX_STDIN_SIZE: u64 = 100_000; // 100KB limit
                    let content = content.unwrap_or_else(|| {
                        use std::io::Read;
                        let mut buf = String::new();
                        std::io::stdin()
                            .take(MAX_STDIN_SIZE)
                            .read_to_string(&mut buf)
                            .unwrap_or_default();
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
                MemoryCommand::History { id } => {
                    let revisions = mdkb::store::memory::get_revisions(&ctx.conn, &id)?;
                    if revisions.is_empty() {
                        println!("No revision history for '{id}'");
                    } else {
                        println!("Revision history for '{}' ({} revision{}):\n",
                            id, revisions.len(), if revisions.len() == 1 { "" } else { "s" });
                        for (i, rev) in revisions.iter().enumerate() {
                            let date = chrono::DateTime::<chrono::Utc>::from_timestamp(rev.created_at, 0)
                                .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                                .unwrap_or_else(|| "?".to_string());
                            println!("--- Revision {} ({}) ---\n{}\n", i + 1, date, rev.diff);
                        }
                    }
                }
                MemoryCommand::Import { path, dry_run, skip_duplicates } => {
                    let result = handle_memory_import(&ctx, &path, dry_run, skip_duplicates)?;
                    if dry_run {
                        println!("Dry run: would import {} entries", result.imported);
                    } else {
                        println!("Imported {} entries", result.imported);
                    }
                    if result.skipped > 0 {
                        println!("Skipped {} duplicates", result.skipped);
                    }
                    for err in &result.errors {
                        eprintln!("Error: {err}");
                    }
                }
                MemoryCommand::Prune { days, dry_run } => {
                    let pruned = handle_memory_prune(&ctx, days, dry_run)?;
                    format_prune_result(&pruned, days, dry_run, cli.format);
                }
                #[cfg(feature = "llm")]
                MemoryCommand::Condense { tag, dry_run, interactive: _, min_entries } => {
                    let result = mdkb::cli::handlers::handle_memory_condense(
                        &ctx,
                        tag.as_deref(),
                        dry_run,
                        min_entries,
                    )?;
                    format_condense_result(&result, dry_run, cli.format);
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
        Command::Experiment(cmd) => {
            let ctx = Context::open(&cwd)?;
            match cmd {
                ExperimentCommand::Create {
                    name,
                    config_a,
                    config_b,
                    description,
                    split,
                    min_samples,
                } => {
                    let result = handle_experiment_create(
                        &ctx,
                        &name,
                        description.as_deref(),
                        &config_a,
                        &config_b,
                        split,
                        min_samples,
                    )?;
                    println!("Created experiment '{}' (ID: {})", result.name, result.id);
                }
                ExperimentCommand::Status { name } => {
                    if let Some(status) = handle_experiment_status(&ctx, &name)? {
                        format_experiment_status(&status, cli.format);
                    } else {
                        println!("Experiment '{name}' not found");
                    }
                }
                ExperimentCommand::End { name, winner } => {
                    let actual_winner = handle_experiment_end(&ctx, &name, winner.as_deref())?;
                    if let Some(w) = actual_winner {
                        println!("Experiment '{name}' ended with winner: {w}");
                    } else {
                        println!("Experiment '{name}' ended with no significant winner");
                    }
                }
                ExperimentCommand::Cancel { name } => {
                    handle_experiment_cancel(&ctx, &name)?;
                    println!("Experiment '{name}' cancelled");
                }
                ExperimentCommand::List { running } => {
                    let experiments = handle_experiment_list(&ctx, running)?;
                    format_experiment_list(&experiments, cli.format);
                }
            }
        }
        Command::Journal(cmd) => {
            let ctx = Context::open(&cwd)?;
            match cmd {
                JournalCommand::Import { path, dry_run } => {
                    let result = mdkb::cli::handlers::handle_journal_import(
                        &ctx,
                        std::path::Path::new(&path),
                        dry_run,
                    )?;
                    format_journal_import_result(&result, dry_run, cli.format);
                }
                JournalCommand::ImportAll { dir, dry_run, skip_existing } => {
                    let journal_dir = dir.unwrap_or_else(|| ".claude/journal".to_string());
                    let results = mdkb::cli::handlers::handle_journal_import_all(
                        &ctx,
                        std::path::Path::new(&journal_dir),
                        dry_run,
                        skip_existing,
                    )?;
                    format_journal_import_all_results(&results, dry_run, cli.format);
                }
            }
        }
        Command::Code(cmd) => {
            match cmd {
                CodeCommand::Init => {
                    mdkb::cli::handlers::handle_code_init(&cwd)?;
                    println!("Initialized code index at .mdkb/code.sqlite");
                }
                CodeCommand::Index { paths } => {
                    let stats = mdkb::cli::handlers::handle_code_index(&cwd, &paths)?;
                    format_code_index_stats(&stats, cli.format);
                }
                CodeCommand::Search { query, limit, kind } => {
                    let symbols = mdkb::cli::handlers::handle_code_search(
                        &cwd,
                        &query,
                        limit,
                        kind.as_deref(),
                    )?;
                    format_code_symbols(&symbols, cli.format);
                }
                CodeCommand::Find { name, kind, file } => {
                    let symbols = mdkb::cli::handlers::handle_code_find(
                        &cwd,
                        &name,
                        kind.as_deref(),
                        file.as_deref(),
                    )?;
                    format_code_symbols(&symbols, cli.format);
                }
                CodeCommand::Calls { name } => {
                    let (source, callees) =
                        mdkb::cli::handlers::handle_code_calls(&cwd, &name)?;
                    format_code_graph("Calls", &source, &callees, cli.format);
                }
                CodeCommand::Callers { name } => {
                    let (target, callers) =
                        mdkb::cli::handlers::handle_code_callers(&cwd, &name)?;
                    format_code_graph("Called by", &target, &callers, cli.format);
                }
                CodeCommand::Impact { name, depth } => {
                    let (source, impacted) =
                        mdkb::cli::handlers::handle_code_impact(&cwd, &name, depth)?;
                    format_code_graph("Impact radius", &source, &impacted, cli.format);
                }
                CodeCommand::Info => {
                    let info = mdkb::cli::handlers::handle_code_info(&cwd)?;
                    format_code_info(&info, cli.format);
                }
                CodeCommand::Parse { file } => {
                    let symbols = mdkb::cli::handlers::handle_code_parse(
                        std::path::Path::new(&file),
                    )?;
                    format_code_parse(&symbols, &file, cli.format);
                }
            }
        }
        Command::Setup(cmd) => {
            match cmd {
                SetupCommand::Mcp(mcp_cmd) => {
                    match mcp_cmd {
                        SetupMcpCommand::Claude { scope, yes } => {
                            let global = scope == "user";
                            if scope != "local" && scope != "user" && scope != "project" {
                                eprintln!("Error: Invalid scope '{}'. Must be 'local', 'user', or 'project'.", scope);
                                std::process::exit(1);
                            }
                            let result = mdkb::cli::setup::handle_setup_mcp_claude(&cwd, global, yes)?;
                            if result.success {
                                println!("{}", result.message);
                                println!();
                                println!("Restart Claude Code to activate the mdkb MCP server.");
                            } else {
                                println!("{}", result.message);
                            }
                        }
                    }
                }
            }
        }
        Command::Session(cmd) => {
            match cmd {
                SessionCommand::Index { sessions_path, project_root } => {
                    let ctx = Context::open(&cwd)?;
                    let sessions_base = sessions_path
                        .map(std::path::PathBuf::from)
                        .unwrap_or_else(|| {
                            std::path::PathBuf::from(
                                std::env::var("HOME").unwrap_or_default(),
                            )
                            .join(".claude/projects")
                        });
                    let root = project_root.unwrap_or_else(|| cwd.to_string_lossy().to_string());
                    let result = handle_session_index(&ctx, &sessions_base, &root)?;
                    format_update_result(&result, cli.format);
                }
            }
        }
    }

    Ok(())
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

fn format_status(result: &mdkb::cli::handlers::StatusResult, format: OutputFormat) {
    let status = &result.index;
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "index": status,
                "collections": result.collections.iter().map(|c| serde_json::json!({
                    "name": c.name,
                    "path": c.path,
                    "pattern": c.pattern,
                    "source": c.source,
                    "doc_count": c.doc_count,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("collections,documents,stale,db_size_bytes");
            println!(
                "{},{},{},{}",
                status.collections, status.documents, status.stale_documents, status.db_size_bytes
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Documents:   {}", status.documents);
            println!("Stale:       {}", status.stale_documents);
            println!("DB Size:     {} bytes", status.db_size_bytes);
            if let Some(ts) = status.last_updated {
                println!("Last Update: {}", ts);
            }
            println!("\nCollections ({}):", result.collections.len());
            if result.collections.is_empty() {
                println!("  (none)");
            } else {
                for c in &result.collections {
                    let tag = if c.source == "convention" { "[convention]" } else { "[manual]" };
                    println!("  - {} {} ({}): {} docs, pattern: {}", c.name, tag, c.path, c.doc_count, c.pattern);
                }
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

#[cfg(feature = "llm")]
fn format_condense_result(result: &mdkb::cli::handlers::CondenseResult, dry_run: bool, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "dry_run": dry_run,
                "groups": result.groups.iter().map(|g| {
                    serde_json::json!({
                        "entry_ids": g.entry_ids,
                        "common_tags": g.common_tags,
                        "proposed_id": g.proposed_id,
                        "proposed_title": g.proposed_title,
                    })
                }).collect::<Vec<_>>(),
                "consolidated_count": result.consolidated_count,
                "merged_count": result.merged_count,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("proposed_id,entry_ids,common_tags");
            for g in &result.groups {
                println!("{},{},{}",
                    g.proposed_id,
                    g.entry_ids.join(";"),
                    g.common_tags.join(";")
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            if result.groups.is_empty() {
                println!("No groups of related entries found to condense.");
                return;
            }

            if dry_run {
                println!("=== Proposed Consolidations (dry run) ===\n");
            } else {
                println!("=== Consolidation Complete ===\n");
            }

            for (i, g) in result.groups.iter().enumerate() {
                println!("Group {}: {} entries -> {}", i + 1, g.entry_ids.len(), g.proposed_id);
                println!("  Tags: {}", g.common_tags.join(", "));
                println!("  Entries:");
                for id in &g.entry_ids {
                    println!("    - {}", id);
                }
                if let Some(title) = &g.proposed_title {
                    println!("  Proposed title: {}", title);
                }
                println!();
            }

            if !dry_run {
                println!("Consolidated {} entries into {} merged entries.",
                    result.consolidated_count, result.merged_count);
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

fn format_experiment_status(status: &mdkb::store::stats::ExperimentStatusReport, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(status).unwrap_or_default());
        }
        OutputFormat::Csv => {
            println!("experiment,variant,sample_count,avg_score,avg_latency_ms,p95_latency_ms,zero_result_rate");
            println!("{},A,{},{:.3},{:.1},{},{}",
                status.experiment.name,
                status.variant_a.sample_count,
                status.variant_a.avg_score,
                status.variant_a.avg_latency_ms,
                status.variant_a.p95_latency_ms,
                status.variant_a.zero_result_rate);
            println!("{},B,{},{:.3},{:.1},{},{}",
                status.experiment.name,
                status.variant_b.sample_count,
                status.variant_b.avg_score,
                status.variant_b.avg_latency_ms,
                status.variant_b.p95_latency_ms,
                status.variant_b.zero_result_rate);
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            let exp = &status.experiment;
            let started = chrono::DateTime::from_timestamp(exp.created_at, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".to_string());

            println!("Experiment: {} ({} since {})", exp.name, exp.status, started);
            if let Some(desc) = &exp.description {
                println!("Description: {desc}");
            }
            println!();

            // Variant A
            let a = &status.variant_a;
            println!("Variant A: avg score {:.3}, p95 latency {}ms, n={}",
                a.avg_score, a.p95_latency_ms, a.sample_count);
            println!("  Config: {}", exp.config_a);

            // Variant B
            let b = &status.variant_b;
            println!("Variant B: avg score {:.3}, p95 latency {}ms, n={}",
                b.avg_score, b.p95_latency_ms, b.sample_count);
            println!("  Config: {}", exp.config_b);
            println!();

            // Significance
            if !status.has_min_samples {
                let needed = exp.min_sample_size - a.sample_count.min(b.sample_count);
                println!("Significance: Need {} more samples before statistical analysis", needed.max(0));
            } else if let Some(sig) = &status.significance {
                if sig.significant {
                    println!("Significance: {:.0}% confidence {} has better quality (p={:.4}, effect size={:.2})",
                        sig.confidence_level,
                        sig.winner.as_deref().unwrap_or("?"),
                        sig.p_value,
                        sig.effect_size);
                } else {
                    println!("Significance: No significant difference detected (p={:.4})", sig.p_value);
                }
            }
        }
    }
}

fn format_experiment_list(experiments: &[mdkb::store::stats::Experiment], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(experiments).unwrap_or_default());
        }
        OutputFormat::Csv => {
            println!("name,status,traffic_split,min_samples,created_at,winner");
            for exp in experiments {
                println!("{},{},{},{},{},{}",
                    exp.name,
                    exp.status,
                    exp.traffic_split,
                    exp.min_sample_size,
                    exp.created_at,
                    exp.winner.as_deref().unwrap_or(""));
            }
        }
        OutputFormat::Markdown => {
            println!("| Name | Status | Split | Min Samples | Winner |");
            println!("|------|--------|-------|-------------|--------|");
            for exp in experiments {
                println!("| {} | {} | {:.0}% | {} | {} |",
                    exp.name,
                    exp.status,
                    exp.traffic_split * 100.0,
                    exp.min_sample_size,
                    exp.winner.as_deref().unwrap_or("-"));
            }
        }
        OutputFormat::Text => {
            if experiments.is_empty() {
                println!("No experiments found.");
            } else {
                for exp in experiments {
                    let started = chrono::DateTime::from_timestamp(exp.created_at, 0)
                        .map(|dt| dt.format("%Y-%m-%d").to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let winner_str = exp.winner.as_ref()
                        .map(|w| format!(" -> Winner: {w}"))
                        .unwrap_or_default();
                    println!("{}: {} (started {}){}", exp.name, exp.status, started, winner_str);
                }
            }
        }
    }
}

fn format_journal_import_result(result: &JournalImportResult, dry_run: bool, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(result).unwrap());
        }
        OutputFormat::Csv => {
            println!("source,created,skipped");
            println!("{},{},{}", result.source_path, result.created.len(), result.skipped.len());
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            let prefix = if dry_run { "[DRY RUN] " } else { "" };
            println!("{}Imported: {}", prefix, result.source_path);
            if !result.created.is_empty() {
                println!("  Created {} memory entries:", result.created.len());
                for id in &result.created {
                    println!("    - {}", id);
                }
            }
            if !result.skipped.is_empty() {
                println!("  Skipped {} entries:", result.skipped.len());
                for (reason, name) in &result.skipped {
                    println!("    - {}: {}", name, reason);
                }
            }
        }
    }
}

fn format_journal_import_all_results(results: &[JournalImportResult], dry_run: bool, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(results).unwrap());
        }
        OutputFormat::Csv => {
            println!("source,created,skipped");
            for result in results {
                println!("{},{},{}", result.source_path, result.created.len(), result.skipped.len());
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            let prefix = if dry_run { "[DRY RUN] " } else { "" };
            let total_created: usize = results.iter().map(|r| r.created.len()).sum();
            let total_skipped: usize = results.iter().map(|r| r.skipped.len()).sum();

            println!("{}Journal Import Summary", prefix);
            println!("  Files processed: {}", results.len());
            println!("  Entries created: {}", total_created);
            println!("  Entries skipped: {}", total_skipped);

            if total_created > 0 {
                println!("\nCreated:");
                for result in results {
                    for id in &result.created {
                        println!("  - {}", id);
                    }
                }
            }

            let errors: Vec<_> = results.iter()
                .flat_map(|r| r.skipped.iter().filter(|(reason, _)| reason.starts_with("Error")))
                .collect();

            if !errors.is_empty() {
                println!("\nErrors:");
                for (reason, path) in errors {
                    println!("  - {}: {}", path, reason);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Code intelligence format functions
// ---------------------------------------------------------------------------

fn format_code_index_stats(stats: &mdkb::code::indexing::types::IndexStats, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "files_discovered": stats.files_discovered,
                "files_indexed": stats.files_indexed,
                "symbols_indexed": stats.symbols_indexed,
                "relationships_collected": stats.relationships_collected,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("files_discovered,files_indexed,symbols_indexed,relationships");
            println!(
                "{},{},{},{}",
                stats.files_discovered,
                stats.files_indexed,
                stats.symbols_indexed,
                stats.relationships_collected,
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Files discovered: {}", stats.files_discovered);
            println!("Files indexed:    {}", stats.files_indexed);
            println!("Symbols indexed:  {}", stats.symbols_indexed);
            println!("Relationships:    {}", stats.relationships_collected);
        }
    }
}

fn format_code_symbols(symbols: &[mdkb::code::symbol::Symbol], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(symbols).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,name,kind,file,line");
            for s in symbols {
                println!(
                    "{},{},{},{},{}",
                    s.id.value(),
                    s.name,
                    s.kind,
                    s.file_path,
                    s.range.start_line,
                );
            }
        }
        OutputFormat::Markdown => {
            println!("| ID | Name | Kind | File | Line |");
            println!("|----|------|------|------|------|");
            for s in symbols {
                let sig = s.signature.as_deref().unwrap_or("");
                println!(
                    "| {} | `{}` | {} | {} | {} |",
                    s.id.value(),
                    if sig.is_empty() { s.name.as_ref() } else { sig },
                    s.kind,
                    s.file_path,
                    s.range.start_line,
                );
            }
        }
        OutputFormat::Text => {
            if symbols.is_empty() {
                println!("No symbols found.");
            } else {
                for s in symbols {
                    print!(
                        "  sym#{} {} {} in {}:{}",
                        s.id.value(),
                        s.kind,
                        s.name,
                        s.file_path,
                        s.range.start_line,
                    );
                    if let Some(ref sig) = s.signature {
                        print!("  sig: {}", sig);
                    }
                    println!();
                }
            }
        }
    }
}

fn format_code_graph(
    label: &str,
    source: &mdkb::code::symbol::Symbol,
    related: &[mdkb::code::symbol::Symbol],
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "source": source,
                "relationship": label,
                "targets": related,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("source,relationship,target,target_kind,file,line");
            for t in related {
                println!(
                    "{},{},{},{},{},{}",
                    source.name,
                    label,
                    t.name,
                    t.kind,
                    t.file_path,
                    t.range.start_line,
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!(
                "{} for {} {} ({}:{})",
                label,
                source.kind,
                source.name,
                source.file_path,
                source.range.start_line,
            );
            if related.is_empty() {
                println!("  (none)");
            } else {
                for t in related {
                    println!(
                        "  - {} {} ({}:{})",
                        t.kind, t.name, t.file_path, t.range.start_line,
                    );
                }
            }
        }
    }
}

fn format_code_info(info: &mdkb::cli::handlers::CodeInfoResult, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(info).unwrap());
        }
        OutputFormat::Csv => {
            println!("symbols,files,relationships");
            println!("{},{},{}", info.symbols, info.files, info.relationships);
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Code Index:");
            println!("  Symbols:       {}", info.symbols);
            println!("  Files:         {}", info.files);
            println!("  Relationships: {}", info.relationships);
        }
    }
}

fn format_code_parse(symbols: &[mdkb::code::symbol::Symbol], file: &str, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output: Vec<_> = symbols
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "name": s.name.as_ref(),
                        "kind": s.kind.to_string(),
                        "line": s.range.start_line,
                        "end_line": s.range.end_line,
                        "signature": s.signature.as_deref(),
                        "doc_comment": s.doc_comment.as_deref(),
                        "visibility": format!("{:?}", s.visibility),
                    })
                })
                .collect();
            // JSONL output for parse command
            for item in &output {
                println!("{}", item);
            }
        }
        OutputFormat::Csv => {
            println!("name,kind,line,end_line,visibility");
            for s in symbols {
                println!(
                    "{},{},{},{},{:?}",
                    s.name,
                    s.kind,
                    s.range.start_line,
                    s.range.end_line,
                    s.visibility,
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Parsed {} ({} symbols):\n", file, symbols.len());
            for s in symbols {
                print!(
                    "  L{}-{} {} {} {}",
                    s.range.start_line,
                    s.range.end_line,
                    s.kind,
                    s.name,
                    format!("{:?}", s.visibility).to_lowercase(),
                );
                if let Some(ref sig) = s.signature {
                    print!("  sig: {}", sig);
                }
                println!();
            }
        }
    }
}
