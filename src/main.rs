//! mdkb - Local markdown knowledge base CLI.

use mimalloc::MiMalloc;
use std::env;
use std::path::Path;

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
    EmbedResult, EvalOptions, EvolutionHistoryEntry, handle_collection_add, handle_collection_list,
    handle_collection_remove, handle_collection_rename, handle_collection_update, handle_current,
    handle_embed, handle_eval_judge, handle_eval_recall, handle_evolve_corrects,
    handle_evolve_extends, handle_evolve_retracts, handle_evolve_supersedes, handle_evolve_updates,
    handle_experiment_cancel, handle_experiment_create, handle_experiment_end,
    handle_experiment_list, handle_experiment_status, handle_get, handle_graph_backlinks,
    handle_graph_dangling, handle_graph_hubs, handle_graph_links, handle_graph_neighbors,
    handle_graph_path, handle_graph_relations, handle_history, handle_init, handle_memory_add,
    handle_memory_confirm, handle_memory_export, handle_memory_import, handle_memory_import_dir,
    handle_memory_import_file, handle_memory_link, handle_memory_list, handle_memory_prune,
    handle_memory_rm, handle_memory_search, handle_memory_show, handle_memory_warmup,
    handle_metrics_export, handle_metrics_latency, handle_metrics_purge, handle_metrics_show,
    handle_metrics_status, handle_prune_sessions, handle_superseded_by, parse_retention_secs,
};
use mdkb::cli::hook_client;
use mdkb::cli::hook_logic;
use mdkb::cli::journal::JournalImportResult;
use mdkb::cli::{
    Cli, CollectionCommand, Command, DaemonCommand, EvalCommand, EvolveCommand, ExperimentCommand,
    GraphCommand, HookCommand, JournalCommand, MemoryCommand, MetricsCommand, OutputFormat,
    RemoveHooksCommand, RemoveMcpCommand, SessionCommand, SetupCommand, SetupHooksCommand,
    SetupMcpCommand, SetupRemoveCommand, parse_eval_modes,
};
use mdkb::core::Context;
use mdkb::core::indexing::{UpdateOutcome, UpdateRequest, report_code_stats, update_documents};
use mdkb::core::search::{handle_hybrid_search, handle_mget};
use mdkb::core::sessions::handle_session_index;
use mdkb::mcp::server::run_server;
use mdkb::store::evolution::Evolution;
use mdkb::store::memory::MemoryEntry;
use rmcp::ServiceExt;

/// One line per mode, then the queries each mode missed, so a drop in the
/// number comes with the cases behind it.
fn print_eval_recall(runs: &[mdkb::eval::ModeRun<mdkb::eval::recall::RecallReport>]) {
    for run in runs {
        match &run.report {
            // `n/a` rather than a number when the fixture labelled no
            // negatives: `hits / hits` is not a measurement.
            Some(r) => println!(
                "{:<10} recall@{}: {:.3}  MRR: {:.3}  precision: {}  \
                 (n={}, misses={}, negatives={}, false positives={})",
                run.mode.as_str(),
                r.k,
                r.recall_at_k,
                r.mrr,
                r.precision
                    .map_or_else(|| "n/a".to_string(), |p| format!("{p:.3}")),
                r.n,
                r.misses.len(),
                r.n_negatives,
                r.false_positives.len()
            ),
            None => println!(
                "{:<10} skipped: {}",
                run.mode.as_str(),
                run.skipped.as_deref().unwrap_or("no reason given")
            ),
        }
    }
    for run in runs {
        if let Some(r) = &run.report {
            if !r.misses.is_empty() {
                println!("\nmissed ({}):", run.mode.as_str());
                for q in &r.misses {
                    println!("  {q}");
                }
            }
            // The other half of the picture: a query with no answer that got
            // one anyway. A precision drop has to name its queries too.
            if !r.false_positives.is_empty() {
                println!("\nadmitted a negative ({}):", run.mode.as_str());
                for q in &r.false_positives {
                    println!("  {q}");
                }
            }
        }
    }
}

/// Open a store for reading, and say so out loud if that migrated it.
///
/// `Context::open_read_only_migrating` deliberately migrates a stale store
/// rather than refusing: a read command that works is worth more than one
/// that sends you to run `mdkb update` first. What it must not do is stay
/// quiet about it. The migration is not a schema bump alone — v21 deletes
/// memory entries with unreadable ids, v23 and v26 archive prior clusters,
/// v27 moves prior candidates — and 82 of the 102 stores measured on this
/// machine are older than v28, so a fleet survey run with a reporting command
/// rewrites every one of them.
///
/// One place owns this message, rather than seventeen call sites each
/// deciding whether to mention it.
fn open_reader(cwd: &std::path::Path) -> mdkb::error::Result<mdkb::core::Context> {
    let ctx = mdkb::core::Context::open_read_only_migrating(cwd)?;
    announce_migration(&ctx, "This was a read command");
    Ok(ctx)
}

/// Open a store for writing, and say so out loud if that migrated it.
///
/// The write path migrates identically and is the one an operator reaches for
/// deliberately on an old store — `mdkb update` on a v20 repo runs every
/// data-mutating step there is. Shipping the notice on reads alone left the
/// higher-impact path silent, which is the half of this the first pass missed.
fn open_writer(cwd: &std::path::Path) -> mdkb::error::Result<mdkb::core::Context> {
    let ctx = mdkb::core::Context::open_writer_admitted(cwd)?;
    announce_migration(&ctx, "The command you ran migrated it");
    Ok(ctx)
}

/// One message, one owner. Both openers route here so a reader and a writer
/// cannot drift into describing the same event differently.
fn announce_migration(ctx: &mdkb::core::Context, lead: &str) {
    if let Some(from) = ctx.migrated_from {
        eprintln!(
            "mdkb: migrated this store from schema v{from} to v{}. {lead}; the \
             migration also rewrites memory entries and prior clusters. Back up \
             .mdkb/index.sqlite before the next one if that matters.",
            mdkb::store::schema::SCHEMA_VERSION
        );
    }
}

/// A readable store with no registered document collections is not a healthy
/// empty result. Keep the notice on stderr so JSON and CSV stdout stay valid.
fn announce_no_collections(ctx: &mdkb::core::Context) -> mdkb::error::Result<()> {
    if mdkb::store::collections::list_collections(&ctx.conn)?.is_empty() {
        eprintln!(
            "mdkb: this store has no registered collections; run `mdkb update` to detect and index them."
        );
    }
    Ok(())
}

fn main() {
    // A `Result` returned from `main` is printed with `{:?}`, which for our
    // error type is the struct dump `Error { kind: .., backtrace: .. }` and
    // hides every `#[error]` Display string. Print the Display form once,
    // where the error finally surfaces, and exit 1 as the default would.
    if let Err(e) = run_process() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}

/// Everything `main` does, as the `Result` it reports.
fn run_process() -> Result<()> {
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
    run_on_sized_stack(move || {
        let rt = builder
            .enable_all()
            .build()
            .map_err(|e| mdkb::Error::other(format!("build tokio runtime: {e}")))?;
        let result = rt.block_on(run());
        // Dropping the runtime joins every worker, and a worker stays busy
        // until its current task yields. `serve` spawns the startup code
        // reindex as synchronous work on a worker, so a SIGTERM that arrived
        // during that reindex kept the process alive until it finished
        // (measured 2026-09-12: 37.8s on a debug build for 38 changed
        // files). Once `run` has returned nothing awaits that work: bound
        // the wait. SQLite rolls the interrupted write back on the next open.
        rt.shutdown_timeout(std::time::Duration::from_secs(1));
        result
    })
}

