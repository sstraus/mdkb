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
use mdkb::cli::CodeCommand;
use mdkb::cli::daemon as daemon_cli;
use mdkb::cli::handlers::{
    EmbedResult, EvolutionHistoryEntry, handle_collection_add, handle_collection_list,
    handle_collection_remove, handle_collection_rename, handle_current, handle_embed,
    handle_eval_judge, handle_eval_recall, handle_evolve_corrects, handle_evolve_extends,
    handle_evolve_retracts, handle_evolve_supersedes, handle_evolve_updates,
    handle_experiment_cancel, handle_experiment_create, handle_experiment_end,
    handle_experiment_list, handle_experiment_status, handle_get, handle_graph_backlinks,
    handle_graph_dangling, handle_graph_hubs, handle_graph_links, handle_graph_neighbors,
    handle_graph_path, handle_history, handle_hybrid_search, handle_init, handle_memory_add,
    handle_memory_confirm, handle_memory_export, handle_memory_import, handle_memory_import_dir,
    handle_memory_link, handle_memory_list, handle_memory_prune, handle_memory_rm,
    handle_memory_search, handle_memory_show, handle_memory_warmup, handle_metrics_export,
    handle_metrics_latency, handle_metrics_show, handle_mget, handle_prune_sessions,
    handle_session_index, handle_superseded_by, handle_update_files_force, handle_update_force,
    parse_retention_secs,
};
#[cfg(unix)]
use mdkb::cli::hook_client;
use mdkb::cli::hook_logic;
use mdkb::cli::journal::JournalImportResult;
use mdkb::cli::{
    Cli, CollectionCommand, Command, DaemonCommand, EvalCommand, EvolveCommand, ExperimentCommand,
    GraphCommand, HookCommand, JournalCommand, MemoryCommand, MetricsCommand, OutputFormat,
    RemoveHooksCommand, RemoveMcpCommand, SessionCommand, SetupCommand, SetupHooksCommand,
    SetupMcpCommand, SetupRemoveCommand,
};
use mdkb::core::Context;
use mdkb::mcp::server::run_server;
use mdkb::store::evolution::Evolution;
use mdkb::store::memory::MemoryEntry;
use rmcp::ServiceExt;

fn main() -> Result<()> {
    // `--detach` has to happen before any threads exist — tokio spawns
    // workers eagerly, and fork() with live threads is undefined behavior.
    // We detect the flag by argv and run the double-fork in a single
    // threaded world, then build the runtime in the grandchild.
    if should_detach_from_argv() {
        mdkb::cli::daemon::detach_current_process()?;
    }
    // Only the long-lived servers (`serve`, `mcp`) need a multi-thread runtime.
    // One-shot subcommands (hook, get, search, …) do a single round-trip and
    // start faster on a current-thread runtime with no worker-pool spin-up
    // (PERF-E1). spawn_blocking still uses the separate blocking pool.
    let mut builder = if wants_multi_thread_runtime_from_argv() {
        tokio::runtime::Builder::new_multi_thread()
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    let rt = builder
        .enable_all()
        .build()
        .map_err(|e| mdkb::Error::other(format!("build tokio runtime: {e}")))?;
    rt.block_on(run())
}

/// True iff the invocation is a long-lived server (`serve` or `mcp`) that
/// benefits from a multi-thread runtime. Every other subcommand is one-shot.
fn wants_multi_thread_runtime_from_argv() -> bool {
    is_server_invocation(env::args().skip(1))
}

/// Pure form of [`wants_multi_thread_runtime_from_argv`] over an arg iterator.
fn is_server_invocation<I: IntoIterator<Item = String>>(args: I) -> bool {
    args.into_iter().any(|arg| arg == "serve" || arg == "mcp")
}

/// True iff the invocation is `mdkb serve --daemon --detach` in any argv
/// order. Anything else — in particular `mdkb daemon restart`, which spawns
/// a detached daemon as a CHILD — does not detach the current process.
fn should_detach_from_argv() -> bool {
    let mut has_serve = false;
    let mut has_daemon = false;
    let mut has_detach = false;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "serve" => has_serve = true,
            "--daemon" => has_daemon = true,
            "--detach" => has_detach = true,
            _ => {}
        }
    }
    has_serve && has_daemon && has_detach
}

async fn run() -> Result<()> {
    let cli = Cli::parse_args();
    let format = cli.format;

    let result = run_cli(cli).await;
    if let Err(ref e) = result {
        if matches!(format, OutputFormat::Json) {
            eprintln!("{}", serde_json::json!({"error": e.to_string()}));
            std::process::exit(1);
        }
    }
    result
}

