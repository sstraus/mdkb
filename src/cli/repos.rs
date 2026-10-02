//! `mdkb repos` subcommands: `list`, `refresh`.

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::cli::{OutputFormat, RefreshFilter};
use crate::core::refresh::{RefreshReport, RefreshStatus, refresh_outdated};
use crate::daemon::config::DaemonConfig;
use crate::daemon::repo_listing::{list_repos, render_text};
use crate::daemon::repo_map::{discover_nested_stores, read_known_roots};
use crate::daemon::scope::ScopePolicy;
use crate::error::{Error, Result};
use crate::mcp::tools::{RootSelector, ScopedRoots};

/// The stores nested under `known`, minus what `daemon.toml` ignores.
fn nested_stores(known: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let home = DaemonConfig::daemon_home();
    let ignore = DaemonConfig::load_or_default(&home.join("daemon.toml"))?.ignored_paths();
    Ok(discover_nested_stores(known, &ignore).into_iter().collect())
}

/// The stores `root="*"` can read: the known roots and the stores nested under
/// them. Refreshing the same set is what makes the footer's outdated line go
/// away.
fn stores_in_scope() -> Result<Vec<PathBuf>> {
    nested_stores(&read_known_roots(
        &DaemonConfig::daemon_home().join("repos.json"),
    ))
}

/// The daemon's own list when one runs (the one source `daemon status` also
/// reads), otherwise the persisted map. Neither walks the disk, and a daemon
/// that is running but does not answer is an error, not a reason to guess.
async fn known_roots() -> Result<Vec<PathBuf>> {
    match crate::cli::daemon::running_daemon_repos().await {
        Ok(Some(roots)) => Ok(roots),
        Ok(None) => Ok(read_known_roots(
            &DaemonConfig::daemon_home().join("repos.json"),
        )),
        Err(e) => Err(Error::other(format!(
            "the running daemon did not list its repos: {e}"
        ))),
    }
}

/// The repos a `--root` value names: the MCP grammar through the MCP parser
/// and resolver, against the same map the daemon answers from. Nested stores
/// join the map only for the selectors that need them (`needs_discovery`), as
/// on the MCP side, so an absolute path costs no directory walk. The caller's
/// scope is the working directory's.
pub async fn resolve_roots(raw: &str) -> Result<ScopedRoots> {
    let selector = RootSelector::parse(Some(raw)).map_err(Error::other)?;
    let mut known = known_roots().await?;
    if selector.needs_discovery() {
        known.extend(nested_stores(&known)?);
        known.sort();
        known.dedup();
    }
    let config = DaemonConfig::load_or_default(&DaemonConfig::daemon_home().join("daemon.toml"))?;
    let caller: Vec<PathBuf> = std::env::current_dir()
        .map(|cwd| crate::domain::canonicalize_plain(&cwd).unwrap_or(cwd))
        .into_iter()
        .collect();
    selector
        .resolve_scoped(&known, &[], &ScopePolicy::load(&config), &caller)
        .map_err(Error::other)
}

pub async fn handle_list(format: OutputFormat) -> Result<()> {
    let rows = list_repos(&known_roots().await?);
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string_pretty(&rows)?),
        _ => print!("{}", render_text(&rows)),
    }
    Ok(())
}

pub fn handle_refresh(only: RefreshFilter) -> Result<()> {
    let RefreshFilter::Outdated = only;
    // The repo map lists default stores. Under a namespace every store path
    // resolves to `.mdkb/namespaces/<name>/`, so each default store would be
    // reported as missing: fail once, with the reason, instead of N times.
    if let Some(namespace) = crate::store::namespace::active()? {
        return Err(Error::other(format!(
            "refusing to refresh under MDKB_NAMESPACE={namespace}: the repo map lists default \
             stores, and a namespace redirects every store path. Unset MDKB_NAMESPACE."
        )));
    }
    let reports = refresh_outdated(&stores_in_scope()?);
    let (text, failed) = render_refresh(&reports);
    print!("{text}");
    if failed > 0 {
        return Err(Error::other(format!(
            "{failed} store(s) failed; the FAILED lines above say what state each is in"
        )));
    }
    Ok(())
}

/// One line per store that was touched or could not be, then a total. The
/// stores already current are counted, not listed: on a healthy machine they
/// are most of them.
fn render_refresh(reports: &[RefreshReport]) -> (String, usize) {
    let (mut migrated, mut current, mut newer, mut failed) = (0, 0, 0, 0);
    let mut out = String::new();
    for report in reports {
        let root = report.root.display();
        match &report.outcome {
            Ok(RefreshStatus::Current) => current += 1,
            Ok(RefreshStatus::Newer { found }) => {
                newer += 1;
                let _ = writeln!(
                    out,
                    "newer     {root}: schema v{found} is newer than this binary; left alone"
                );
            }
            Ok(RefreshStatus::Migrated(m)) => {
                migrated += 1;
                let _ = writeln!(
                    out,
                    "migrated  {root}: v{} -> v{}, memory entries {} -> {}, backup {}",
                    m.from,
                    crate::store::schema::SCHEMA_VERSION,
                    m.memory_before,
                    m.memory_after,
                    m.backup.display()
                );
            }
            Err(f) => {
                failed += 1;
                let backup = f
                    .backup
                    .as_ref()
                    .map(|b| format!(" (backup kept at {})", b.display()))
                    .unwrap_or_default();
                let state = match f.schema_after {
                    Some(v) => format!(" — the store is at schema v{v}"),
                    None => " — the store's schema could not be read afterwards".to_string(),
                };
                let _ = writeln!(out, "FAILED    {root}: {}{state}{backup}", f.reason);
            }
        }
    }
    let _ = writeln!(
        out,
        "{migrated} migrated, {current} already current, {newer} newer than this binary, {failed} failed"
    );
    if migrated > 0 {
        out.push_str(
            "Restart a running daemon so it serves the migrated stores: mdkb daemon restart\n",
        );
    }
    (out, failed)
}