/// Stack for the thread every invocation actually runs on.
///
/// The main thread's stack is sized by the platform, not by this program:
/// 8 MiB on Linux and macOS, 1 MiB on Windows (the MSVC linker default).
/// Measured on macOS aarch64 debug (2026-09-08): building the clap command
/// tree needs between 768 KiB and 896 KiB on its own — and every invocation
/// builds it, in `Cli::parse_args`, with `mdkb schema` building it a second
/// time on top of the `run_cli` frame. That fits under 8 MiB with room to
/// spare and sits on the 1 MiB Windows budget, which is why debug builds of
/// `mdkb schema` aborted there with 0xC00000FD while release builds — whose
/// frames are far smaller — survived (issue #6).
///
/// The recursive `command_to_json` walk was the suspect; it is not. The same
/// measurement shows it adds nothing detectable on top of the clap build, so
/// rewriting it iteratively would have fixed nothing.
///
/// 8 MiB is what Unix already handed us, so sizing the stack here changes no
/// platform's behavior — it only stops the platform from deciding.
const WORK_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Run `work` on a thread with [`WORK_STACK_BYTES`] of stack, returning its
/// result.
///
/// A panic inside `work` is re-raised here rather than converted, so the
/// process still aborts with the usual panic output and exit code instead of
/// reporting a thread-plumbing error in its place.
fn run_on_sized_stack<T, F>(work: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name("mdkb-main".to_string())
        .stack_size(WORK_STACK_BYTES)
        .spawn(work)
        .map_err(|e| mdkb::Error::other(format!("spawn worker thread: {e}")))?
        .join()
        .unwrap_or_else(|payload| std::panic::resume_unwind(payload))
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

/// The five events the harness fires on its own, as opposed to the `hook`
/// subcommands an agent invokes deliberately.
///
/// The distinction decides what happens where no store can be anchored: an
/// event nobody asked for is dropped, while `hook search` or `hook
/// memory-write` still fails loudly, because silence there would hide a miss
/// from the caller that meant to write or read something.
fn is_lifecycle_hook(command: &Command) -> bool {
    matches!(
        command,
        Command::Hook(
            HookCommand::SessionStart
                | HookCommand::UserPromptSubmit
                | HookCommand::PostToolUse
                | HookCommand::PreToolUse
                | HookCommand::Stop
        )
    )
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

async fn run_cli(mut cli: Cli) -> Result<()> {
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
    } else if matches!(cli.command, Command::Mcp { .. }) {
        // `mdkb mcp` anchors nothing: both run modes serve globally and take no
        // cwd at all — `run_global_stdio_server` and `mcp_proxy::run_proxy`
        // resolve a root per tool call. Holding it to the anchoring guard cost
        // the whole connection and bought nothing: launched in a directory that
        // holds repositories, it returned the refusal as an `Err` and died
        // before writing a single JSON-RPC byte, so the client could only
        // report `-32000 Connection closed`. Measured 2026-09-21 on
        // CC_Playground/brainstorming, the day an immediate child repo appeared
        // under it. The guard is unchanged for every command that does anchor.
        raw_cwd.clone()
    } else {
        let hint = std::env::var_os("CLAUDE_PROJECT_DIR").map(std::path::PathBuf::from);
        match mdkb::git::resolve_project_root(&raw_cwd, hint.as_deref()) {
            Some(root) => root,
            // A lifecycle hook fires wherever the agent happens to be, which
            // includes directories no store may be anchored at. It has no
            // human reading its stderr and nothing to record here, so it ends
            // quietly instead of repeating the refusal on every event.
            None if is_lifecycle_hook(&cli.command) => return Ok(()),
            None => {
                return Err(mdkb::Error::other(format!(
                    "refusing to anchor a store at {}: it holds git repositories (or is your \
                     home directory), so a store here would index every repository underneath \
                     it. Run mdkb from inside a project, or `mdkb init` here if you really \
                     mean it.",
                    raw_cwd.display()
                )));
            }
        }
    };
    let cwd = cwd.canonicalize().unwrap_or(cwd);

    // On Unix, the daemon is the sole writer. Every mutation is sent over one typed
    // internal RPC instead of opening a second
    // write-capable connection to a file the daemon is already writing — the
    // same client pattern `mdkb hook` uses (story 018-56b2).
    //
    // `MDKB_NO_DAEMON=1` is the single escape hatch, checked inside
    // `should_route`. An unreachable daemon falls back to running in-process
    // rather than failing: a routing layer that turns a daemon outage into a
    // broken CLI is worse than no routing (the lesson from 021-0636).
    #[cfg(unix)]
    if mdkb::core::routing::should_route(&cli.command) {
        use mdkb::cli::hook_client::MutationFailure;
        let request = mdkb::core::routing::mutation_request(&mut cli.command, &raw_cwd, &cwd)?
            .ok_or_else(|| mdkb::Error::other("mutating command has no cli.mutate mapping"))?;
        match mdkb::cli::hook_client::call_cli_mutation(&request, &cwd).await {
            Ok(result) => {
                print_routed_result(&cli.command, &result, cli.format)?;
                return Ok(());
            }
            // The daemon may be writing right now. Running the same mutation
            // here would be the second writer this routing exists to remove —
            // on the longest write in the program, which is the one most likely
            // to outlast a client's patience.
            Err(MutationFailure::Undetermined(e)) => {
                // Cause first: it is the only part that changes between
                // failures, and the only part the operator can act on. The
                // explanation of why this is not retried here follows it.
                return Err(mdkb::error::Error::other(format!(
                    "{e}. The daemon took this request and did not report back, so it may \
                     still be writing — not running it here. Check `mdkb daemon status`; if \
                     no daemon is running, re-run with MDKB_NO_DAEMON=1."
                )));
            }
            Err(MutationFailure::Unstarted(e)) => {
                tracing::info!("cli.mutate: daemon did not run this ({e}); writing in-process");
            }
        }
    }

    // A successfully routed Unix mutation returned above. Anything mutating
    // that reaches this point will execute directly: Windows has no Unix-socket
    // daemon, MDKB_NO_DAEMON is an explicit override, or daemon admission
    // proved the request never started. Serialize the complete command with the
    // universal writer-admission lock shared by daemon telemetry, watcher work
    // and schema initialization. Index-wide operations then take the narrower
    // mutation lock in the fixed order writer -> index.
    let _direct_mutation_guard = if mdkb::core::routing::routing_for(&cli.command)
        == mdkb::core::routing::Routing::Mutation
    {
        Some(mdkb::store::mutation_lock::acquire_direct_cli(&cwd)?)
    } else {
        None
    };

    match cli.command {
        Command::Init => {
            tracing::info!("Initializing mdkb...");
            handle_init(&cwd)?;
            println!("Initialized .mdkb/ in {}", cwd.display());
        }
        Command::Collection(cmd) => {
            let ctx = if matches!(&cmd, CollectionCommand::List) {
                open_reader(&cwd)?
            } else {
                open_writer(&cwd)?
            };
            match cmd {
                CollectionCommand::Add {
                    name,
                    path,
                    pattern,
                } => {
                    handle_collection_add(&ctx, &name, &path, &pattern)?;
                    println!("Added collection '{name}'");
                }
                CollectionCommand::Update {
                    name,
                    pattern,
                    path,
                } => {
                    let updated =
                        handle_collection_update(&ctx, &name, path.as_deref(), pattern.as_deref())?;
                    println!(
                        "{}",
                        collection_updated_line(&name, &updated.path, &updated.pattern)
                    );
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
            let ctx = open_reader(&cwd)?;
            announce_no_collections(&ctx)?;
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
                    let entries = handle_memory_search(&ctx, &query, limit, entry_type.as_deref())?;
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
                    // `--entry-type` narrows the memory half here too: the flag
                    // describes the memory corpus, not the scope it was asked
                    // for, and it used to be dropped silently without `--scope`.
                    let entries = handle_memory_search(&ctx, &query, limit, entry_type.as_deref())?;
                    // One JSON document, not two arrays under markdown headings:
                    // a consumer parsing stdout must never see anything else.
                    if matches!(cli.format, OutputFormat::Json) {
                        let out = serde_json::json!({
                            "documents": results,
                            "memory": entries,
                        });
                        println!("{}", serde_json::to_string_pretty(&out)?);
                        return Ok(());
                    }
                    // The headings label two sections for a human reader. CSV is
                    // read by a program: a markdown heading in that stream is
                    // noise, and the two tables stay told apart by their header
                    // rows.
                    let headings = !matches!(cli.format, OutputFormat::Csv);
                    if !results.is_empty() {
                        if headings {
                            println!("## Documents\n");
                        }
                        format_search_results(&results, cli.format);
                    }
                    if !entries.is_empty() {
                        if headings {
                            println!("## Memory Entries\n");
                        } else {
                            println!();
                        }
                        format_memory_list(&entries, cli.format);
                    }
                    if results.is_empty() && entries.is_empty() {
                        println!("No results found.");
                        if mdkb::store::search::index_is_empty(&ctx.conn)? {
                            println!("{}", mdkb::store::search::INDEX_EMPTY_HINT);
                        }
                    }
                }
                Some("code") => {
                    let scored = mdkb::cli::handlers::handle_semantic_code_search(
                        &cwd,
                        &ctx.config_path,
                        &query,
                        kind.as_deref(),
                        limit,
                    )?;
                    format_scored_symbols(&scored, cli.format);
                }
                Some("symbols") => {
                    let found = mdkb::cli::handlers::handle_symbol_search(
                        &cwd,
                        &query,
                        kind.as_deref(),
                        file.as_deref(),
                        limit,
                    )?;
                    format_code_symbols(&found.symbols, cli.format);
                    report_find_truncation(&found);
                }
                Some("duplicates") => {
                    // The query is not a query here: duplication is a sweep,
                    // and what narrows it is `--file`, not words. An empty
                    // query is therefore the ordinary case, not a mistake.
                    let report = run_dup(
                        &cwd,
                        Some(&ctx.conn),
                        &ctx.config_path,
                        &mdkb::core::dup::DupOverrides {
                            file: file
                                .clone()
                                .or_else(|| (!query.is_empty()).then(|| query.clone())),
                            ..Default::default()
                        },
                    )?;
                    print!("{}", report.markdown);
                }
                Some("coupling") => {
                    // Same shape as `duplicates` above: a sweep, not a query,
                    // so there is nothing to pass the query text to.
                    let report = mdkb::core::coupling::handle_coupling(
                        &cwd,
                        &mdkb::core::coupling::CouplingOverrides::default(),
                    )?;
                    print!("{}", report.markdown);
                }
                Some(invalid) => {
                    eprintln!(
                        "Invalid scope: '{}'. Valid values: docs, memory, code, symbols, duplicates, coupling. Omit for docs+memory.",
                        invalid
                    );
                    std::process::exit(1);
                }
            }
        }
        Command::Dup {
            semantic,
            threshold,
            min_nodes,
            file,
            since,
        } => {
            // Read-only, and tolerant of a repository nobody has indexed: the
            // report says so and exits 0. An audit that has nothing to audit is
            // not a failure.
            let ctx = open_reader(&cwd)?;
            let report = run_dup(
                &cwd,
                Some(&ctx.conn),
                &ctx.config_path,
                &mdkb::core::dup::DupOverrides {
                    semantic,
                    threshold,
                    min_nodes,
                    file,
                    since,
                },
            )?;
            print!("{}", render_dup(&report, cli.format));
        }
        Command::Coupling {
            min_cochanges,
            since,
            git_ref,
        } => {
            // Read-only, and tolerant the same way `dup` is: a repository with
            // no index, or no history to read, is reported and exits 0.
            let report = mdkb::core::coupling::handle_coupling(
                &cwd,
                &mdkb::core::coupling::CouplingOverrides {
                    min_cochanges,
                    since,
                    git_ref,
                },
            )?;
            print!("{}", render_coupling(&report, cli.format));
        }
        Command::Get { id, lines } => {
            use mdkb::cli::handlers::GetResult;
            let ctx = open_reader(&cwd)?;

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
            let ctx = open_reader(&cwd)?;
            let results = handle_mget(&ctx, &pattern, collection.as_deref())?;
            format_mget_results(&results, cli.format);
        }
        Command::Update { files, force } => {
            let ctx = open_writer(&cwd)?;
            report_unreadable_config(&ctx.config_path);
            let request = UpdateRequest { files, force };
            let outcome = run_update_in_process(&ctx, &cwd, &request)?;
            format_update_outcome(&outcome, cli.format);
        }
        Command::Embed { collection } => {
            let ctx = open_writer(&cwd)?;
            // This process exists to embed and then exit, which is the only
            // place the drop is safe: it cannot be undone without privilege,
            // so a long-lived process that did it would stay demoted. See
            // `llm::lower_process_priority` and the layering test that keeps
            // it out of the daemon.
            let nice = mdkb::config::Config::load_or_default(&ctx.config_path)
                .search
                .embed_nice;
            if let Some(applied) = mdkb::llm::lower_process_priority(nice) {
                eprintln!(
                    "mdkb: embedding at nice {applied} — it takes every idle core and \
                     yields the busy ones. Set search.embed_nice = 0 to disable."
                );
            }
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
                run_global_stdio_server("serve --global").await?;
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
            // Resolve the three facts here so the mode decision itself stays
            // a pure, unit-tested function (`resolve_mcp_run_mode`) — the
            // command arm only carries them across.
            let no_daemon = !mdkb::core::routing::daemon_serves_this_process();
            match resolve_mcp_run_mode(no_daemon, cfg!(unix), socket)? {
                McpRunMode::InProcess => {
                    // Same wire protocol as the proxy path; the only
                    // difference is the server instance lives in this process.
                    run_global_stdio_server("mcp in-process, daemon bypassed").await?;
                }
                McpRunMode::DaemonProxy { socket_override } => {
                    #[cfg(unix)]
                    {
                        let socket_path = socket_override.unwrap_or_else(|| {
                            mdkb::DaemonConfig::load_or_default(&mdkb::DaemonConfig::config_path())
                                .map(|c| c.socket_path())
                                .unwrap_or_else(|_| {
                                    mdkb::DaemonConfig::daemon_home().join("daemon.sock")
                                })
                        });
                        mdkb::cli::mcp_proxy::run_proxy(socket_path).await?;
                    }
                    // `resolve_mcp_run_mode` returns DaemonProxy only when
                    // `daemon_supported` is true, and the caller feeds that
                    // from `cfg!(unix)` — so this arm cannot be reached on a
                    // non-unix build.
                    #[cfg(not(unix))]
                    {
                        let _ = socket_override;
                        unreachable!("DaemonProxy resolved on a platform without the daemon");
                    }
                }
            }
        }
        Command::Stats { no_color } => {
            let ctx = open_reader(&cwd)?;
            announce_no_collections(&ctx)?;
            report_unreadable_config(&ctx.config_path);
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
            let ctx = open_writer(&cwd)?;
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
            mdkb::store::heal::verify_and_mark_throttled(&ctx.db_path)?;
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
        Command::Metrics(cmd) => match cmd {
            MetricsCommand::Status => {
                let ctx = open_reader(&cwd)?;
                let status = handle_metrics_status(&ctx, &cwd)?;
                format_telemetry_status(&status, cli.format);
            }
            MetricsCommand::Show { period } => {
                let ctx = open_reader(&cwd)?;
                let metrics = handle_metrics_show(&ctx, period)?;
                format_metrics_summary(&metrics, period, cli.format);
            }
            MetricsCommand::Latency { period } => {
                let ctx = open_reader(&cwd)?;
                let stats = handle_metrics_latency(&ctx)?;
                format_latency_stats(&stats, period, cli.format);
            }
            MetricsCommand::Quality { period } => {
                let ctx = open_reader(&cwd)?;
                let metrics = handle_metrics_show(&ctx, period)?;
                format_quality_metrics(&metrics, period, cli.format);
            }
            MetricsCommand::Export { period } => {
                let ctx = open_reader(&cwd)?;
                let events = handle_metrics_export(&ctx, period)?;
                format_metrics_export(&events, cli.format);
            }
            MetricsCommand::Purge { yes } => {
                if !yes {
                    return Err(mdkb::Error::other(
                        "refusing to delete query telemetry without --yes",
                    ));
                }
                let ctx = open_writer(&cwd)?;
                let deleted = handle_metrics_purge(&ctx)?;
                println!("Deleted {deleted} query telemetry event(s)");
            }
        },
        Command::Eval(cmd) => match cmd {
            EvalCommand::Recall {
                fixture,
                k,
                mode,
                download,
                min_recall,
                min_precision,
            } => {
                let modes = parse_eval_modes(&mode);
                let runs = handle_eval_recall(&EvalOptions {
                    fixture: fixture.as_deref(),
                    k,
                    modes: &modes,
                    download,
                })?;
                match cli.format {
                    OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&runs)?),
                    _ => print_eval_recall(&runs),
                }
                if let Some(floor) = min_recall {
                    let below: Vec<String> = runs
                        .iter()
                        .filter_map(|run| run.report.as_ref())
                        .filter(|r| r.recall_at_k < floor)
                        .map(|r| format!("{} {:.3}", r.mode.as_str(), r.recall_at_k))
                        .collect();
                    if !below.is_empty() {
                        return Err(mdkb::Error::other(format!(
                            "recall@{k} below {floor}: {}",
                            below.join(", ")
                        )));
                    }
                }
                if let Some(floor) = min_precision {
                    // A mode with no measured precision (the fixture labelled
                    // no negatives) cannot clear a precision floor: asking for
                    // one against an unlabelled fixture is the error.
                    let below: Vec<String> = runs
                        .iter()
                        .filter_map(|run| run.report.as_ref())
                        .filter(|r| r.precision.is_none_or(|p| p < floor))
                        .map(|r| {
                            format!(
                                "{} {}",
                                r.mode.as_str(),
                                r.precision.map_or_else(
                                    || "unmeasured".to_string(),
                                    |p| format!("{p:.3}")
                                )
                            )
                        })
                        .collect();
                    if !below.is_empty() {
                        return Err(mdkb::Error::other(format!(
                            "precision below {floor}: {}",
                            below.join(", ")
                        )));
                    }
                }
            }
            EvalCommand::Judge {
                fixture,
                k,
                mode,
                download,
            } => {
                let modes = parse_eval_modes(&mode);
                let runs = handle_eval_judge(&EvalOptions {
                    fixture: fixture.as_deref(),
                    k,
                    modes: &modes,
                    download,
                })?;
                match cli.format {
                    OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&runs)?),
                    _ => {
                        for run in &runs {
                            match &run.report {
                                Some(r) => println!(
                                    "{:<10} accuracy: {:.3}  (n={}, k={})",
                                    run.mode.as_str(),
                                    r.accuracy,
                                    r.n,
                                    r.k
                                ),
                                None => println!(
                                    "{:<10} skipped: {}",
                                    run.mode.as_str(),
                                    run.skipped.as_deref().unwrap_or("no reason given")
                                ),
                            }
                        }
                    }
                }
            }
        },
        Command::Memory(cmd) => {
            let ctx = if matches!(
                &cmd,
                MemoryCommand::Show { .. }
                    | MemoryCommand::List { .. }
                    | MemoryCommand::Search { .. }
                    | MemoryCommand::Warmup { .. }
                    | MemoryCommand::History { .. }
                    | MemoryCommand::Export { .. }
            ) {
                open_reader(&cwd)?
            } else {
                open_writer(&cwd)?
            };
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
                    relates,
                    agent,
                    on_conflict,
                    dry_run,
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
                        &relates,
                        agent.as_deref(),
                        on_conflict.as_deref(),
                        dry_run,
                    )?;
                    if dry_run {
                        println!("dry-run: validated memory entry '{id}'");
                    } else {
                        println!("Added memory entry '{id}'");
                    }
                }
                MemoryCommand::Show { id } => {
                    // A miss exits non-zero, as `get` already does for the same
                    // slug. Printing "not found" and exiting 0 makes a miss
                    // indistinguishable from a hit to any caller that scripts
                    // this command and reads only the status.
                    let entry = handle_memory_show(&ctx, &id)?.ok_or_else(|| {
                        mdkb::Error::from(mdkb::error::ErrorKind::DocumentNotFound {
                            id: id.clone(),
                        })
                    })?;
                    format_memory_entry(&entry, cli.format);
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
                    println!("Linked {id} --{relation}--> {target}");
                }
                MemoryCommand::List { limit, status } => {
                    let entries = handle_memory_list(&ctx, limit, status.as_deref())?;
                    format_memory_list(&entries, cli.format);
                }
                MemoryCommand::Search { query, limit } => {
                    let entries = handle_memory_search(&ctx, &query, limit, None)?;
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
                MemoryCommand::Sync => {
                    let s = mdkb::core::memory_sync::sync_memory_files(&ctx)?;
                    println!(
                        "Projected: {}  Imported: {}  Adopted: {}  Conflicts: {}  \
                         Revived: {}  Archived: {}",
                        s.projected, s.imported, s.adopted, s.conflicts, s.revived, s.archived
                    );
                    print_memory_sync_warnings(&s);
                }
                MemoryCommand::Import {
                    path,
                    dry_run,
                    skip_duplicates,
                } => {
                    let p = std::path::Path::new(&path);
                    // A single .md file is a RESTORE, not a bulk import: it
                    // preserves the timestamps and counters the file records,
                    // because collapsing months of history into the moment of
                    // the restore destroys recency ranking (story 017-a378).
                    // A directory or a .json file keeps the bulk semantics,
                    // where telemetry is DB-owned and starts fresh.
                    let result = if p.is_dir() {
                        handle_memory_import_dir(&ctx, p, dry_run, skip_duplicates)?
                    } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
                        if dry_run {
                            println!("Dry run: would restore {}", p.display());
                            return Ok(());
                        }
                        handle_memory_import_file(&ctx, p)?;
                        println!("Restored 1 entry from {}", p.display());
                        return Ok(());
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
                MemoryCommand::Audit { dry_run } => {
                    let outcome = mdkb::core::memory_audit::handle_memory_audit(&ctx, dry_run)?;
                    format_audit_result(&outcome, dry_run, cli.format);
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
            let ctx = open_writer(&cwd)?;
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
            let ctx = open_reader(&cwd)?;
            let history = handle_history(&ctx, &path)?;
            format_evolution_history(&history, cli.format);
        }
        Command::Current { path } => {
            let ctx = open_reader(&cwd)?;
            if let Some(doc) = handle_current(&ctx, &path)? {
                format_current_document(&doc, cli.format);
            } else {
                println!("No current version found for '{path}'");
            }
        }
        Command::SupersededBy { path } => {
            let ctx = open_reader(&cwd)?;
            let evolutions = handle_superseded_by(&ctx, &path)?;
            format_superseded_by(&evolutions, cli.format);
        }
        Command::Graph(cmd) => {
            let ctx = open_reader(&cwd)?;
            announce_no_collections(&ctx)?;
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
                GraphCommand::Dangling { collection } => {
                    let dangling = handle_graph_dangling(&ctx, collection.as_deref())?;
                    format_graph_dangling(&dangling, cli.format);
                }
                GraphCommand::Hubs { relation, limit } => {
                    let hubs = handle_graph_hubs(&ctx, relation.as_deref(), limit)?;
                    format_graph_hubs(&hubs, cli.format);
                }
                GraphCommand::Relations { apply } => {
                    let found = handle_graph_relations(&ctx)?;
                    let mode = mdkb::config::Config::load_or_default(&ctx.config_path)
                        .graph
                        .relations;
                    let applied = if apply {
                        apply_relations_if_useful(&ctx, mode, &found)?
                    } else {
                        None
                    };
                    format_graph_relations(&found, mode, applied.as_ref(), cli.format);
                }
            }
        }
        Command::Experiment(cmd) => {
            let ctx = if matches!(
                &cmd,
                ExperimentCommand::Status { .. } | ExperimentCommand::List { .. }
            ) {
                open_reader(&cwd)?
            } else {
                open_writer(&cwd)?
            };
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
            let ctx = open_writer(&cwd)?;
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
        Command::Surface => {
            print!("{}", mdkb::core::surface::render());
            return Ok(());
        }
        Command::Cheatsheet => {
            print!(
                "\
# Search
mdkb search <query>                                    # docs + memory (default)
mdkb search <query> --scope memory                     # memory only
mdkb search <query> --scope memory --entry-type TYPE    # filter by type
mdkb search <query> --scope docs                        # documents only
mdkb search <query> --scope symbols                     # symbol definitions (fuzzy)
mdkb search <query> --scope symbols --file hook         # path substring, not a glob
mdkb search <query> --scope code                        # semantic code search
mdkb search <query> -c <collection>                     # a collection is -c, NOT --scope
mdkb get <id|path|collection/path|collection:path|slug> # id, path (with or without .md), or memory slug
mdkb get <id> --lines 10:50                             # line range
mdkb mget <pattern>                                     # several documents at once

# Memory (--entry-type: topic, problem, decision, reminder, prior)
mdkb memory add <id> --title T --content C              # create (default: topic)
mdkb memory add <id> --title T --content C --source-type official_docs  # provenance/trust
mdkb memory add <id> --title T --content C --entry-type prior --tags t1,t2
mdkb memory add <id> --title T --content C --entry-type reminder --due-in 3600
mdkb memory confirm <id> --outcome confirmed            # raise confidence (or refuted)
mdkb memory link <id> <relation> <target>              # typed edge (supports|contradicts|supersedes|derived_from|relates_to)
mdkb memory link <id> derived_from <path> --doc        # link to a document; --agent records provenance
mdkb memory rm <id>                                     # delete
mdkb memory list                                        # list active entries
mdkb memory show <id>                                   # one entry in full
mdkb memory search <query>                              # memory only, same as search --scope memory
mdkb memory warmup                                      # compact index to load at session start
mdkb memory history <id>                                # revisions, including versions a conflict superseded

# Memory projection (entries live in the database; .mdkb/memory/entries/*.md is
# the git-tracked copy, and the only copy that survives losing the database)
mdkb memory export                                      # write missing entries to disk; add --overwrite to refresh all
mdkb memory sync                                        # reconcile database and disk both ways
mdkb memory import <path>                               # load a JSON file or a folder of markdown

# Code intelligence
mdkb code callers <symbol>                              # who calls this?
mdkb code calls <symbol>                                # what does this call?
mdkb code impact <symbol>                               # transitive dependency graph
mdkb code search <query>                                # fuzzy symbol search
mdkb code find <name>                                   # exact symbol lookup; --kind and --file narrow it
mdkb code info                                          # code index counts

# Audits (both read the code index)
mdkb dup                                                # duplication sweep; structural pass only, no model
mdkb dup --semantic                                     # add the embedding pass: minutes, not seconds ([code.duplication] semantic = true makes it standing)
mdkb dup --since HEAD                                   # review mode: only clusters the change touched
mdkb coupling                                           # files that change together without referencing each other

# Knowledge graph (frontmatter + wikilink edges; refs accept collection-prefixed paths)
mdkb graph links <entity>                               # outgoing edges (endpoints shown as paths, with 'via' relation)
mdkb graph backlinks <entity>                           # incoming edges
mdkb graph neighbors <entity> --depth 2                 # adjacent entities (undirected), each with 'via'
mdkb graph path <a> <b>                                 # shortest path between two entities
mdkb graph dangling [-c <collection>]                   # refs resolving to no doc (full scan; explicit only)
mdkb graph hubs --relation owner --limit 20             # entities by degree centrality (full scan; explicit only)
mdkb graph relations [--apply]                          # keys whose values name real docs; --apply writes them (no-op in auto mode)

# Collections
mdkb collection list                                    # name, path, pattern, doc count per collection
mdkb collection add <name> <path> -p '**/*.md'          # register a collection
mdkb collection update <name> --path <path> -p '**/*.md' # change path/pattern without dropping matching embeddings

# Developer feedback (local; prompt text is never stored)
mdkb setup developer                                    # enable private per-repository recall telemetry
mdkb metrics status                                     # activation, retention, key, stored event count
mdkb metrics quality --period 7                         # zero results, repeated searches, score bands
mdkb metrics latency --period 7                         # recall count and latency
mdkb metrics purge --yes                                # delete all stored query telemetry

# Maintenance
mdkb update                                             # reindex all (auto-embeds docs + backfills memory)
mdkb embed --collection claude_sessions                # embed a specific collection (sessions excluded by default)
mdkb stats                                              # index health, hooks, mining, sessions
mdkb compact                                            # vacuum both databases
mdkb compact --prune-sessions --older-than 90d --export dir  # hard-delete archived transcripts (exports first)
mdkb memory prune --days 90 --dry-run                   # preview: expired entries + reminders/priors/handoffs unread for 90d
mdkb memory audit                                       # entries worth re-reading (dead refs, drifted source, duplicates); decides nothing

# Daemon (the daemon owns every write; the CLI routes mutations to it)
mdkb daemon status                                      # is it running, and against which store
mdkb daemon restart                                     # after upgrading the binary
MDKB_NO_DAEMON=1 mdkb <cmd>                             # run in-process instead, for debugging
MDKB_NAMESPACE=<name> mdkb <cmd>                        # use .mdkb/namespaces/<name>/ instead; test runners get `test` unasked

# Naming
mdkb surface                                            # each MCP tool next to its CLI equivalent
mdkb schema [COMMAND]                                   # the CLI as JSON, for machine callers

# Repo selector (the MCP `root` parameter; the CLI always works in the current repo)
(omitted)                                               # the declared workspace, and every store nested under it
root=\"/abs/path\"                                        # one repo by path; need not be a known repo
root=\"name\"                                             # one repo by name, the last component of a known root
root=\"name,/abs/path\"                                   # several repos, comma-separated, names and paths mixed
root=\"*\"                                                # every known repo (`mdkb daemon status` lists them)
# Only `search` fans out; every other tool needs a selector naming one repo —
# with `root` omitted that is the declared workspace itself, when it is a store.
# A comma always separates repos, so a path containing one is refused, not split.

# A store carries a schema version. When the STORE is newer than the binary,
# every command refuses rather than touch it. When the store is OLDER, a read
# command migrates it and says so on stderr — that migration also rewrites
# memory entries and prior clusters, so back the store up first if it matters.
"
            );
            if let Some(exe) = std::env::current_exe()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
            {
                println!(
                    "\n# Fallback: if `mdkb` is not on PATH, this binary answered from: {exe}"
                );
            }
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
                let found =
                    mdkb::cli::handlers::handle_code_search(&cwd, &query, limit, kind.as_deref())?;
                format_code_symbols(&found.symbols, cli.format);
                report_find_truncation(&found);
            }
            CodeCommand::Find {
                name,
                kind,
                file,
                limit,
            } => {
                let found = mdkb::cli::handlers::handle_code_find(
                    &cwd,
                    &name,
                    kind.as_deref(),
                    file.as_deref(),
                    limit,
                )?;
                format_code_symbols(&found.symbols, cli.format);
                report_find_truncation(&found);
            }
            CodeCommand::Calls { name } => {
                let (source, callees, unplaced) =
                    mdkb::cli::handlers::handle_code_calls(&cwd, &name)?;
                format_code_calls(&source, &callees, cli.format);
                format_unplaced_calls(&unplaced, cli.format);
            }
            CodeCommand::Callers { name } => {
                let (target, callers, unplaced) =
                    mdkb::cli::handlers::handle_code_callers(&cwd, &name)?;
                format_code_graph("Called by", &target, &callers, cli.format);
                format_unplaced_arrivals(unplaced, &target, cli.format);
            }
            CodeCommand::Impact { name, depth } => {
                let (source, report) = mdkb::cli::handlers::handle_code_impact(&cwd, &name, depth)?;
                format_code_impact(&source, &report, cli.format);
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
            SetupCommand::Developer {
                retention_days,
                dry_run,
            } => {
                let result =
                    mdkb::cli::setup::handle_setup_developer(&cwd, retention_days, dry_run)?;
                if !result.dry_run {
                    println!("Developer telemetry enabled for {}", cwd.display());
                    println!("Retention: {} days", result.retention_days);
                    println!("Config: {}", result.config_path.display());
                    println!("Private key: {}", result.key_path.display());
                    println!("Restart the mdkb daemon to activate the changed repository config.");
                }
            }
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
                    http_url,
                } => {
                    let result = mdkb::cli::setup::handle_setup_hooks_claude_with_http(
                        &cwd,
                        &scope,
                        &disable,
                        dry_run,
                        profile_dir.as_deref(),
                        http_url.as_deref(),
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
            SetupCommand::Check => {
                use mdkb::cli::setup::DistillerCheck;
                // Both halves are reported before exiting, so one failure never
                // hides the other.
                let mut failed = false;

                let hooks = mdkb::cli::setup::check_hooks(&cwd)?;
                println!("Claude hooks: user {}", hooks.user_path.display());
                println!("              local {}", hooks.local_path.display());
                if hooks.drift.is_clean() {
                    println!("  OK — every lifecycle event is registered exactly once");
                } else {
                    if !hooks.drift.missing.is_empty() {
                        eprintln!(
                            "  MISSING: {} — these events never fire; run: mdkb setup hooks claude",
                            hooks.drift.missing.join(", ")
                        );
                    }
                    if !hooks.drift.duplicated.is_empty() {
                        eprintln!(
                            "  DUPLICATED: {} — these events fire twice per turn",
                            hooks.drift.duplicated.join(", ")
                        );
                    }
                    failed = true;
                }

                match mdkb::cli::setup::check_distiller(&cwd) {
                    DistillerCheck::NotConfigured(reason) => {
                        println!("Prior distiller: not configured ({reason})");
                    }
                    DistillerCheck::Pass { program, lesson } => {
                        println!("Prior distiller: OK ({program})");
                        println!("  distilled: {lesson}");
                    }
                    DistillerCheck::Fail { program, detail } => {
                        eprintln!("Prior distiller: FAILED ({program})");
                        eprintln!("  {detail}");
                        failed = true;
                    }
                }

                // Non-zero exit: this command exists to be run from a script
                // or a setup checklist, and a check that reports failure on
                // stdout while exiting 0 is a check nobody notices.
                if failed {
                    std::process::exit(1);
                }
            }
        },
        Command::Session(cmd) => match cmd {
            SessionCommand::Index {
                sessions_path,
                project_root,
            } => {
                let ctx = open_writer(&cwd)?;
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
        Command::Hook(hook_cmd) => match hook_cmd {
            HookCommand::SessionStart => dispatch_hook("hook.session_start").await?,
            HookCommand::UserPromptSubmit => dispatch_hook("hook.user_prompt_submit").await?,
            HookCommand::PostToolUse => dispatch_hook("hook.post_tool_use").await?,
            HookCommand::PreToolUse => dispatch_hook("hook.pre_tool_use").await?,
            HookCommand::Stop => dispatch_hook("hook.stop").await?,
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
        },
    }

    Ok(())
}

/// Read one lifecycle event from stdin and forward it under the resolved store.
///
/// The five hook subcommands differed only by method name, so the skip rules
/// live here once: `None` from the resolver means no directory here may hold a
/// store (a container of repos, or `$HOME`), and `.mdkbignore` is the repo's own
/// opt-out. Either way the hook exits quietly — hosts require exit 0.
async fn dispatch_hook(method: &str) -> Result<()> {
    let input = hook_logic::read_stdin_best_effort();
    let event = hook_logic::parse_event(&input);
    let Some(root) = hook_client::resolve_hook_root(&event, None) else {
        tracing::debug!("{method}: no directory here may hold a store; skipping");
        return Ok(());
    };
    if hook_logic::mdkbignore_hooks_present(&root) {
        return Ok(());
    }
    hook_client::call_hook_event(method, event, Some(root)).await
}

/// How `mdkb mcp` runs on this invocation.
///
/// Kept as data (instead of branching inline in the command arm) so the
/// platform/env/flag decision is a pure function with unit tests that run on
/// every host: the daemon itself is unix-only, but the *rule* for picking a
/// mode is portable and must stay visible to CI regardless of platform.
#[derive(Debug, Clone, PartialEq, Eq)]
enum McpRunMode {
    /// Bridge the client's stdio transport to the singleton daemon over its
    /// unix socket (the single-writer architecture; see the routing notes at
    /// the top of `run_cli`).
    ///
    /// `socket_override` carries the `--socket` path when one was typed, so
    /// the resolver — not the command arm — is the place that proves the
    /// override survives. `None` means "use the config default".
    DaemonProxy {
        socket_override: Option<std::path::PathBuf>,
    },
    /// Serve MCP directly from this process — no daemon involved.
    InProcess,
}

/// Decide how `mdkb mcp` runs, from the three facts that matter.
///
/// The caller resolves the facts; this function only applies the rules:
/// - `no_daemon`: MDKB_NO_DAEMON is set (the explicit escape hatch),
/// - `daemon_supported`: this platform has the unix-socket daemon
///   (fed from `cfg!(unix)`),
/// - `socket`: the `--socket` path, when one was passed (an explicit ask
///   for the daemon proxy at that path).
///
/// Rules, in order:
/// 1. The proxy runs only when the daemon is both wanted and available:
///    MDKB_NO_DAEMON is the escape hatch that keeps the daemon out of the
///    path anywhere, and a platform without the daemon has nothing to proxy
///    to. Either one alone selects the in-process server. This is the
///    Windows default: the unix-socket daemon has no Windows port, and
///    erroring here instead surfaced to MCP clients as an opaque
///    CONNECTION_CLOSED at session start — from the config `mdkb setup mcp`
///    writes.
/// 2. A `--socket` that the chosen mode cannot honor is an operational
///    consistency error (by convention). The flag names the daemon proxy, so
///    ignoring it leaves the user believing the daemon serves them while the
///    in-process server does instead. The same rule covers both routes into
///    in-process mode.
///
/// The same three steps, stated plainly:
/// 1. Is the daemon both allowed and available? If so, run the proxy. A
///    typed `--socket` path rides along in the mode and replaces the config
///    default.
/// 2. Is the daemon out of reach, but `--socket` was typed? That is an
///    error, because the flag works only with the proxy. The message names
///    the cause — MDKB_NO_DAEMON, or a platform without the daemon —
///    because each cause has its own remedy.
/// 3. Otherwise run the in-process server.
fn resolve_mcp_run_mode(
    no_daemon: bool,
    daemon_supported: bool,
    socket: Option<std::path::PathBuf>,
) -> Result<McpRunMode> {
    if !no_daemon && daemon_supported {
        return Ok(McpRunMode::DaemonProxy {
            socket_override: socket,
        });
    }
    if socket.is_some() {
        // Name the reason the proxy is out of reach — the two causes need
        // different remedies, so one generic message would not help.
        let reason = if no_daemon {
            "MDKB_NO_DAEMON forces the in-process server; unset it or drop --socket"
        } else {
            "the daemon proxy requires Unix; drop --socket to serve in-process here"
        };
        return Err(mdkb::Error::other(format!(
            "--socket selects the daemon proxy, but {reason}"
        )));
    }
    Ok(McpRunMode::InProcess)
}

/// Serve MCP in global (multi-repo) mode over stdio, from this process.
///
/// One body for every in-process stdio entry point — `mdkb serve --global`
/// and the in-process path of `mdkb mcp`. The wire behavior must stay
/// identical across those entry points, so they share this function instead
/// of each carrying a copy of the setup.
///
/// `mode_note` only feeds the startup log line, so an operator reading logs
/// can tell which entry point put the server in-process.
async fn run_global_stdio_server(mode_note: &str) -> Result<()> {
    let daemon_config = mdkb::DaemonConfig::load_or_default(&mdkb::DaemonConfig::config_path())?;
    let registry = std::sync::Arc::new(mdkb::daemon::registry::RepoRegistry::new(daemon_config));
    let server = mdkb::mcp::server::McpServer::global(registry);

    tracing::info!("Starting mdkb MCP server in global mode (stdio, {mode_note})...");
    let (stdin, stdout) = rmcp::transport::io::stdio();
    let service = server
        .serve((stdin, stdout))
        .await
        .map_err(|e| mdkb::Error::other(format!("Failed to start server: {e}")))?;
    service
        .waiting()
        .await
        .map_err(|e| mdkb::Error::other(format!("Server error: {e}")))?;
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

/// Run all three phases of `update` in this process.
///
/// The in-process twin of `mcp::dispatch::update_impl`, which runs the same
/// three phases against the daemon's long-lived handles. What must not diverge
/// — which document handler runs, and whether sessions run at all — lives in
/// `core::indexing` and is called by both; what differs here is only how the
/// connections are acquired.
fn run_update_in_process(
    ctx: &Context,
    root: &Path,
    request: &UpdateRequest,
) -> Result<UpdateOutcome> {
    let docs = update_documents(ctx, root, request)?;

    let (code, code_error) = match mdkb::cli::handlers::handle_code_index(root, &request.files) {
        Ok(stats) => (report_code_stats(request.is_targeted(), stats), None),
        Err(e) => {
            tracing::warn!("Code reindexing failed: {e:?}");
            (None, Some(e.to_string()))
        }
    };

    let sessions = if request.is_targeted() {
        None
    } else {
        index_sessions_in_process(ctx, root)
    };

    Ok(UpdateOutcome {
        docs,
        code,
        code_error,
        sessions,
    })
}

/// The session leg of an update, or `None` when it did nothing worth reporting.
///
/// Every failure here is a warning, not an error: sessions index transcripts
/// the user never asked mdkb to own, and losing them must not fail an update of
/// the documents they did.
fn index_sessions_in_process(ctx: &Context, root: &Path) -> Option<mdkb::domain::UpdateResult> {
    let home = match mdkb::daemon::config::home_dir() {
        Ok(home) => home,
        Err(e) => {
            tracing::warn!("Session indexing skipped: cannot resolve home dir: {e}");
            return None;
        }
    };
    let project_root = root.to_string_lossy().to_string();
    match handle_session_index(ctx, &home.join(".claude/projects"), &project_root) {
        Ok(sr) if sr.added > 0 || sr.updated > 0 || sr.sessions_archived > 0 => Some(sr),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!("Session indexing failed: {e:?}");
            None
        }
    }
}

/// Print everything one `update` did, in the format the caller asked for.
///
/// The single renderer for both paths, which is why the daemon returns numbers
/// and never prose: text formatted inside the daemon is text `--format json`
/// cannot undo, and a routed update that reads differently from a direct one is
/// a routing layer the user can feel.
///
/// One run produces one document. The phases used to print themselves in
/// sequence, so `--format json` emitted three separate objects with prose
/// headers between them and `--format csv` two tables plus a sentence — output
/// that reads fine to a human and cannot be parsed by anything else, which is
/// the entire point of asking for those formats.
fn format_update_outcome(outcome: &UpdateOutcome, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(outcome).unwrap()),
        OutputFormat::Csv => print_update_outcome_csv(outcome),
        OutputFormat::Markdown | OutputFormat::Text => {
            format_update_result(&outcome.docs, format);
            if let Some(stats) = &outcome.code {
                println!("\nCode index:");
                format_code_index_stats(stats, format);
            }
            if let Some(sr) = &outcome.sessions {
                println!(
                    "\nSessions: {} added, {} updated, {} unchanged, {} archived",
                    sr.added, sr.updated, sr.unchanged, sr.sessions_archived
                );
            }
        }
    }
    // On stderr in every format, so a machine-readable run on stdout stays
    // machine-readable and a failed code phase is still impossible to miss.
    if let Some(e) = &outcome.code_error {
        eprintln!("Warning: code reindexing failed: {e}");
    }
}

/// One header, one row, every phase — a phase that did not run leaves its cells
/// empty rather than dropping columns, so a caller can diff two runs.
fn print_update_outcome_csv(outcome: &UpdateOutcome) {
    println!(
        "docs_indexed,added,updated,removed,unchanged,errors,\
         code_files_discovered,code_files_indexed,code_files_removed,code_symbols_indexed,\
         code_relationships,code_parse_errors,\
         sessions_added,sessions_updated,sessions_unchanged,sessions_archived"
    );

    let d = &outcome.docs;
    print!(
        "{},{},{},{},{},{}",
        d.docs_indexed(),
        d.added,
        d.updated,
        d.removed,
        d.unchanged,
        d.errors.len()
    );
    match &outcome.code {
        Some(c) => print!(
            ",{},{},{},{},{},{}",
            c.files_discovered,
            c.files_indexed,
            c.files_removed,
            c.symbols_indexed,
            c.relationships_collected,
            c.parse_errors
        ),
        None => print!(",,,,,,"),
    }
    match &outcome.sessions {
        Some(s) => println!(
            ",{},{},{},{}",
            s.added, s.updated, s.unchanged, s.sessions_archived
        ),
        None => println!(",,,,"),
    }
}

/// Print what the daemon returned for a routed mutation.
///
/// Every result remains structured across the socket; this CLI process owns
/// all user-facing stdout and stderr formatting.
fn print_routed_result(
    command: &Command,
    result: &mdkb::core::cli_mutation::CliMutationResult,
    format: OutputFormat,
) -> Result<()> {
    use mdkb::core::cli_mutation::{CliMutationResult as R, MemoryImportOutcome};
    match (command, result) {
        (Command::Update { .. }, R::Update { outcome }) => format_update_outcome(outcome, format),
        (
            Command::Embed { .. },
            R::Embed {
                generated,
                skipped,
                errors,
            },
        ) => format_embed_result(
            &EmbedResult {
                generated: *generated,
                skipped: *skipped,
                errors: errors.clone(),
            },
            format,
        ),
        (
            Command::Compact { export, .. },
            R::Compact {
                prune,
                index_bytes,
                code_bytes,
            },
        ) => {
            if let Some(summary) = prune {
                if let Some(dir) = export
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .or_else(|| summary.export_dir.clone())
                {
                    eprintln!(
                        "Pruned {} archived session(s); exported {} to {}",
                        summary.pruned, summary.exported, dir
                    );
                } else {
                    eprintln!("Pruned {} archived session(s)", summary.pruned);
                }
            }
            eprintln!("index.sqlite vacuumed ({} KB)", index_bytes / 1024);
            if let Some(bytes) = code_bytes {
                eprintln!("code.sqlite vacuumed ({} KB)", bytes / 1024);
            }
        }
        (Command::Collection(CollectionCommand::Add { name, .. }), R::CollectionAdded) => {
            println!("Added collection '{name}'");
        }
        (
            Command::Collection(CollectionCommand::Remove { name }),
            R::CollectionRemoved { removed },
        ) => {
            if *removed {
                println!("Removed collection '{name}'");
            } else {
                println!("Collection '{name}' not found");
            }
        }
        (
            Command::Collection(CollectionCommand::Rename { old_name, new_name }),
            R::CollectionRenamed,
        ) => println!("Renamed collection '{old_name}' to '{new_name}'"),
        (
            Command::Collection(CollectionCommand::Update { name, .. }),
            R::CollectionUpdated { path, pattern },
        ) => println!("{}", collection_updated_line(name, path, pattern)),
        (Command::Memory(MemoryCommand::Add { id, dry_run, .. }), R::MemoryAdded) => {
            if *dry_run {
                println!("dry-run: validated memory entry '{id}'");
            } else {
                println!("Added memory entry '{id}'");
            }
        }
        (Command::Memory(MemoryCommand::Confirm { .. }), R::MemoryConfirmed { outcome }) => {
            match format {
                OutputFormat::Json => println!("{}", serde_json::to_string_pretty(outcome)?),
                _ => println!("{}", outcome.message),
            }
        }
        (
            Command::Memory(MemoryCommand::Link {
                id,
                relation,
                target,
                ..
            }),
            R::MemoryLinked,
        ) => println!("Linked {id} --{relation}--> {target}"),
        (Command::Memory(MemoryCommand::Rm { id }), R::MemoryRemoved { deleted }) => {
            if *deleted {
                println!("Deleted memory entry '{id}'");
            } else {
                println!("Memory entry '{id}' not found");
            }
        }
        (Command::Metrics(MetricsCommand::Purge { .. }), R::MetricsPurged { deleted }) => {
            println!("Deleted {deleted} query telemetry event(s)");
        }
        (Command::Memory(MemoryCommand::Sync), R::MemorySynced { summary }) => {
            println!(
                "Projected: {}  Imported: {}  Adopted: {}  Conflicts: {}  Revived: {}  Archived: {}",
                summary.projected,
                summary.imported,
                summary.adopted,
                summary.conflicts,
                summary.revived,
                summary.archived
            );
            print_memory_sync_warnings(summary);
        }
        (
            Command::Memory(MemoryCommand::Import { path, dry_run, .. }),
            R::MemoryImported { outcome },
        ) => match outcome {
            MemoryImportOutcome::RestoreDryRun => println!("Dry run: would restore {path}"),
            MemoryImportOutcome::Restored => println!("Restored 1 entry from {path}"),
            MemoryImportOutcome::Bulk { outcome } => {
                if *dry_run {
                    println!("Dry run: would import {} entries", outcome.imported);
                } else {
                    println!("Imported {} entries", outcome.imported);
                }
                if outcome.skipped > 0 {
                    println!("Skipped {} duplicates", outcome.skipped);
                }
                for error in &outcome.errors {
                    eprintln!("Error: {error}");
                }
            }
        },
        (Command::Memory(MemoryCommand::Prune { days, dry_run }), R::MemoryPruned { ids }) => {
            format_prune_result(ids, *days, *dry_run, format);
        }
        (Command::Memory(MemoryCommand::Audit { dry_run }), R::MemoryAudited { outcome }) => {
            format_audit_result(outcome, *dry_run, format);
        }
        #[cfg(feature = "llm")]
        (
            Command::Memory(MemoryCommand::Condense { dry_run, .. }),
            R::MemoryCondensed { outcome },
        ) => format_condense_result(outcome, *dry_run, format),
        (Command::Evolve(cmd), R::EvolutionCreated { id }) => match cmd {
            EvolveCommand::Supersedes { new, old, .. } => {
                println!("Created evolution relationship #{id}: {new} supersedes {old}");
            }
            EvolveCommand::Updates { new, old, .. } => {
                println!("Created evolution relationship #{id}: {new} updates {old}");
            }
            EvolveCommand::Corrects { new, old, .. } => {
                println!("Created evolution relationship #{id}: {new} corrects {old}");
            }
            EvolveCommand::Retracts { new, old, .. } => {
                println!("Created evolution relationship #{id}: {new} retracts {old}");
            }
            EvolveCommand::Extends { new, old, .. } => {
                println!("Created evolution relationship #{id}: {new} extends {old}");
            }
        },
        (
            Command::Experiment(ExperimentCommand::Create { .. }),
            R::ExperimentCreated { id, name },
        ) => println!("Created experiment '{name}' (ID: {id})"),
        (
            Command::Experiment(ExperimentCommand::End { name, .. }),
            R::ExperimentEnded { winner },
        ) => match winner {
            Some(winner) => println!("Experiment '{name}' ended with winner: {winner}"),
            None => println!("Experiment '{name}' ended with no significant winner"),
        },
        (Command::Experiment(ExperimentCommand::Cancel { name }), R::ExperimentCancelled) => {
            println!("Experiment '{name}' cancelled");
        }
        (
            Command::Journal(JournalCommand::Import { dry_run, .. }),
            R::JournalImported { outcome },
        ) => format_journal_import_result(outcome, *dry_run, format),
        (
            Command::Journal(JournalCommand::ImportAll { dry_run, .. }),
            R::JournalsImported { outcomes },
        ) => format_journal_import_all_results(outcomes, *dry_run, format),
        (Command::Code(CodeCommand::Init), R::CodeInitialized) => {
            println!("Initialized code index at .mdkb/code.sqlite");
        }
        (Command::Code(CodeCommand::Index { .. }), R::CodeIndexed { stats }) => {
            format_code_index_stats(stats, format);
        }
        (Command::Session(SessionCommand::Index { .. }), R::SessionIndexed { outcome }) => {
            format_update_result(outcome, format);
        }
        _ => {
            return Err(mdkb::Error::other(
                "cli.mutate returned a result for a different command",
            ));
        }
    }
    Ok(())
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
            // Per-collection, because the total above cannot distinguish a
            // healthy re-index from one collection dropping to zero while
            // another grows. A collection going 2307 -> absent is what this
            // line exists to make visible.
            for c in &result.collections {
                let delta = match c.previous {
                    Some(p) if p != c.documents => {
                        format!(
                            " ({}{})",
                            if c.documents > p { "+" } else { "" },
                            c.documents as i64 - p as i64
                        )
                    }
                    _ => String::new(),
                };
                println!("  {:<24} {:>6} docs{delta}", c.name, c.documents);
            }
            for name in &result.collections_vanished {
                println!(
                    "⚠ Collection `{name}` was registered on a previous run and is GONE — \
                     its documents are not indexed. Restore with \
                     `mdkb collection add {name} <path> && mdkb update`, and check \
                     `mdkb stats` for a quarantine."
                );
            }
            if result.no_collections_registered {
                println!(
                    "⚠ No document collection is registered — this run indexed nothing. \
                     Add one with `mdkb collection add <name> <path>`."
                );
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
            if result.memory_files_imported > 0 {
                println!(
                    "Memory entries imported from files: {}",
                    result.memory_files_imported
                );
            }
            if result.memory_files_adopted > 0 {
                println!(
                    "Memory entries updated from files: {}",
                    result.memory_files_adopted
                );
            }
            if result.memory_entries_revived > 0 {
                println!(
                    "Memory entries revived (file returned): {}",
                    result.memory_entries_revived
                );
            }
            if result.memory_entries_archived > 0 {
                println!(
                    "Memory entries archived (file deleted): {}",
                    result.memory_entries_archived
                );
            }
            // Name where the files went: an entry that lapsed is recoverable,
            // and a count alone reads like a deletion.
            if result.memory_entries_expired > 0 {
                println!(
                    "Memory entries archived (TTL lapsed): {} — files moved to \
                     .mdkb/memory/archive/",
                    result.memory_entries_expired
                );
            }
            // Conflicts and quarantines demand a human decision; a colleague
            // whose edit lost must be able to find it, so name the command that
            // retrieves the preserved version rather than only counting.
            if result.memory_sync_conflicts > 0 {
                println!(
                    "⚠ Memory conflicts resolved by newest edit: {} — the superseded \
                     versions are kept; inspect with `mdkb memory history <id>`.",
                    result.memory_sync_conflicts
                );
            }
            if result.memory_files_quarantined > 0 {
                println!(
                    "⚠ Memory files SKIPPED as unreadable: {} (unresolved merge markers, \
                     bad frontmatter, or id/filename mismatch) — see the warning log, \
                     fix them and re-run.",
                    result.memory_files_quarantined
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
            if let Some(warning) = &result.memory_gitignore_shadowed {
                println!("⚠ {warning}");
            }
            // Said out loud because widening a collection pattern can multiply
            // a store's index — one measured store went from 49 documents to
            // 690 — and the user needs to be able to see it and undo it.
            if !result.pattern_upgrades.is_empty() {
                println!("Corrected collection patterns mdkb had written:");
                for upgrade in &result.pattern_upgrades {
                    println!("  - {upgrade}");
                }
                println!("  Revert one with: mdkb collection update <name> --pattern '<old>'");
            }
            if !result.contested_identities.is_empty() {
                println!("Identities more than one document claims:");
                for collision in &result.contested_identities {
                    println!("  - {collision}");
                }
                println!(
                    "  Every reference to one of these resolves to a single document and the \
                     others are unreachable by that name. Find them with: mdkb graph backlinks \
                     <identity>"
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

/// Surface the two sync outcomes a human has to act on: a resolved conflict
/// (someone's edit lost and needs retrieving) and an ignore rule that silently
/// makes the whole projection untrackable.
fn print_memory_sync_warnings(s: &mdkb::core::memory_sync::MemorySyncSummary) {
    if s.conflicts > 0 {
        println!(
            "⚠ {} conflict(s) resolved by newest edit — the superseded versions are \
             kept; inspect with `mdkb memory history <id>`.",
            s.conflicts
        );
    }
    if s.quarantined > 0 {
        println!(
            "⚠ {} file(s) skipped as unreadable (unresolved merge markers, bad \
             frontmatter, or id/filename mismatch) — see the warning log.",
            s.quarantined
        );
    }
    if s.duplicates_skipped > 0 {
        println!(
            "⚠ {} file(s) not imported — the entry is already here under another id. \
             Keep one and delete the other file.",
            s.duplicates_skipped
        );
    }
    if s.archive_skipped > 0 {
        println!(
            "⚠ Archival SKIPPED — {} projected files missing at once (suspected bulk \
             loss). Restore .mdkb/memory/entries/ and re-run.",
            s.archive_skipped
        );
    }
    if let Some(warning) = &s.gitignore_shadowed {
        println!("⚠ {warning}");
    }
}

/// Render an audit timestamp as the day a human reads.
///
/// One function for every date the audit prints: the signal lines and the
/// "last audited" note showed the same derivation typed twice. A timestamp
/// chrono cannot turn into a date is printed raw rather than dropped — the
/// audit exists to report, and a number the reader can question beats a
/// silence they cannot.
fn audit_date(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Render one audit signal as the single line a human reads.
///
/// Kept out of the JSON arm on purpose: the structured output carries the
/// variant and its fields, and a machine reading it does not need a sentence.
fn audit_signal_line(signal: &mdkb::core::memory_audit::AuditSignal) -> String {
    use mdkb::core::memory_audit::AuditSignal as S;
    match signal {
        S::DeadCodeReference { reference } => format!("cites {reference}, which is gone"),
        S::SourceChangedSince { path, changed_at } => {
            format!(
                "{path} changed on {} since this was written",
                audit_date(*changed_at)
            )
        }
        S::NearDuplicate { other, similarity } => {
            format!("near-duplicate of {other} ({similarity:.3})")
        }
        S::Contradicts { other } => format!("contradicts {other}, unresolved"),
        S::Expired { expires_at } => format!("expired on {}", audit_date(*expires_at)),
        S::AgedLifecycle { last_touched } => {
            format!("lifecycle entry unread since {}", audit_date(*last_touched))
        }
        S::ReferenceCheckFailed { reference } => {
            format!("could not check whether {reference} is still live: git failed to answer")
        }
    }
}

fn format_audit_result(
    outcome: &mdkb::core::memory_audit::AuditOutcome,
    dry_run: bool,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(outcome).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,entry_type,signal_count");
            for c in &outcome.candidates {
                println!("{},{},{}", c.id, c.entry_type, c.signals.len());
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            if outcome.candidates.is_empty() {
                println!(
                    "Scanned {} entries; nothing selected for re-reading.",
                    outcome.scanned
                );
                print_audit_gaps(outcome);
                return;
            }
            println!(
                "Scanned {} entries; {} worth re-reading. The audit decides nothing \
                 — confirm, refute or supersede by hand.",
                outcome.scanned,
                outcome.candidates.len()
            );
            for c in &outcome.candidates {
                let seen_before = match c.previously_audited_at {
                    Some(ts) => format!(", last audited {}", audit_date(ts)),
                    None => String::new(),
                };
                println!("\n  {} [{}]{seen_before}", c.id, c.entry_type);
                println!("    {}", c.title);
                for signal in &c.signals {
                    println!("    - {}", audit_signal_line(signal));
                }
            }
            print_audit_gaps(outcome);
            if dry_run {
                println!("\n(dry run: nothing was recorded as audited)");
            }
        }
    }
}

/// Report everything the audit could not judge: entries whose only signal is a
/// reference git could not check, and a whole pass that never ran.
///
/// One function for both, called once per rendering path, because they answer
/// the same question — what is missing from the number above? An entry held
/// back for a check failure is kept out of the "worth re-reading" count: that
/// is a fact about git, not about the entry, and a broken repository must not
/// inflate the number an operator reads. Printed on the empty path too —
/// "scanned, nothing found" is the wrong answer when the check never ran.
fn print_audit_gaps(outcome: &mdkb::core::memory_audit::AuditOutcome) {
    let text = audit_gaps_text(outcome);
    if !text.is_empty() {
        print!("{text}");
    }
}

/// The text of [`print_audit_gaps`], built rather than printed.
///
/// Separate so the wording can be asserted in a test: whether the audit
/// admits a gap must not depend on whether an embedding model happens to be
/// installed on the machine running the suite.
fn audit_gaps_text(outcome: &mdkb::core::memory_audit::AuditOutcome) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if !outcome.unchecked.is_empty() {
        let _ = writeln!(
            out,
            "\n{} entr{} carried a reference git could not check, so it was not judged.",
            outcome.unchecked.len(),
            if outcome.unchecked.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );
        for c in &outcome.unchecked {
            let _ = writeln!(out, "  {} [{}] {}", c.id, c.entry_type, c.title);
        }
    }
    if !outcome.near_duplicate_checked {
        let _ = writeln!(
            out,
            "\nNear-duplicate check skipped: no entry has an embedding yet (run `mdkb embed`)."
        );
    } else if !outcome.near_duplicate_coverage.complete() {
        // "It ran" is not the same as "it looked at everything", and the
        // difference is what the operator needs to decide whether a clean
        // result means anything.
        let _ = writeln!(
            out,
            "\nNear-duplicate check covered {} of {} entries; the rest have no embedding yet \
             (run `mdkb embed`).",
            outcome.near_duplicate_coverage.embedded, outcome.near_duplicate_coverage.scanned
        );
    }
    out
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
                    "No entries to prune (nothing expired, no lifecycle entry unread for {} days).",
                    days
                );
            } else if dry_run {
                println!(
                    "Would archive {} entries (expired, or lifecycle entries unread for {} days):",
                    pruned.len(),
                    days
                );
                for id in pruned {
                    println!("  - {}", id);
                }
            } else {
                println!(
                    "Archived {} entries (expired, or lifecycle entries unread for {} days):",
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

/// Apply the detected keys, unless the mode makes that pointless.
///
/// Under `auto` the keys are already extracted, so writing them into the
/// config would only freeze today's derivation into a file that then has to be
/// maintained by hand. Returns `None` when nothing was attempted.
fn apply_relations_if_useful(
    ctx: &mdkb::core::Context,
    mode: mdkb::config::RelationMode,
    found: &mdkb::core::graph::RelationDetection,
) -> mdkb::error::Result<Option<mdkb::core::graph::AppliedRelations>> {
    if mode == mdkb::config::RelationMode::Auto {
        return Ok(None);
    }
    let keys: Vec<String> = found.candidates.iter().map(|c| c.key.clone()).collect();
    mdkb::core::graph::apply_detected_relations(&ctx.config_path, &keys).map(Some)
}

fn format_graph_relations(
    found: &mdkb::core::graph::RelationDetection,
    mode: mdkb::config::RelationMode,
    applied: Option<&mdkb::core::graph::AppliedRelations>,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            let rows: Vec<serde_json::Value> = found
                .examined
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "key": c.key,
                        "hits": c.hits,
                        "total": c.total,
                        "score": c.score(),
                        "is_relation": c.is_relation(),
                    })
                })
                .collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "mode": format!("{mode:?}").to_lowercase(),
                    "documents": found.documents,
                    "identities": found.identities,
                    "keys": rows,
                    "applied": applied.map(|a| a.added.clone()),
                }))
                .unwrap()
            );
        }
        OutputFormat::Csv => {
            println!("key,hits,total,score,is_relation");
            for c in &found.examined {
                println!(
                    "{},{},{},{:.3},{}",
                    c.key,
                    c.hits,
                    c.total,
                    c.score(),
                    c.is_relation()
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            if found.examined.is_empty() {
                println!("No frontmatter keys carry document references.");
            } else {
                println!("| Key | Hits | Total | Score | Relation |");
                println!("|-----|------|-------|-------|----------|");
                for c in &found.examined {
                    println!(
                        "| {} | {} | {} | {:.2} | {} |",
                        c.key,
                        c.hits,
                        c.total,
                        c.score(),
                        if c.is_relation() { "yes" } else { "no" }
                    );
                }
            }
            println!();
            println!(
                "mode: {}  documents: {}  declared identities: {}",
                format!("{mode:?}").to_lowercase(),
                found.documents,
                found.identities
            );
            // An empty result has two causes and they call for opposite
            // actions, so the command says which one it is rather than
            // leaving the reader to assume the corpus has no relations.
            if found.candidates.is_empty() && found.has_no_identity_space() {
                println!(
                    "No document declares an `id:` or `aliases:`, so nothing can resolve by name \
                     and no key can be measured. This is a missing precondition, not a finding."
                );
            } else if found.candidates.is_empty() {
                println!("No key reaches the threshold: measured, not assumed.");
            }
            match applied {
                Some(a) if a.added.is_empty() => {
                    println!("--apply: nothing to add, config.toml untouched.")
                }
                Some(a) => println!(
                    "--apply: added {} to graph.frontmatter_relations.",
                    a.added.join(", ")
                ),
                None if mode == mdkb::config::RelationMode::Auto => {}
                None => {}
            }
            if mode == mdkb::config::RelationMode::Auto {
                println!(
                    "relations = \"auto\": these keys are already extracted. `--apply` is a no-op \
                     and the config is never written."
                );
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

fn format_telemetry_status(status: &mdkb::core::ops::TelemetryStatus, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(status).unwrap_or_default()
            );
        }
        OutputFormat::Csv => {
            println!("enabled,retention_days,key_present,stored_events");
            println!(
                "{},{},{},{}",
                status.enabled, status.retention_days, status.key_present, status.stored_events
            );
        }
        OutputFormat::Markdown => {
            println!("| Enabled | Retention days | Private key | Stored events |");
            println!("|---|---:|---|---:|");
            println!(
                "| {} | {} | {} | {} |",
                status.enabled,
                status.retention_days,
                if status.key_present {
                    "present"
                } else {
                    "missing"
                },
                status.stored_events
            );
        }
        OutputFormat::Text => {
            println!(
                "Developer telemetry: {}",
                if status.enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!("Retention:          {} days", status.retention_days);
            println!(
                "Private HMAC key:    {}",
                if status.key_present {
                    "present"
                } else {
                    "missing"
                }
            );
            println!("Stored events:      {}", status.stored_events);
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

/// Render semantic search hits. The similarity score is the reason a hit is in
/// the list at all, so it travels with the symbol rather than being dropped.
/// Run the duplication audit for either surface that asks for it.
///
/// `mdkb dup` and `mdkb search --scope duplicates` are two ways of asking the
/// same question; they load the config the same way and call the same handler,
/// so they cannot answer differently.
/// The CLI's only duplication entry point — `mdkb dup` and
/// `mdkb search --scope duplicates` both come through here.
///
/// That is why the priority drop lives in this function rather than in one of
/// the two match arms: both are the CLI, both are the same minutes-long sweep,
/// and putting it in one arm would leave the other running at full priority for
/// no reason a user could explain. It is emphatically *not* in `core::dup`,
/// which the MCP surface shares — the daemon answers interactive searches from
/// a long-lived process, and backgrounding that would make every search pay for
/// an audit nobody asked it to run.
/// Tell the user, on stderr, that the config they wrote is being ignored.
///
/// `Config::load_or_default` warns through `tracing`, which reaches a log file
/// and not the person who just edited `config.toml` and is now reading a store
/// that behaves as if the file were empty. `update` and `stats` are the two
/// commands that user runs to find out what the store is doing, so they are the
/// two that say it out loud. stderr rather than stdout, and not a field on the
/// result: it is a diagnostic, and it has to appear under `--format json` too
/// without changing a shape the dashboard plugin parses.
fn report_unreadable_config(config_path: &std::path::Path) {
    let (_, problem) = mdkb::config::Config::load_or_report(config_path);
    if let Some(problem) = problem {
        eprintln!("mdkb: config.toml unreadable — using built-in defaults");
        eprintln!("  {problem}");
    }
}

fn run_dup(
    root: &std::path::Path,
    memory: Option<&rusqlite::Connection>,
    config_path: &std::path::Path,
    overrides: &mdkb::core::dup::DupOverrides,
) -> mdkb::error::Result<mdkb::core::dup::DupReport> {
    let config = mdkb::config::Config::load_or_default(config_path);
    // Only the semantic pass is worth backgrounding: the structural sweep is
    // seconds, and a user waiting on seconds should not have them throttled.
    if mdkb::core::dup::semantic_requested(&config.code.duplication, overrides) {
        mdkb::cli::priority::lower_to_background();
    }
    mdkb::core::dup::handle_dup(root, memory, &config, overrides)
}

fn format_scored_symbols(scored: &[(mdkb::code::symbol::Symbol, f32)], format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            let output: Vec<_> = scored
                .iter()
                .map(|(s, score)| serde_json::json!({ "symbol": s, "similarity": score }))
                .collect();
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("id,name,kind,file,line,similarity");
            for (s, score) in scored {
                println!(
                    "{},{},{},{},{},{:.3}",
                    s.id.value(),
                    s.name,
                    s.kind,
                    s.file_path,
                    s.range.start_line,
                    score,
                );
            }
        }
        OutputFormat::Markdown => {
            println!("| ID | Name | Kind | File | Line | Similarity |");
            println!("|----|------|------|------|------|------------|");
            for (s, score) in scored {
                println!(
                    "| {} | `{}` | {} | {} | {} | {:.3} |",
                    s.id.value(),
                    s.name,
                    s.kind,
                    s.file_path,
                    s.range.start_line,
                    score,
                );
            }
        }
        OutputFormat::Text => {
            if scored.is_empty() {
                println!("No semantic matches found.");
                return;
            }
            for (s, score) in scored {
                println!(
                    "  sym#{} {} {} in {}:{}  similarity: {:.3}",
                    s.id.value(),
                    s.kind,
                    s.name,
                    s.file_path,
                    s.range.start_line,
                    score,
                );
            }
        }
    }
}

/// Report a truncated `code find` result. It goes to stderr so that JSON and
/// CSV output on stdout stays parseable.
fn report_find_truncation(found: &mdkb::cli::handlers::CodeFindResult) {
    if found.total > found.symbols.len() {
        eprintln!(
            "Showing {} of {} matches. Narrow with --kind/--file, or raise --limit.",
            found.symbols.len(),
            found.total,
        );
    }
}

/// The duplication audit in the format the caller asked for.
///
/// A repository with no index prints its prose in every format, deliberately.
/// `{"clusters": 0, "findings": []}` is indistinguishable from "nothing is
/// duplicated here", which is the one answer an unindexed repository must not
/// be able to give — the same reason `handle_dup` reports it instead of
/// returning an empty list.
fn render_dup(report: &mdkb::core::dup::DupReport, format: OutputFormat) -> String {
    use mdkb::code::duplication::report::{render_csv, render_json};
    if !report.indexed {
        return report.markdown.clone();
    }
    match format {
        OutputFormat::Json => render_json(&report.findings, report.hamming_threshold),
        OutputFormat::Csv => render_csv(&report.findings),
        // The prose report is markdown already, so `text` and `markdown` are
        // one surface rather than two that could drift apart.
        OutputFormat::Text | OutputFormat::Markdown => report.markdown.clone(),
    }
}

/// The hidden-coupling audit in the format the caller asked for. Same
/// unindexed rule as [`render_dup`].
fn render_coupling(report: &mdkb::core::coupling::CouplingReport, format: OutputFormat) -> String {
    use mdkb::core::coupling::{render_csv, render_json};
    if !report.indexed {
        return report.markdown.clone();
    }
    match format {
        OutputFormat::Json => render_json(&report.findings),
        OutputFormat::Csv => render_csv(&report.findings),
        OutputFormat::Text | OutputFormat::Markdown => report.markdown.clone(),
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

fn format_code_calls(
    source: &mdkb::code::symbol::Symbol,
    related: &[mdkb::core::code::ResolvedCall],
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "source": source,
                "relationship": "Calls",
                "targets": related,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("source,relationship,target,target_kind,file,line,tier,is_unique");
            for call in related {
                let t = &call.symbol;
                println!(
                    "{},Calls,{},{},{},{},{},{}",
                    source.name,
                    t.name,
                    t.kind,
                    t.file_path,
                    t.range.start_line,
                    call.tier,
                    call.is_unique,
                );
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            println!(
                "Calls for {} {} ({}:{})",
                source.kind, source.name, source.file_path, source.range.start_line,
            );
            if related.is_empty() {
                println!("  (none)");
            } else {
                for call in related {
                    let t = &call.symbol;
                    println!(
                        "  - {} {} ({}:{}) [tier {}, {}]",
                        t.kind,
                        t.name,
                        t.file_path,
                        t.range.start_line,
                        call.tier,
                        if call.is_unique {
                            "unique"
                        } else {
                            "candidate list"
                        },
                    );
                }
            }
        }
    }
}

/// Report the calls that resolved to nothing in this index, so an empty callee
/// list is never read as "this calls nothing".
///
/// Silent on the structured formats: `format_code_graph` has already printed a
/// complete JSON or CSV document, and appending to it would produce output no
/// parser accepts.
fn format_unplaced_calls(unplaced: &mdkb::core::code::UnplacedCalls, format: OutputFormat) {
    if !matches!(format, OutputFormat::Markdown | OutputFormat::Text) {
        return;
    }
    if !unplaced.external.is_empty() {
        println!(
            "  {} target(s) outside this index: {}",
            unplaced.external.len(),
            unplaced.external.join(", "),
        );
    }
    if !unplaced.unknown.is_empty() {
        println!(
            "  {} call(s) on a receiver whose type is not indexed: {}",
            unplaced.unknown.len(),
            unplaced.unknown.join(", "),
        );
    }
}

/// Report how many entries arrived through a call no rule could place, so a
/// long list is never read as more certain than it is.
///
/// Silent on the structured formats for the same reason as
/// [`format_unplaced_calls`].
fn format_unplaced_arrivals(
    unplaced: usize,
    target: &mdkb::code::symbol::Symbol,
    format: OutputFormat,
) {
    if unplaced == 0 || !matches!(format, OutputFormat::Markdown | OutputFormat::Text) {
        return;
    }
    println!(
        "  {unplaced} of these arrived through an unqualified call and may belong to another `{}`",
        target.name,
    );
}

/// Print an impact radius as two lists and a coverage note.
///
/// One list would merge two different answers: what a placed call reaches, and
/// what merely shares a name with the target. The walk does not continue
/// through the second kind, so the radius is short by their subtrees — and a
/// list that does not say so reads as exhaustive.
///
/// One JSON document, not two: `mdkb code impact --format json | jq` has to
/// work, which it cannot if the two lists are printed as separate documents.
fn format_code_impact(
    source: &mdkb::code::symbol::Symbol,
    report: &mdkb::core::code::ImpactReport,
    format: OutputFormat,
) {
    match format {
        OutputFormat::Json => {
            let output = serde_json::json!({
                "source": source,
                "relationship": "Impact radius",
                "targets": report.impacted,
                "ambiguous": report.ambiguous,
                "stopped_arrivals": report.stopped_arrivals,
            });
            println!("{}", serde_json::to_string_pretty(&output).unwrap());
        }
        OutputFormat::Csv => {
            println!("source,relationship,target,target_kind,file,line");
            for (label, group) in [
                ("Impact radius", &report.impacted),
                ("Ambiguous frontier", &report.ambiguous),
            ] {
                for t in group {
                    println!(
                        "{},{},{},{},{},{}",
                        source.name, label, t.name, t.kind, t.file_path, t.range.start_line,
                    );
                }
            }
        }
        OutputFormat::Markdown | OutputFormat::Text => {
            format_code_graph("Impact radius", source, &report.impacted, format);
            if report.ambiguous.is_empty() {
                return;
            }
            println!(
                "Ambiguous frontier for {} — reached by an unqualified call, so each may \
                 belong to another `{}`:",
                source.name, source.name,
            );
            for t in &report.ambiguous {
                println!(
                    "  - {} {} ({}:{})",
                    t.kind, t.name, t.file_path, t.range.start_line,
                );
            }
            if report.stopped_arrivals > 0 {
                println!(
                    "  the walk stopped at {} of these, so the radius is short by whatever \
                     they call",
                    report.stopped_arrivals,
                );
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

    // The daemon is the sole writer of the DEFAULT store for every process on
    // the machine. Started from a test environment it would inherit the
    // namespace and redirect everyone's writes into it.
    if let Some(name) = mdkb::store::namespace::active()? {
        return Err(mdkb::Error::other(format!(
            "refusing to start the daemon in namespace '{name}': the daemon serves the default store only"
        )));
    }

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
        background: None,
    });

    let shutdown = CancellationToken::new();
    let ipc_shutdown = shutdown.clone();
    let ipc_base = base_dir.clone();
    let ipc_registry = Arc::clone(&registry);
    let ipc_dctx = Arc::clone(&dctx);
    let ipc_task = tokio::spawn(async move {
        // A failure to bind has to wake the shutdown select below. Without the
        // cancel the daemon keeps the singleton lock and waits for a signal it
        // will never get, serving nothing: clients then see a connect timeout
        // and the real error is never reported by anyone.
        let result =
            ipc_server::serve(&ipc_base, ipc_shutdown.clone(), ipc_registry, ipc_dctx).await;
        if result.is_err() {
            ipc_shutdown.cancel();
        }
        result
    });

    // Retire when the executable this process was launched from is replaced.
    //
    // The daemon outlives its own binary: measured on one machine, a daemon up
    // for two days while `target/release/mdkb` was rebuilt underneath it, with
    // two schema versions landing in between. Every one-shot CLI call then ran
    // newer code against the same store. Standing down is enough — the next call
    // spawns a daemon from the new binary — and it must be a graceful shutdown,
    // not an exit, so in-flight connections drain and the singleton lock is
    // released for the successor.
    let exe_watch = mdkb::daemon::ExeIdentity::of_current();
    let exe_shutdown = shutdown.clone();
    if let Some(launched_as) = exe_watch {
        tokio::spawn(async move {
            let mut ticks = tokio::time::interval(std::time::Duration::from_secs(30));
            ticks.tick().await; // fires immediately; skip it
            loop {
                ticks.tick().await;
                if launched_as.changed() {
                    tracing::warn!(
                        exe = %launched_as.path().display(),
                        "the executable this daemon was launched from has been replaced — \
                         standing down so the next call spawns a matching build"
                    );
                    exe_shutdown.cancel();
                    return;
                }
            }
        });
    }

    // Wait for SIGINT or SIGTERM, or for the self-retirement above.
    tokio::select! {
        result = mdkb::mcp::wait_for_shutdown_signal() => {
            if let Err(e) = result {
                tracing::warn!("signal: {e}");
            }
            tracing::info!("mdkb daemon received shutdown signal");
        }
        () = shutdown.cancelled() => {
            tracing::info!("mdkb daemon retiring: its executable changed on disk");
        }
    }

    shutdown.cancel();

    // Draining a long mutation can take minutes, and tokio never unregisters a
    // signal handler — the default "terminate now" disposition does not come
    // back after the first signal, so a second one has to be handled here or it
    // goes nowhere and SIGKILL is the operator's only remaining option.
    //
    // It has to end the process, not merely stop the wait: `Runtime::drop` waits
    // for `spawn_blocking` work that already started, which is the very write
    // being interrupted. SQLite's WAL makes that safe — an interrupted writer is
    // recovered on the next open, exactly as after a SIGKILL — so the honest
    // action is to unlink what shutdown would have unlinked and exit.
    let exit_base = base_dir.clone();
    tokio::spawn(async move {
        // A second call installs a fresh listener that still sees the next
        // SIGINT/SIGTERM — see `wait_for_shutdown_signal`'s doc comment.
        let signum = mdkb::mcp::wait_for_shutdown_signal().await.unwrap_or(15);
        tracing::warn!("mdkb daemon received a second signal; exiting without finishing the write");
        ipc_server::unlink_sockets(&exit_base);
        std::process::exit(128 + signum);
    });

    let served = match ipc_task.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(mdkb::Error::other(format!("ipc server: {e}"))),
        Err(e) => Err(mdkb::Error::other(format!("ipc server join: {e}"))),
    };

    drop(guard);
    served
}

/// True when stdin is not attached to a terminal. Used by the `mdkb serve`
/// back-compat shim to detect the "legacy mcp.json" launch pattern without
/// surprising an interactive operator running `mdkb serve` in their shell.
fn stdin_is_not_tty() -> bool {
    use std::io::IsTerminal;
    !std::io::stdin().is_terminal()
}

/// What `mdkb collection update` reports, for the direct and the daemon-routed
/// path alike.
///
/// The new path and pattern are echoed because the command takes either flag on
/// its own: printing only what was passed would leave the operator guessing
/// what the collection now covers. The reminder to re-run `update` is the one
/// non-obvious step — the row changed, the index has not.
fn collection_updated_line(name: &str, path: &str, pattern: &str) -> String {
    format!("Updated collection '{name}' -> {path}, {pattern} (run `mdkb update` to reconcile)")
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

#[cfg(test)]
mod tests {
    use super::audit_date;
    use super::is_server_invocation;
    use super::{McpRunMode, resolve_mcp_run_mode};

    use super::audit_gaps_text;
    use mdkb::core::memory_audit::{AuditCandidate, AuditOutcome};

    fn outcome(unchecked: Vec<AuditCandidate>, near_duplicate_checked: bool) -> AuditOutcome {
        AuditOutcome {
            scanned: 3,
            audited_at: 1_758_326_400,
            candidates: Vec::new(),
            unchecked,
            near_duplicate_checked,
            near_duplicate_coverage: mdkb::store::memory_audit::NearDuplicateCoverage {
                embedded: if near_duplicate_checked { 3 } else { 0 },
                scanned: 3,
            },
            source_drift_checked: true,
        }
    }

    fn candidate(id: &str) -> AuditCandidate {
        AuditCandidate {
            id: id.to_string(),
            title: "A title".to_string(),
            entry_type: "decision".to_string(),
            updated_at: 1_758_326_400,
            previously_audited_at: None,
            signals: Vec::new(),
        }
    }

    #[test]
    fn a_pass_that_never_ran_says_so_instead_of_reporting_nothing() {
        let text = audit_gaps_text(&outcome(Vec::new(), false));
        assert!(
            text.contains("Near-duplicate check skipped"),
            "a skipped pass must be named, not folded into 'found nothing': {text:?}"
        );
        assert!(
            text.contains("mdkb embed"),
            "the operator needs the command that fixes it: {text:?}"
        );
    }

    #[test]
    fn an_audit_that_ran_every_pass_and_judged_every_entry_admits_no_gap() {
        assert_eq!(
            audit_gaps_text(&outcome(Vec::new(), true)),
            "",
            "with nothing missing there is nothing to confess"
        );
    }

    #[test]
    fn both_gaps_are_reported_together_not_one_instead_of_the_other() {
        let text = audit_gaps_text(&outcome(vec![candidate("stale-ref")], false));
        assert!(text.contains("stale-ref"), "{text:?}");
        assert!(text.contains("git could not check"), "{text:?}");
        assert!(text.contains("Near-duplicate check skipped"), "{text:?}");
    }

    #[test]
    fn every_audit_date_is_the_day_it_falls_on() {
        assert_eq!(audit_date(1_758_326_400), "2025-09-20");
    }

    #[test]
    fn a_timestamp_no_calendar_holds_is_printed_raw_not_dropped() {
        // Both audit surfaces share this fallback: the "last audited" note used
        // to swallow such a timestamp and print nothing, which reads as "never
        // audited". A number the reader can question is the honest answer.
        assert_eq!(audit_date(i64::MAX), i64::MAX.to_string());
    }

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

    // Truth table for `resolve_mcp_run_mode(no_daemon, daemon_supported,
    // socket)`. Two booleans and an Option, so eight rows; the tests below
    // cover all eight between them:
    //
    //   no_daemon  daemon_supported  socket   result
    //   false      true              None     DaemonProxy (config default)
    //   false      true              Some(p)  DaemonProxy carrying p
    //   true       true              None     InProcess
    //   true       false             None     InProcess
    //   true       true              Some(p)  Err (--socket vs env)
    //   true       false             Some(p)  Err (--socket vs env)
    //   false      false             None     InProcess
    //   false      false             Some(p)  Err (--socket vs platform)
    //
    // The daemon is unix-only, but this rule is a pure function, so Linux CI
    // proves the Windows rows too.

    #[test]
    fn mcp_unix_default_uses_daemon_proxy() {
        // Proves: the control case is unchanged. The proxy runs when no
        // --socket is typed, and `None` tells the `Command::Mcp` arm to fall
        // back to `socket_path` in ~/.mdkb/daemon.toml and then to
        // ~/.mdkb/daemon.sock.
        assert_eq!(
            resolve_mcp_run_mode(false, true, None).unwrap(),
            McpRunMode::DaemonProxy {
                socket_override: None
            },
            "unix default must proxy with the config-default socket"
        );
    }

    #[test]
    fn mcp_explicit_socket_overrides_default_when_daemon_runs() {
        // Proves: `mdkb mcp --socket /path` on a daemon-capable platform
        // still proxies, and the typed path survives resolution intact.
        // Before the path was threaded through the mode, only a bool came
        // back, so a refactor could drop the path and no test would notice.
        //
        // Scope: this pins the passthrough only. Whether the proxy then
        // CONNECTS to that path instead of the config default is decided by
        // the `unwrap_or_else` chain in the `Command::Mcp` arm, which no
        // test reaches yet.
        let typed = std::path::PathBuf::from("/tmp/override.sock");
        assert_eq!(
            resolve_mcp_run_mode(false, true, Some(typed.clone())).unwrap(),
            McpRunMode::DaemonProxy {
                socket_override: Some(typed)
            },
            "the resolver must return the typed --socket path unchanged"
        );
    }

    #[test]
    fn mcp_no_daemon_env_forces_in_process_everywhere() {
        // Proves: MDKB_NO_DAEMON alone still selects the in-process server,
        // on a daemon-capable platform and on one without.
        for daemon_supported in [true, false] {
            assert_eq!(
                resolve_mcp_run_mode(true, daemon_supported, None).unwrap(),
                McpRunMode::InProcess,
                "MDKB_NO_DAEMON must win (daemon_supported={daemon_supported})"
            );
        }
    }

    #[test]
    fn mcp_no_daemon_env_refuses_explicit_socket() {
        // Proves: the first of the two review notes on PR #2. Before this,
        // `MDKB_NO_DAEMON=1 mdkb mcp --socket /path` dropped the flag without
        // a word on Unix, which reads as "the daemon is serving you" when the
        // in-process server is. It is now an error on both platforms, so one
        // rule covers both routes into in-process mode.
        //
        // The assertion is on the message, not just the Err: it names
        // BOTH halves of the conflict (the flag and the variable) or the user
        // cannot tell which side to cut.
        for daemon_supported in [true, false] {
            let socket = Some(std::path::PathBuf::from("/tmp/refused.sock"));
            let err = resolve_mcp_run_mode(true, daemon_supported, socket).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("--socket") && msg.contains("MDKB_NO_DAEMON"),
                "error must name the flag and the variable that conflict: {msg}"
            );
        }
    }

    #[test]
    fn mcp_without_daemon_support_falls_back_to_in_process() {
        // Proves: the original PR #2 fix still holds. On Windows, `mdkb mcp`
        // — the command `mdkb setup mcp` writes into the client config. It used
        // to exit with an error, which MCP clients surface as an opaque
        // CONNECTION_CLOSED at session start. Without --socket it must fall
        // back to serving in-process, silently and successfully.
        assert_eq!(
            resolve_mcp_run_mode(false, false, None).unwrap(),
            McpRunMode::InProcess
        );
    }

    #[test]
    fn mcp_without_daemon_support_refuses_explicit_socket() {
        // Proves: the platform half of the same rule. --socket is an explicit
        // ask for the daemon proxy, so falling back to in-process would ignore
        // a flag the user typed and hide a misconfiguration. The error must
        // name the flag, or the message does not tell the user what to remove.
        let socket = Some(std::path::PathBuf::from("/tmp/refused.sock"));
        let err = resolve_mcp_run_mode(false, false, socket).unwrap_err();
        assert!(
            err.to_string().contains("--socket"),
            "error must name the flag that selected the proxy: {err}"
        );
    }

    /// Burn `frames` stack frames of 64 KiB each, defeating optimization so the
    /// frames are really allocated. Returns the depth reached.
    // The large stack array is the measurement, not an oversight: moving it to
    // the heap would leave the frames empty and the test would prove nothing.
    #[allow(clippy::large_stack_arrays)]
    fn burn_stack(frames: usize) -> usize {
        let mut block = [0u8; 64 * 1024];
        std::hint::black_box(&mut block);
        if frames == 0 {
            0
        } else {
            1 + burn_stack(frames - 1)
        }
    }

    #[test]
    fn the_work_thread_has_more_stack_than_any_platform_default_guarantees() {
        // Proves: `run_on_sized_stack` really hands the work a stack of its own
        // size, not whichever one the thread it was called from happened to
        // have. 4 MiB is over Windows' 1 MiB main-thread default and over the
        // 2 MiB Rust gives a spawned thread — including the harness thread this
        // test runs on — so the same recursion outside the helper would abort.
        //
        // Issue #6: debug builds of `mdkb schema` died at 0xC00000FD on
        // Windows because the platform, not this program, sized the stack the
        // clap builder ran on. Deleting the helper makes this test abort on
        // every platform, which is the point of measuring it here rather than
        // trusting one host's default.
        let depth = crate::run_on_sized_stack(|| Ok(burn_stack(64))).unwrap();
        assert_eq!(depth, 64, "the whole 4 MiB recursion must complete");
    }
}