async fn run_cli(cli: Cli) -> Result<()> {
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

    let raw_cwd = env::current_dir()?;
    // `init` creates a store exactly where it is invoked (explicit user intent).
    // Every other command anchors to the nearest existing `.mdkb/` store, then
    // the git root, then CLAUDE_PROJECT_DIR, then cwd — so running mdkb from a
    // sub-directory finds the project's store instead of spawning a new one.
    let cwd = if matches!(cli.command, Command::Init) {
        mdkb::git::resolve_main_worktree(&raw_cwd)
    } else {
        let hint = std::env::var_os("CLAUDE_PROJECT_DIR").map(std::path::PathBuf::from);
        mdkb::git::resolve_project_root(&raw_cwd, hint.as_deref())
    };
    let cwd = cwd.canonicalize().unwrap_or(cwd);

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
                CollectionCommand::List => {
                    let collections = handle_collection_list(&ctx)?;
                    format_collection_list(&collections, cli.format);
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
            entry_type,
        } => {
            let ctx = Context::open(&cwd)?;
            match scope.as_deref() {
                Some("docs") => {
                    let results = handle_hybrid_search(
                        &ctx,
                        &query,
                        limit,
                        collection.as_deref(),
                        include_superseded,
                    )?;
                    format_search_results(&results, cli.format);
                }
                Some("memory") => {
                    let entries = if let Some(ref et) = entry_type {
                        mdkb::store::memory::search_entries_by_type(&ctx.conn, &query, et, limit)?
                    } else {
                        handle_memory_search(&ctx, &query, limit)?
                    };
                    format_memory_list(&entries, cli.format);
                }
                None => {
                    // Default: search docs + memory
                    let results = handle_hybrid_search(
                        &ctx,
                        &query,
                        limit,
                        collection.as_deref(),
                        include_superseded,
                    )?;
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
                        if mdkb::store::search::index_is_empty(&ctx.conn) {
                            println!("{}", mdkb::store::search::INDEX_EMPTY_HINT);
                        }
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
                    eprintln!(
                        "Invalid scope: '{}'. Valid values: docs, memory, code, symbols. Omit for docs+memory.",
                        invalid
                    );
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
                let mut had_error = false;
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
                            had_error = true;
                        }
                    }
                }
                // Non-zero exit if any id failed — a batch get that printed only
                // per-id errors previously still exited 0 (BUG-E1).
                if had_error {
                    return Err(mdkb::Error::other(
                        "one or more requested ids could not be retrieved",
                    ));
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
        Command::Update { files, force } => {
            let ctx = Context::open(&cwd)?;

            if files.is_empty() {
                // Full reindex (--force ignores mtime so config changes reach all docs)
                let result = handle_update_force(&ctx, &cwd, force)?;
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
                match mdkb::daemon::config::home_dir() {
                    Ok(home) => {
                        let sessions_base = home.join(".claude/projects");
                        let project_root = cwd.to_string_lossy().to_string();
                        match handle_session_index(&ctx, &sessions_base, &project_root) {
                            Ok(sr)
                                if sr.added > 0 || sr.updated > 0 || sr.sessions_archived > 0 =>
                            {
                                println!(
                                    "\nSessions: {} added, {} updated, {} unchanged, {} archived",
                                    sr.added, sr.updated, sr.unchanged, sr.sessions_archived
                                );
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!("Session indexing failed: {:?}", e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Session indexing skipped: cannot resolve home dir: {e}");
                    }
                }
            } else {
                // Targeted file reindex (docs only — code index handles its own incremental)
                let result = handle_update_files_force(&ctx, &cwd, &files, force)?;
                format_update_result(&result, cli.format);

                // Also reindex code for the specified files
                match mdkb::cli::handlers::handle_code_index(&cwd, &files) {
                    Ok(stats) if stats.files_indexed > 0 => {
                        println!("\nCode index:");
                        format_code_index_stats(&stats, cli.format);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!("Code reindexing failed: {:?}", e);
                    }
                }
            }
        }
        Command::Embed { collection } => {
            let ctx = Context::open(&cwd)?;
            let result = handle_embed(&ctx, collection.as_deref())?;
            format_embed_result(&result, cli.format);
        }
        Command::Serve {
            http,
            https,
            bind,
            token,
            allow_no_auth,
            global,
            daemon,
            detach,
        } => {
            // A network transport with no token authenticates nothing — every
            // request is accepted. Refuse to start rather than silently expose
            // the repo; --allow-no-auth is the explicit opt-out.
            if (http || https) && token.is_none() && !allow_no_auth {
                return Err(mdkb::Error::other(
                    "--http/--https require --token; pass --allow-no-auth to run without \
                     authentication (accepts all requests).",
                ));
            }
            if daemon {
                let _ = detach;
                #[cfg(unix)]
                run_daemon().await?;
                #[cfg(not(unix))]
                return Err(mdkb::Error::other("Daemon mode requires Unix"));
            } else if global {
                // Global mode: single process serving multiple repos via MCP roots
                let daemon_config =
                    mdkb::DaemonConfig::load_or_default(&mdkb::DaemonConfig::config_path())?;
                let registry =
                    std::sync::Arc::new(mdkb::daemon::registry::RepoRegistry::new(daemon_config));
                let server = mdkb::mcp::server::McpServer::global(registry);

                tracing::info!("Starting mdkb MCP server in global mode (stdio)...");
                let (stdin, stdout) = rmcp::transport::io::stdio();
                let service = server
                    .serve((stdin, stdout))
                    .await
                    .map_err(|e| mdkb::Error::other(format!("Failed to start server: {e}")))?;
                service
                    .waiting()
                    .await
                    .map_err(|e| mdkb::Error::other(format!("Server error: {e}")))?;
            } else {
                // Standalone mode: single repo from cwd (existing behavior).
                //
                // When launched from a legacy mcp.json entry (no flags, stdin
                // piped) we emit a one-line deprecation nudge on stderr so
                // operators know to migrate to `mdkb mcp`. The server itself
                // still runs in-process stdio exactly as before — no behavior
                // change, just guidance.
                if !http && !https && bind.is_none() && token.is_none() && stdin_is_not_tty() {
                    eprintln!(
                        "mdkb serve: legacy stdio mode is deprecated; \
                         please update your MCP config to `mdkb mcp`."
                    );
                }
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
                        eprintln!(
                            "Warning: --bind and --token are only used with --http or --https"
                        );
                    }
                    mdkb::mcp::server::TransportMode::Stdio
                };
                run_server(cwd, transport).await?;
            }
        }
        Command::Mcp { socket } => {
            if std::env::var_os("MDKB_NO_DAEMON").is_some() {
                // Bypass the daemon entirely: run rmcp in-process in global
                // mode, talking over stdio. Same wire protocol; the only
                // difference is the server instance lives in this process.
                let daemon_config =
                    mdkb::DaemonConfig::load_or_default(&mdkb::DaemonConfig::config_path())?;
                let registry =
                    std::sync::Arc::new(mdkb::daemon::registry::RepoRegistry::new(daemon_config));
                let server = mdkb::mcp::server::McpServer::global(registry);

                tracing::info!("Starting mdkb MCP in-process (MDKB_NO_DAEMON=1)...");
                let (stdin, stdout) = rmcp::transport::io::stdio();
                let service = server
                    .serve((stdin, stdout))
                    .await
                    .map_err(|e| mdkb::Error::other(format!("Failed to start server: {e}")))?;
                service
                    .waiting()
                    .await
                    .map_err(|e| mdkb::Error::other(format!("Server error: {e}")))?;
            } else {
                #[cfg(unix)]
                {
                    let socket_path = socket.unwrap_or_else(|| {
                        mdkb::DaemonConfig::load_or_default(&mdkb::DaemonConfig::config_path())
                            .map(|c| c.socket_path())
                            .unwrap_or_else(|_| {
                                mdkb::DaemonConfig::daemon_home().join("daemon.sock")
                            })
                    });
                    mdkb::cli::mcp_proxy::run_proxy(socket_path).await?;
                }
                #[cfg(not(unix))]
                {
                    let _ = socket;
                    return Err(mdkb::Error::other(
                        "Daemon proxy requires Unix (use MDKB_NO_DAEMON=1 for in-process mode)",
                    ));
                }
            }
        }
        Command::Stats { no_color } => {
            let ctx = Context::open(&cwd)?;
            let report = mdkb::cli::stats_report::collect_report(&ctx)?;
            if let mdkb::cli::OutputFormat::Json = cli.format {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                use std::io::IsTerminal;
                let color = !no_color && std::io::stdout().is_terminal();
                print!("{}", mdkb::cli::stats_render_report::render(&report, color));
            }
        }
        Command::Compact {
            prune_sessions,
            older_than,
            export,
        } => {
            let ctx = Context::open(&cwd)?;
            let mdkb_dir = ctx.db_path.parent().expect("db_path has parent");
            let _mutation_guard = mdkb::store::mutation_lock::acquire(&ctx.db_path, "compact")?;
            mdkb::store::heal::invalidate_marker(&ctx.db_path);

            if prune_sessions {
                let raw = older_than.ok_or_else(|| {
                    mdkb::Error::other(
                        "--prune-sessions requires --older-than <e.g. 90d> to avoid deleting recent archives",
                    )
                })?;
                let secs = parse_retention_secs(&raw)?;
                // Checked: `now - secs` must not wrap for a valid-but-huge `secs`.
                // A wrapped (future) cutoff would make every archived session
                // prunable — the opposite of a retention rail (SEC-1).
                let cutoff = chrono::Utc::now()
                    .timestamp()
                    .checked_sub(secs)
                    .ok_or_else(|| {
                        mdkb::Error::other(format!(
                            "--older-than '{raw}' is too large to compute a cutoff"
                        ))
                    })?;
                let summary = handle_prune_sessions(&ctx, cutoff, export.as_deref())?;
                if let Some(dir) = &summary.export_dir {
                    eprintln!(
                        "Pruned {} archived session(s); exported {} to {}",
                        summary.pruned, summary.exported, dir
                    );
                } else {
                    eprintln!("Pruned {} archived session(s)", summary.pruned);
                }
            }

            // Vacuum index.sqlite
            ctx.conn.execute_batch("VACUUM;")?;
            mdkb::store::heal::verify_and_mark(&ctx.conn, &ctx.db_path)?;
            let idx_size = ctx.db_path.metadata().map(|m| m.len()).unwrap_or(0);
            eprintln!("index.sqlite vacuumed ({} KB)", idx_size / 1024);

            // Vacuum code.sqlite if present
            let code_path = mdkb_dir.join("code.sqlite");
            if code_path.exists() {
                // Announce the connection before opening it: a quarantine that
                // renames the path mid-VACUUM recycles it onto a fresh database,
                // and SQLite derives -wal/-shm from the path — so this
                // connection's frames would land in the replacement's WAL.
                let _live = mdkb::store::mutation_lock::acquire_live_shared(&code_path)?;
                let conn = rusqlite::Connection::open(&code_path)?;
                conn.execute_batch("VACUUM;")?;
                let code_size = code_path.metadata().map(|m| m.len()).unwrap_or(0);
                eprintln!("code.sqlite vacuumed ({} KB)", code_size / 1024);
            }
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
        Command::Eval(cmd) => {
            let (report_json, summary) = match cmd {
                EvalCommand::Recall { fixture, k } => {
                    let r = handle_eval_recall(fixture.as_deref(), k)?;
                    (
                        serde_json::to_string_pretty(&r)?,
                        format!(
                            "recall@{}: {:.3}  MRR: {:.3}  (n={})",
                            r.k, r.recall_at_k, r.mrr, r.n
                        ),
                    )
                }
                EvalCommand::Judge { fixture, k } => {
                    let r = handle_eval_judge(fixture.as_deref(), k)?;
                    (
                        serde_json::to_string_pretty(&r)?,
                        format!("judge accuracy: {:.3}  (n={}, k={})", r.accuracy, r.n, r.k),
                    )
                }
            };
            match cli.format {
                OutputFormat::Json => println!("{report_json}"),
                _ => println!("{summary}"),
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
                    file,
                    ttl,
                    due_in,
                    source_type,
                } => {
                    let (content, source_path) = if let Some(ref path) = file {
                        let text = std::fs::read_to_string(path).map_err(|e| {
                            mdkb::Error::other(format!(
                                "Failed to read file {}: {e}",
                                path.display()
                            ))
                        })?;
                        let abs = path.canonicalize().unwrap_or_else(|_| path.clone());
                        (text, Some(abs.to_string_lossy().to_string()))
                    } else {
                        const MAX_STDIN_SIZE: u64 = 100_000;
                        let text = content.unwrap_or_else(|| {
                            use std::io::Read;
                            let mut buf = String::new();
                            std::io::stdin()
                                .take(MAX_STDIN_SIZE)
                                .read_to_string(&mut buf)
                                .unwrap_or_default();
                            buf
                        });
                        (text, None)
                    };
                    handle_memory_add(
                        &ctx,
                        &id,
                        &title,
                        &entry_type,
                        tags.as_deref(),
                        &content,
                        source_path.as_deref(),
                        ttl,
                        due_in,
                        source_type.as_deref(),
                    )?;
                    println!("Added memory entry '{id}'");
                }
                MemoryCommand::Show { id } => {
                    if let Some(entry) = handle_memory_show(&ctx, &id)? {
                        format_memory_entry(&entry, cli.format);
                    } else {
                        println!("Memory entry '{id}' not found");
                    }
                }
                MemoryCommand::Confirm { id, outcome } => {
                    let result = handle_memory_confirm(&ctx, &id, &outcome)?;
                    match cli.format {
                        mdkb::cli::OutputFormat::Json => {
                            println!("{}", serde_json::to_string_pretty(&result)?);
                        }
                        _ => println!("{}", result.message),
                    }
                }
                MemoryCommand::Link {
                    id,
                    relation,
                    target,
                    doc,
                    agent,
                } => {
                    handle_memory_link(&ctx, &id, &relation, &target, doc, agent.as_deref())?;
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
                        println!(
                            "Revision history for '{}' ({} revision{}):\n",
                            id,
                            revisions.len(),
                            if revisions.len() == 1 { "" } else { "s" }
                        );
                        for (i, rev) in revisions.iter().enumerate() {
                            let date =
                                chrono::DateTime::<chrono::Utc>::from_timestamp(rev.created_at, 0)
                                    .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
                                    .unwrap_or_else(|| "?".to_string());
                            println!("--- Revision {} ({}) ---\n{}\n", i + 1, date, rev.diff);
                        }
                    }
                }
                MemoryCommand::Export {
                    dir,
                    include_expired,
                    overwrite,
                    dry_run,
                } => {
                    let mdkb_dir = ctx.db_path.parent().expect("db_path has parent");
                    let out_dir = dir.unwrap_or_else(|| mdkb_dir.join("memory/entries"));
                    let result =
                        handle_memory_export(&ctx, &out_dir, include_expired, overwrite, dry_run)?;
                    if dry_run {
                        println!("Dry run: would export {} entries", result.exported);
                    } else {
                        println!("Exported {}", result.exported);
                    }
                    if result.skipped > 0 {
                        println!("Skipped {} (already exist)", result.skipped);
                    }
                    for err in &result.errors {
                        eprintln!("Error: {err}");
                    }
                }
                MemoryCommand::Import {
                    path,
                    dry_run,
                    skip_duplicates,
                } => {
                    let p = std::path::Path::new(&path);
                    let result = if p.is_dir() {
                        handle_memory_import_dir(&ctx, p, dry_run, skip_duplicates)?
                    } else {
                        handle_memory_import(&ctx, &path, dry_run, skip_duplicates)?
                    };
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
                MemoryCommand::Condense {
                    tag,
                    dry_run,
                    interactive: _,
                    min_entries,
                } => {
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
                EvolveCommand::Updates {
                    new,
                    old,
                    scope,
                    reason,
                } => {
                    let id = handle_evolve_updates(
                        &ctx,
                        &new,
                        &old,
                        scope.as_deref(),
                        reason.as_deref(),
                    )?;
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
        Command::Graph(cmd) => {
            let ctx = Context::open(&cwd)?;
            match cmd {
                GraphCommand::Links { entity, relation } => {
                    let edges = handle_graph_links(&ctx, &entity, relation.as_deref())?;
                    format_graph_edges(&edges, cli.format);
                }
                GraphCommand::Backlinks { entity, relation } => {
                    let edges = handle_graph_backlinks(&ctx, &entity, relation.as_deref())?;
                    format_graph_edges(&edges, cli.format);
                }
                GraphCommand::Neighbors {
                    entity,
                    relation,
                    depth,
                } => {
                    let neighbors =
                        handle_graph_neighbors(&ctx, &entity, relation.as_deref(), depth)?;
                    format_graph_neighbors(&neighbors, cli.format);
                }
                GraphCommand::Path { a, b, max_hops } => {
                    let path = handle_graph_path(&ctx, &a, &b, max_hops)?;
                    format_graph_path(path.as_deref(), cli.format);
                }
                GraphCommand::Dangling => {
                    let dangling = handle_graph_dangling(&ctx)?;
                    format_graph_dangling(&dangling, cli.format);
                }
                GraphCommand::Hubs { relation, limit } => {
                    let hubs = handle_graph_hubs(&ctx, relation.as_deref(), limit)?;
                    format_graph_hubs(&hubs, cli.format);
                }
            }
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
                JournalCommand::ImportAll {
                    dir,
                    dry_run,
                    skip_existing,
                } => {
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
        Command::Cheatsheet => {
            let bin = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_else(|| "mdkb".to_string());
            print!(
                "\
# Search
{0} search <query>                                    # docs + memory (default)
{0} search <query> --scope memory                     # memory only
{0} search <query> --scope memory --entry-type TYPE    # filter by type
{0} search <query> --scope docs                        # documents only
{0} search <query> --scope symbols                     # symbol definitions (fuzzy)
{0} search <query> --scope symbols --file '*hook*'     # symbols in matching files
{0} search <query> --scope code                        # semantic code search
{0} get <id>                                           # full document/memory by ID
{0} get <id> --lines 10:50                             # line range

# Memory (--entry-type: topic, problem, decision, reminder, prior)
{0} memory add <id> --title T --content C              # create (default: topic)
{0} memory add <id> --title T --content C --source-type official_docs  # provenance/trust
{0} memory add <id> --title T --content C --entry-type prior --tags t1,t2
{0} memory add <id> --title T --content C --entry-type reminder --due-in 3600
{0} memory confirm <id> --outcome confirmed            # raise confidence (or refuted) — daemon-less
{0} memory link <id> <relation> <target>              # typed edge (supports|contradicts|supersedes|derived_from|relates_to)
{0} memory link <id> derived_from <path> --doc        # link to a document; --agent records provenance
{0} memory rm <id>                                     # delete
{0} memory list                                        # list active entries

# Code intelligence
{0} code callers <symbol>                              # who calls this?
{0} code calls <symbol>                                # what does this call?
{0} code impact <symbol>                               # transitive dependency graph
{0} code search <query>                                # fuzzy symbol search

# Knowledge graph (frontmatter + wikilink edges; refs accept collection-prefixed paths)
{0} graph links <entity>                               # outgoing edges (endpoints shown as paths, with 'via' relation)
{0} graph backlinks <entity>                           # incoming edges
{0} graph neighbors <entity> --depth 2                 # adjacent entities (undirected), each with 'via'
{0} graph path <a> <b>                                 # shortest path between two entities
{0} graph dangling                                     # refs resolving to no doc (full scan; explicit only)
{0} graph hubs --relation owner --limit 20             # entities by degree centrality (full scan; explicit only)

# Collections
{0} collection list                                    # name, path, pattern, doc count per collection
{0} collection add <name> <path> -p '**/*.md'          # register a collection

# Maintenance
{0} update                                             # reindex all (auto-embeds docs + backfills memory)
{0} embed --collection claude_sessions                # embed a specific collection (sessions excluded by default)
{0} stats                                              # index health, hooks, mining, sessions
{0} compact                                            # vacuum both databases
{0} compact --prune-sessions --older-than 90d --export dir  # hard-delete archived transcripts (exports first)
",
                bin
            );
        }
        Command::Schema { command } => {
            use clap::CommandFactory;
            let root = Cli::command();
            let target = match command {
                Some(ref name) => root
                    .find_subcommand(name)
                    .ok_or_else(|| mdkb::Error::other(format!("Unknown command: {name}")))?,
                None => &root,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&command_to_json(target))?
            );
        }
        Command::Code(cmd) => match cmd {
            CodeCommand::Init => {
                mdkb::cli::handlers::handle_code_init(&cwd)?;
                println!("Initialized code index at .mdkb/code.sqlite");
            }
            CodeCommand::Index { paths, force } => {
                let stats = if force {
                    mdkb::cli::handlers::handle_code_reindex(&cwd, &paths)?
                } else {
                    mdkb::cli::handlers::handle_code_index(&cwd, &paths)?
                };
                format_code_index_stats(&stats, cli.format);
            }
            CodeCommand::Search { query, limit, kind } => {
                let symbols =
                    mdkb::cli::handlers::handle_code_search(&cwd, &query, limit, kind.as_deref())?;
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
                let (source, callees) = mdkb::cli::handlers::handle_code_calls(&cwd, &name)?;
                format_code_graph("Calls", &source, &callees, cli.format);
            }
            CodeCommand::Callers { name } => {
                let (target, callers) = mdkb::cli::handlers::handle_code_callers(&cwd, &name)?;
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
                let symbols = mdkb::cli::handlers::handle_code_parse(std::path::Path::new(&file))?;
                format_code_parse(&symbols, &file, cli.format);
            }
        },
        Command::Setup(cmd) => match cmd {
            SetupCommand::Mcp(mcp_cmd) => match mcp_cmd {
                SetupMcpCommand::Claude { scope, yes } => {
                    let global = scope == "user";
                    if scope != "local" && scope != "user" && scope != "project" {
                        eprintln!(
                            "Error: Invalid scope '{}'. Must be 'local', 'user', or 'project'.",
                            scope
                        );
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
                SetupMcpCommand::Codex { dry_run } => {
                    let result = mdkb::cli::setup::handle_setup_mcp_codex(dry_run)?;
                    if !result.dry_run {
                        println!("{}", result.message);
                        println!("Wrote: {}", result.config_path.display());
                        println!();
                        println!("Restart Codex CLI to activate the mdkb MCP server.");
                    }
                }
            },
            SetupCommand::Hooks(hooks_cmd) => match hooks_cmd {
                SetupHooksCommand::Claude {
                    scope,
                    disable,
                    dry_run,
                    profile_dir,
                } => {
                    let result = mdkb::cli::setup::handle_setup_hooks_claude(
                        &cwd,
                        &scope,
                        &disable,
                        dry_run,
                        profile_dir.as_deref(),
                    )?;
                    if !result.dry_run {
                        if !result.events_registered.is_empty() {
                            println!("Registered hooks: {}", result.events_registered.join(", "));
                        }
                        if !result.events_skipped.is_empty() {
                            println!("Skipped: {}", result.events_skipped.join(", "));
                        }
                        println!("Wrote: {}", result.settings_path.display());
                        println!("Opt out per-directory with .mdkbignore-hooks");
                    }
                }
                SetupHooksCommand::Codex { disable, dry_run } => {
                    let result = mdkb::cli::setup::handle_setup_hooks_codex(&disable, dry_run)?;
                    if !result.dry_run {
                        if !result.events_registered.is_empty() {
                            println!("Registered hooks: {}", result.events_registered.join(", "));
                        }
                        if !result.events_skipped.is_empty() {
                            println!("Skipped: {}", result.events_skipped.join(", "));
                        }
                        println!("Wrote: {}", result.settings_path.display());
                        if !result.codex_hooks_flag_present {
                            eprintln!(
                                "Warning: `codex_hooks = true` not found in ~/.codex/config.toml \
                                 — Codex CLI will not invoke these hooks until the flag is set."
                            );
                        }
                        println!("Opt out per-directory with .mdkbignore-hooks");
                    }
                }
            },
            SetupCommand::Remove(remove_cmd) => match remove_cmd {
                SetupRemoveCommand::Mcp(mcp_cmd) => match mcp_cmd {
                    RemoveMcpCommand::Claude { scope } => {
                        let msg = mdkb::cli::setup::handle_remove_mcp_claude(&scope)?;
                        println!("{msg}");
                    }
                    RemoveMcpCommand::Codex => {
                        let msg = mdkb::cli::setup::handle_remove_mcp_codex()?;
                        println!("{msg}");
                    }
                },
                SetupRemoveCommand::Hooks(hooks_cmd) => match hooks_cmd {
                    RemoveHooksCommand::Claude { scope, profile_dir } => {
                        let msg = mdkb::cli::setup::handle_remove_hooks_claude(
                            &cwd,
                            &scope,
                            profile_dir.as_deref(),
                        )?;
                        println!("{msg}");
                    }
                    RemoveHooksCommand::Codex => {
                        let msg = mdkb::cli::setup::handle_remove_hooks_codex()?;
                        println!("{msg}");
                    }
                },
                SetupRemoveCommand::Claude { scope } => {
                    match mdkb::cli::setup::handle_remove_mcp_claude(&scope) {
                        Ok(msg) => println!("{msg}"),
                        Err(e) => eprintln!("Warning: MCP removal failed: {e}"),
                    }
                    match mdkb::cli::setup::handle_remove_hooks_claude(&cwd, &scope, None) {
                        Ok(msg) => println!("{msg}"),
                        Err(e) => eprintln!("Warning: hooks removal failed: {e}"),
                    }
                }
            },
        },
        Command::Session(cmd) => match cmd {
            SessionCommand::Index {
                sessions_path,
                project_root,
            } => {
                let ctx = Context::open(&cwd)?;
                let sessions_base = match sessions_path {
                    Some(p) => std::path::PathBuf::from(p),
                    None => mdkb::daemon::config::home_dir()?.join(".claude/projects"),
                };
                let root = project_root.unwrap_or_else(|| cwd.to_string_lossy().to_string());
                let result = handle_session_index(&ctx, &sessions_base, &root)?;
                format_update_result(&result, cli.format);
            }
        },
        Command::Daemon(cmd) => match cmd {
            DaemonCommand::Status => daemon_cli::handle_status()?,
            DaemonCommand::Stop => daemon_cli::handle_stop().await?,
            DaemonCommand::Restart => daemon_cli::handle_restart().await?,
        },
        Command::Hook(hook_cmd) => {
            #[cfg(unix)]
            match hook_cmd {
                HookCommand::SessionStart => {
                    let input = hook_logic::read_stdin_best_effort();
                    let event = hook_logic::parse_event(&input);
                    let root = hook_client::resolve_hook_root(&event, None);
                    if !hook_logic::mdkbignore_hooks_present(&root) {
                        hook_client::call_hook_event("hook.session_start", event, Some(root))
                            .await?;
                    }
                }
                HookCommand::UserPromptSubmit => {
                    let input = hook_logic::read_stdin_best_effort();
                    let event = hook_logic::parse_event(&input);
                    let root = hook_client::resolve_hook_root(&event, None);
                    if !hook_logic::mdkbignore_hooks_present(&root) {
                        hook_client::call_hook_event("hook.user_prompt_submit", event, Some(root))
                            .await?;
                    }
                }
                HookCommand::PostToolUse => {
                    let input = hook_logic::read_stdin_best_effort();
                    let event = hook_logic::parse_event(&input);
                    let root = hook_client::resolve_hook_root(&event, None);
                    if !hook_logic::mdkbignore_hooks_present(&root) {
                        hook_client::call_hook_event("hook.post_tool_use", event, Some(root))
                            .await?;
                    }
                }
                HookCommand::PreToolUse => {
                    let input = hook_logic::read_stdin_best_effort();
                    let event = hook_logic::parse_event(&input);
                    let root = hook_client::resolve_hook_root(&event, None);
                    if !hook_logic::mdkbignore_hooks_present(&root) {
                        hook_client::call_hook_event("hook.pre_tool_use", event, Some(root))
                            .await?;
                    }
                }
                HookCommand::Stop => {
                    let input = hook_logic::read_stdin_best_effort();
                    let event = hook_logic::parse_event(&input);
                    let root = hook_client::resolve_hook_root(&event, None);
                    if !hook_logic::mdkbignore_hooks_present(&root) {
                        hook_client::call_hook_event("hook.stop", event, Some(root)).await?;
                    }
                }
                HookCommand::Reindex { files, root } => {
                    hook_client::call_reindex(files, root).await?;
                }
                HookCommand::Search {
                    query,
                    scope,
                    limit,
                    root,
                } => {
                    hook_client::call_search(query, scope, limit, root).await?;
                }
                HookCommand::MemoryWrite {
                    id,
                    title,
                    entry_type,
                    content,
                    tags,
                    ttl,
                    root,
                } => {
                    hook_client::call_memory_write(id, title, entry_type, content, tags, ttl, root)
                        .await?;
                }
                HookCommand::MemoryConfirm { id, outcome, root } => {
                    hook_client::call_memory_confirm(id, outcome, root).await?;
                }
                HookCommand::Status { root } => {
                    hook_client::call_status(root).await?;
                }
            }
            #[cfg(not(unix))]
            {
                let _ = hook_cmd;
                return Err(mdkb::Error::other(
                    "Hook commands require Unix domain sockets",
                ));
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
                    println!(
                        "[{}] {}:{} - {} (score: {:.2})",
                        r.id, r.collection, r.path, title, r.score
                    );
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

fn format_update_result(result: &mdkb::domain::UpdateResult, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(result).unwrap());
        }
        OutputFormat::Csv => {
            println!("docs_indexed,added,updated,removed,unchanged,errors");
            println!(
                "{},{},{},{},{},{}",
                result.docs_indexed(),
                result.added,
                result.updated,
                result.removed,
                result.unchanged,
                result.errors.len()
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            // Honest doc-collection total up front, so a re-run on an unchanged
            // store reads as "7 indexed" not the misleading "0" from code stats.
            println!(
                "Docs: {} indexed ({} new, {} changed)",
                result.docs_indexed(),
                result.added,
                result.updated,
            );
            println!("Unchanged: {}", result.unchanged);
            if result.removed > 0 {
                println!("Removed: {}", result.removed);
            }
            if result.memory_embeddings_backfilled > 0 {
                println!(
                    "Memory embeddings backfilled: {}",
                    result.memory_embeddings_backfilled
                );
            }
            if result.doc_embeddings_generated > 0 {
                println!(
                    "Doc embeddings generated: {}",
                    result.doc_embeddings_generated
                );
            }
            if result.memory_entries_archived > 0 {
                println!(
                    "Memory entries archived (file deleted): {}",
                    result.memory_entries_archived
                );
            }
            if result.memory_entries_archive_skipped > 0 {
                println!(
                    "⚠ Memory archival SKIPPED — {} projected files missing at once \
                     (suspected bulk loss, e.g. git checkout/clean). Restore \
                     .mdkb/memory/entries/ and re-run `mdkb update`.",
                    result.memory_entries_archive_skipped
                );
            }
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
            println!(
                "**Tags:** {}",
                entry
                    .tags
                    .iter()
                    .map(|t| format!("#{t}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
            println!("**Access count:** {}", entry.access_count);
            if let Some(ref path) = entry.source_path {
                println!("**Source:** {path}");
            }
            println!();
            println!("---\n");
            println!("{}", entry.content);
        }
        OutputFormat::Text => {
            println!("[{}] {} ({})", entry.id, entry.title, entry.entry_type);
            if !entry.tags.is_empty() {
                println!(
                    "Tags: {}",
                    entry
                        .tags
                        .iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
            println!("Access count: {}", entry.access_count);
            if let Some(ref path) = entry.source_path {
                println!("Source: {path}");
            }
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
                let tags = e
                    .tags
                    .iter()
                    .map(|t| format!("#{t}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!(
                    "| {} | {} | {} | {} | {} |",
                    e.id, e.title, e.entry_type, tags, e.access_count
                );
            }
        }
        OutputFormat::Text => {
            if entries.is_empty() {
                println!("No memory entries found.");
            } else {
                for e in entries {
                    let tags = e
                        .tags
                        .iter()
                        .map(|t| format!("#{t}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!(
                        "[{}] {} ({}) {} - {} accesses",
                        e.id, e.title, e.entry_type, tags, e.access_count
                    );
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
                println!(
                    "No entries to prune (all entries accessed within {} days).",
                    days
                );
            } else if dry_run {
                println!(
                    "Would archive {} entries not accessed in {} days:",
                    pruned.len(),
                    days
                );
                for id in pruned {
                    println!("  - {}", id);
                }
            } else {
                println!(
                    "Archived {} entries not accessed in {} days:",
                    pruned.len(),
                    days
                );
                for id in pruned {
                    println!("  - {}", id);
                }
            }
        }
    }
}

#[cfg(feature = "llm")]
fn format_condense_result(
    result: &mdkb::cli::handlers::CondenseResult,
    dry_run: bool,
    format: OutputFormat,
) {
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
                println!(
                    "{},{},{}",
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
                println!(
                    "Group {}: {} entries -> {}",
                    i + 1,
                    g.entry_ids.len(),
                    g.proposed_id
                );
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
                println!(
                    "Consolidated {} entries into {} merged entries.",
                    result.consolidated_count, result.merged_count
                );
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

fn format_graph_edges(edges: &[mdkb::store::graph::EdgeView], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(edges).unwrap());
        }
        OutputFormat::Csv => {
            println!("source,target_ref,relation,source_kind,scope");
            for e in edges {
                println!(
                    "{},{},{},{},{}",
                    e.source,
                    e.target_ref,
                    e.relation,
                    e.source_kind,
                    e.scope.as_deref().unwrap_or(""),
                );
            }
        }
        OutputFormat::Markdown => {
            if edges.is_empty() {
                println!("No edges found.");
            } else {
                println!("| Source | Target | Relation | Kind |");
                println!("|--------|--------|----------|------|");
                for e in edges {
                    println!(
                        "| {} | {} | {} | {} |",
                        e.source, e.target_ref, e.relation, e.source_kind
                    );
                }
            }
        }
        OutputFormat::Text => {
            if edges.is_empty() {
                println!("No edges found.");
            } else {
                for e in edges {
                    println!(
                        "  {} --{}--> {} ({})",
                        e.source, e.relation, e.target_ref, e.source_kind
                    );
                }
            }
        }
    }
}

fn format_graph_neighbors(neighbors: &[mdkb::store::graph::Neighbor], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(neighbors).unwrap());
        }
        OutputFormat::Csv => {
            println!("entity,depth,via");
            for n in neighbors {
                println!("{},{},{}", n.entity, n.depth, n.via.join("|"));
            }
        }
        OutputFormat::Markdown => {
            if neighbors.is_empty() {
                println!("No neighbors found.");
            } else {
                println!("| Entity | Depth | Via |");
                println!("|--------|-------|-----|");
                for n in neighbors {
                    println!("| {} | {} | {} |", n.entity, n.depth, n.via.join(", "));
                }
            }
        }
        OutputFormat::Text => {
            if neighbors.is_empty() {
                println!("No neighbors found.");
            } else {
                for n in neighbors {
                    println!(
                        "  {} (depth {}, via {})",
                        n.entity,
                        n.depth,
                        n.via.join(", ")
                    );
                }
            }
        }
    }
}

fn format_graph_path(path: Option<&[String]>, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&path).unwrap());
        }
        _ => match path {
            Some(nodes) => println!("{}", nodes.join(" -> ")),
            None => println!("No path found."),
        },
    }
}

fn format_collection_list(
    collections: &[mdkb::cli::handlers::CollectionInfo],
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(collections).unwrap());
        }
        OutputFormat::Csv => {
            println!("name,path,pattern,doc_count");
            for c in collections {
                println!("{},{},{},{}", c.name, c.path, c.pattern, c.doc_count);
            }
        }
        OutputFormat::Markdown => {
            if collections.is_empty() {
                println!("No collections.");
            } else {
                println!("| Name | Path | Pattern | Docs |");
                println!("|------|------|---------|------|");
                for c in collections {
                    println!(
                        "| {} | {} | {} | {} |",
                        c.name, c.path, c.pattern, c.doc_count
                    );
                }
            }
        }
        OutputFormat::Text => {
            if collections.is_empty() {
                println!("No collections.");
            } else {
                for c in collections {
                    println!(
                        "  {} ({}, {}) — {} docs",
                        c.name, c.path, c.pattern, c.doc_count
                    );
                }
            }
        }
    }
}

fn format_graph_dangling(dangling: &[mdkb::store::graph::DanglingRef], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(dangling).unwrap());
        }
        OutputFormat::Csv => {
            println!("target_ref,relation,source");
            for d in dangling {
                println!("{},{},{}", d.target_ref, d.relation, d.source);
            }
        }
        OutputFormat::Markdown => {
            if dangling.is_empty() {
                println!("No dangling references.");
            } else {
                println!("| Target | Relation | Source |");
                println!("|--------|----------|--------|");
                for d in dangling {
                    println!("| {} | {} | {} |", d.target_ref, d.relation, d.source);
                }
            }
        }
        OutputFormat::Text => {
            if dangling.is_empty() {
                println!("No dangling references.");
            } else {
                for d in dangling {
                    println!(
                        "  {} --{}--> {} (unresolved)",
                        d.source, d.relation, d.target_ref
                    );
                }
            }
        }
    }
}

fn format_graph_hubs(hubs: &[mdkb::store::graph::Hub], format: OutputFormat) {
    let breakdown = |h: &mdkb::store::graph::Hub| {
        h.by_relation
            .iter()
            .map(|(r, c)| format!("{r}:{c}"))
            .collect::<Vec<_>>()
            .join(" ")
    };
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(hubs).unwrap());
        }
        OutputFormat::Csv => {
            println!("entity,in_degree,out_degree,by_relation");
            for h in hubs {
                println!(
                    "{},{},{},{}",
                    h.entity,
                    h.in_degree,
                    h.out_degree,
                    breakdown(h).replace(',', ";")
                );
            }
        }
        OutputFormat::Markdown => {
            if hubs.is_empty() {
                println!("No edges.");
            } else {
                println!("| Entity | In | Out | By relation |");
                println!("|--------|----|-----|-------------|");
                for h in hubs {
                    println!(
                        "| {} | {} | {} | {} |",
                        h.entity,
                        h.in_degree,
                        h.out_degree,
                        breakdown(h)
                    );
                }
            }
        }
        OutputFormat::Text => {
            if hubs.is_empty() {
                println!("No edges.");
            } else {
                for h in hubs {
                    println!(
                        "  {} (in {}, out {}) [{}]",
                        h.entity,
                        h.in_degree,
                        h.out_degree,
                        breakdown(h)
                    );
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
            println!(
                "Current version: [{}] {}:{} - {}",
                doc.id, doc.collection, doc.relative_path, title
            );
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

fn format_metrics_summary(
    metrics: &mdkb::store::stats::QueryMetricsSummary,
    period: u32,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(metrics).unwrap_or_default()
            );
        }
        OutputFormat::Csv => {
            println!(
                "total_queries,zero_result_rate,re_search_rate,latency_p50,latency_p95,latency_p99"
            );
            println!(
                "{},{:.1},{:.1},{},{},{}",
                metrics.total_queries,
                metrics.zero_result_rate,
                metrics.re_search_rate,
                metrics.latency_p50,
                metrics.latency_p95,
                metrics.latency_p99
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("=== Query Metrics (last {} days) ===\n", period);
            println!("Total queries: {}", metrics.total_queries);
            println!(
                "Zero-result rate: {:.1}%{}",
                metrics.zero_result_rate,
                if metrics.zero_result_rate > 10.0 {
                    " ⚠️ High - queries not finding results"
                } else {
                    ""
                }
            );
            println!(
                "Re-search rate: {:.1}%{}",
                metrics.re_search_rate,
                if metrics.re_search_rate > 15.0 {
                    " ⚠️ High - initial results may be poor"
                } else {
                    ""
                }
            );
            println!();
            println!("Latency:");
            println!("  p50: {}ms", metrics.latency_p50);
            println!("  p95: {}ms", metrics.latency_p95);
            println!(
                "  p99: {}ms{}",
                metrics.latency_p99,
                if metrics.latency_p99 > 500 {
                    " ⚠️ Slow - performance issue"
                } else {
                    ""
                }
            );
            println!();
            println!("Score distribution:");
            println!("  > 0.8: {:.1}%", metrics.score_above_80);
            println!("  0.5-0.8: {:.1}%", metrics.score_50_to_80);
            println!("  < 0.5: {:.1}%", metrics.score_below_50);
        }
    }
}

fn format_latency_stats(
    stats: &[mdkb::store::stats::QueryLatencyStats],
    period: u32,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(stats).unwrap_or_default()
            );
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
                    println!(
                        "  Zero results: {} ({:.1}%)",
                        s.zero_result_count,
                        if s.count > 0 {
                            (s.zero_result_count as f64 / s.count as f64) * 100.0
                        } else {
                            0.0
                        }
                    );
                    println!();
                }
            }
        }
    }
}

