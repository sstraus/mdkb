//! mdkb - Local markdown knowledge base CLI.

use tracing::Level;
use tracing_subscriber::EnvFilter;

use mdkb::cli::{Cli, Command, CollectionCommand};
use mdkb::Result;

fn main() -> Result<()> {
    let cli = Cli::parse_args();

    // Set up tracing based on verbosity
    let level = match cli.verbose {
        0 => Level::WARN,
        1 => Level::INFO,
        2 => Level::DEBUG,
        _ => Level::TRACE,
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive(level.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    tracing::debug!("mdkb starting with verbosity level {}", cli.verbose);

    match cli.command {
        Command::Init => {
            tracing::info!("Initializing mdkb...");
            println!("mdkb init: not yet implemented");
        }
        Command::Collection(cmd) => match cmd {
            CollectionCommand::Add { name, path, pattern } => {
                println!("collection add {name} {path} (pattern: {pattern}): not yet implemented");
            }
            CollectionCommand::Remove { name } => {
                println!("collection remove {name}: not yet implemented");
            }
            CollectionCommand::List => {
                println!("collection list: not yet implemented");
            }
            CollectionCommand::Rename { old_name, new_name } => {
                println!("collection rename {old_name} -> {new_name}: not yet implemented");
            }
        },
        Command::Search { query, limit, collection } => {
            println!("search '{query}' (limit: {limit}, collection: {collection:?}): not yet implemented");
        }
        Command::Get { id, lines } => {
            println!("get {id} (lines: {lines:?}): not yet implemented");
        }
        Command::Status => {
            println!("status: not yet implemented");
        }
        Command::Update => {
            println!("update: not yet implemented");
        }
        Command::Serve => {
            tracing::info!("Starting MCP server...");
            println!("serve: not yet implemented");
        }
    }

    Ok(())
}