fn format_quality_metrics(
    metrics: &mdkb::store::stats::QueryMetricsSummary,
    period: u32,
    format: OutputFormat,
) {
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
            println!(
                "{}",
                serde_json::to_string_pretty(&quality).unwrap_or_default()
            );
        }
        OutputFormat::Csv => {
            println!(
                "period_days,total_queries,zero_result_rate,re_search_rate,score_above_80,score_50_to_80,score_below_50"
            );
            println!(
                "{},{},{:.1},{:.1},{:.1},{:.1},{:.1}",
                period,
                metrics.total_queries,
                metrics.zero_result_rate,
                metrics.re_search_rate,
                metrics.score_above_80,
                metrics.score_50_to_80,
                metrics.score_below_50
            );
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
            println!(
                "{}",
                serde_json::to_string_pretty(events).unwrap_or_default()
            );
        }
        OutputFormat::Csv => {
            println!("query_hash,query_text,search_type,result_count,latency_ms,top_score");
            for e in events {
                println!(
                    "{},{},{},{},{},{}",
                    e.query_hash,
                    e.query_text.replace(',', ";"),
                    e.search_type,
                    e.result_count,
                    e.latency_ms,
                    e.top_score.map(|s| format!("{:.3}", s)).unwrap_or_default()
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Query events ({} total):\n", events.len());
            for (i, e) in events.iter().take(20).enumerate() {
                println!("{}. \"{}\"", i + 1, e.query_text);
                println!(
                    "   Type: {} | Results: {} | Latency: {}ms | Score: {}",
                    e.search_type,
                    e.result_count,
                    e.latency_ms,
                    e.top_score
                        .map(|s| format!("{:.2}", s))
                        .unwrap_or_else(|| "N/A".to_string())
                );
            }
            if events.len() > 20 {
                println!(
                    "\n... and {} more events (use --json or --csv for full export)",
                    events.len() - 20
                );
            }
        }
    }
}

fn format_experiment_status(
    status: &mdkb::store::stats::ExperimentStatusReport,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(status).unwrap_or_default()
            );
        }
        OutputFormat::Csv => {
            println!(
                "experiment,variant,sample_count,avg_score,avg_latency_ms,p95_latency_ms,zero_result_rate"
            );
            println!(
                "{},A,{},{:.3},{:.1},{},{}",
                status.experiment.name,
                status.variant_a.sample_count,
                status.variant_a.avg_score,
                status.variant_a.avg_latency_ms,
                status.variant_a.p95_latency_ms,
                status.variant_a.zero_result_rate
            );
            println!(
                "{},B,{},{:.3},{:.1},{},{}",
                status.experiment.name,
                status.variant_b.sample_count,
                status.variant_b.avg_score,
                status.variant_b.avg_latency_ms,
                status.variant_b.p95_latency_ms,
                status.variant_b.zero_result_rate
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            let exp = &status.experiment;
            let started = chrono::DateTime::from_timestamp(exp.created_at, 0)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".to_string());

            println!(
                "Experiment: {} ({} since {})",
                exp.name, exp.status, started
            );
            if let Some(desc) = &exp.description {
                println!("Description: {desc}");
            }
            println!();

            // Variant A
            let a = &status.variant_a;
            println!(
                "Variant A: avg score {:.3}, p95 latency {}ms, n={}",
                a.avg_score, a.p95_latency_ms, a.sample_count
            );
            println!("  Config: {}", exp.config_a);

            // Variant B
            let b = &status.variant_b;
            println!(
                "Variant B: avg score {:.3}, p95 latency {}ms, n={}",
                b.avg_score, b.p95_latency_ms, b.sample_count
            );
            println!("  Config: {}", exp.config_b);
            println!();

            // Significance
            if !status.has_min_samples {
                let needed = exp.min_sample_size - a.sample_count.min(b.sample_count);
                println!(
                    "Significance: Need {} more samples before statistical analysis",
                    needed.max(0)
                );
            } else if let Some(sig) = &status.significance {
                if sig.significant {
                    println!(
                        "Significance: {:.0}% confidence {} has better quality (p={:.4}, effect size={:.2})",
                        sig.confidence_level,
                        sig.winner.as_deref().unwrap_or("?"),
                        sig.p_value,
                        sig.effect_size
                    );
                } else {
                    println!(
                        "Significance: No significant difference detected (p={:.4})",
                        sig.p_value
                    );
                }
            }
        }
    }
}

fn format_experiment_list(experiments: &[mdkb::store::stats::Experiment], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(experiments).unwrap_or_default()
            );
        }
        OutputFormat::Csv => {
            println!("name,status,traffic_split,min_samples,created_at,winner");
            for exp in experiments {
                println!(
                    "{},{},{},{},{},{}",
                    exp.name,
                    exp.status,
                    exp.traffic_split,
                    exp.min_sample_size,
                    exp.created_at,
                    exp.winner.as_deref().unwrap_or("")
                );
            }
        }
        OutputFormat::Markdown => {
            println!("| Name | Status | Split | Min Samples | Winner |");
            println!("|------|--------|-------|-------------|--------|");
            for exp in experiments {
                println!(
                    "| {} | {} | {:.0}% | {} | {} |",
                    exp.name,
                    exp.status,
                    exp.traffic_split * 100.0,
                    exp.min_sample_size,
                    exp.winner.as_deref().unwrap_or("-")
                );
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
                    let winner_str = exp
                        .winner
                        .as_ref()
                        .map(|w| format!(" -> Winner: {w}"))
                        .unwrap_or_default();
                    println!(
                        "{}: {} (started {}){}",
                        exp.name, exp.status, started, winner_str
                    );
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
            println!(
                "{},{},{}",
                result.source_path,
                result.created.len(),
                result.skipped.len()
            );
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

fn format_journal_import_all_results(
    results: &[JournalImportResult],
    dry_run: bool,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(results).unwrap());
        }
        OutputFormat::Csv => {
            println!("source,created,skipped");
            for result in results {
                println!(
                    "{},{},{}",
                    result.source_path,
                    result.created.len(),
                    result.skipped.len()
                );
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

            let errors: Vec<_> = results
                .iter()
                .flat_map(|r| {
                    r.skipped
                        .iter()
                        .filter(|(reason, _)| reason.starts_with("Error"))
                })
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
                "files_removed": stats.files_removed,
                "symbols_indexed": stats.symbols_indexed,
                "relationships_collected": stats.relationships_collected,
                "parse_errors": stats.parse_errors,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!(
                "files_discovered,files_indexed,files_removed,symbols_indexed,relationships,parse_errors"
            );
            println!(
                "{},{},{},{},{},{}",
                stats.files_discovered,
                stats.files_indexed,
                stats.files_removed,
                stats.symbols_indexed,
                stats.relationships_collected,
                stats.parse_errors,
            );
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!("Files discovered: {}", stats.files_discovered);
            println!("Files indexed:    {}", stats.files_indexed);
            // Deletions are the one number the file counts can't imply — stay
            // quiet when nothing was dropped.
            if stats.files_removed > 0 {
                println!("Files removed:    {}", stats.files_removed);
            }
            println!("Symbols indexed:  {}", stats.symbols_indexed);
            println!("Relationships:    {}", stats.relationships_collected);
            // Only surface parse failures when there are any — a clean run stays quiet.
            if stats.parse_errors > 0 {
                println!("Parse errors:     {} (see logs)", stats.parse_errors);
            }
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
                    source.name, label, t.name, t.kind, t.file_path, t.range.start_line,
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!(
                "{} for {} {} ({}:{})",
                label, source.kind, source.name, source.file_path, source.range.start_line,
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
                    s.name, s.kind, s.range.start_line, s.range.end_line, s.visibility,
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

/// Run the mdkb daemon as a singleton.
///
/// 1. Acquires `flock(LOCK_EX|LOCK_NB)` on `~/.mdkb/daemon.pid`. On contention,
///    exits 0 with a message on stderr — the user's goal (daemon running) is met.
/// 2. Writes pid into the lock file.
/// 3. Starts IPC listeners (MCP + hook unix sockets) under `~/.mdkb/`.
/// 4. Waits for SIGINT/SIGTERM, then signals the IPC server to unlink sockets
///    and drops the lock guard. Watcher + registry wire-up arrives in Story 7.
#[cfg(unix)]
async fn run_daemon() -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicU64};

    use mdkb::daemon::config::DaemonConfig;
    use mdkb::daemon::ipc_server;
    use mdkb::daemon::registry::RepoRegistry;
    use mdkb::daemon::singleton::{
        AcquireError, acquire_singleton_lock, default_lock_path, read_pid,
    };
    use mdkb::mcp::dispatch::DispatchContext;
    use mdkb::metrics::UsageMetrics;
    use tokio_util::sync::CancellationToken;

    let lock_path = default_lock_path();

    let mut guard = match acquire_singleton_lock(&lock_path) {
        Ok(g) => g,
        Err(AcquireError::AlreadyHeld) => {
            let pid = read_pid(&lock_path)
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".to_string());
            eprintln!(
                "mdkb daemon already running at pid {pid} ({})",
                lock_path.display()
            );
            return Ok(());
        }
        Err(e) => return Err(mdkb::Error::other(format!("daemon lock: {e}"))),
    };

    let pid = std::process::id();
    guard
        .write_pid(pid)
        .map_err(|e| mdkb::Error::other(format!("daemon pid write: {e}")))?;

    let base_dir = lock_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    tracing::info!(
        "mdkb daemon started (pid {pid}, base {})",
        base_dir.display()
    );

    let daemon_config_path = base_dir.join("daemon.toml");
    let daemon_config = DaemonConfig::load_or_default(&daemon_config_path)
        .map_err(|e| mdkb::Error::other(format!("daemon config: {e}")))?;
    let registry = Arc::new(RepoRegistry::new(daemon_config));
    let dctx = Arc::new(DispatchContext {
        metrics: Arc::new(UsageMetrics::new()),
        session_id: Arc::new(AtomicI64::new(0)),
        persistent_call_count: Arc::new(AtomicU64::new(0)),
        optimize_interval_calls: 200,
        hook_dedup: Arc::new(std::sync::Mutex::new(Default::default())),
    });

    let shutdown = CancellationToken::new();
    let ipc_shutdown = shutdown.clone();
    let ipc_base = base_dir.clone();
    let ipc_registry = Arc::clone(&registry);
    let ipc_dctx = Arc::clone(&dctx);
    let ipc_task = tokio::spawn(async move {
        ipc_server::serve(&ipc_base, ipc_shutdown, ipc_registry, ipc_dctx).await
    });

    // Wait for SIGINT or SIGTERM, whichever arrives first.
    wait_for_shutdown_signal().await?;
    tracing::info!("mdkb daemon received shutdown signal");

    shutdown.cancel();
    match ipc_task.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("ipc server shutdown error: {e}"),
        Err(e) => tracing::warn!("ipc server join error: {e}"),
    }

    drop(guard);
    Ok(())
}

/// True when stdin is not attached to a terminal. Used by the `mdkb serve`
/// back-compat shim to detect the "legacy mcp.json" launch pattern without
/// surprising an interactive operator running `mdkb serve` in their shell.
fn stdin_is_not_tty() -> bool {
    use std::io::IsTerminal;
    !std::io::stdin().is_terminal()
}

/// Serialize a clap `Command` into a machine-readable JSON description
/// (name, about, args, nested subcommands) for agent introspection.
fn command_to_json(cmd: &clap::Command) -> serde_json::Value {
    let args: Vec<serde_json::Value> = cmd
        .get_arguments()
        .filter(|a| a.get_id() != "help" && a.get_id() != "version")
        .map(|a| {
            serde_json::json!({
                "name": a.get_id().as_str(),
                "help": a.get_help().map(|h| h.to_string()),
                "required": a.is_required_set(),
                "positional": a.is_positional(),
                "long": a.get_long(),
                "short": a.get_short().map(|c| c.to_string()),
                "takes_value": a.get_action().takes_values(),
            })
        })
        .collect();

    let subcommands: Vec<serde_json::Value> = cmd.get_subcommands().map(command_to_json).collect();

    serde_json::json!({
        "name": cmd.get_name(),
        "about": cmd.get_about().map(|a| a.to_string()),
        "args": args,
        "subcommands": subcommands,
    })
}

/// Wait for the first of SIGINT or SIGTERM.
#[cfg(unix)]
async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())
            .map_err(|e| mdkb::Error::other(format!("sigterm handler: {e}")))?;
        let mut int = signal(SignalKind::interrupt())
            .map_err(|e| mdkb::Error::other(format!("sigint handler: {e}")))?;
        tokio::select! {
            _ = term.recv() => {},
            _ = int.recv() => {},
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c()
            .await
            .map_err(|e| mdkb::Error::other(format!("signal handler: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::is_server_invocation;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn server_subcommands_use_multi_thread_runtime() {
        assert!(is_server_invocation(args(&["serve"])));
        assert!(is_server_invocation(args(&[
            "serve", "--http", "--token", "x"
        ])));
        assert!(is_server_invocation(args(&["mcp"])));
        assert!(is_server_invocation(args(&["serve", "--daemon"])));
    }

    #[test]
    fn one_shot_subcommands_use_current_thread_runtime() {
        for one_shot in [
            args(&["hook", "user-prompt-submit"]),
            args(&["get", "42"]),
            args(&["search", "query"]),
            args(&["update"]),
            args(&["daemon", "restart"]),
        ] {
            assert!(
                !is_server_invocation(one_shot.clone()),
                "one-shot invocation must not select the multi-thread runtime: {one_shot:?}"
            );
        }
    }
}
