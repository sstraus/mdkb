//! `mdkb repos` subcommands: `refresh`.

use std::fmt::Write as _;
use std::path::PathBuf;

use crate::cli::RefreshFilter;
use crate::core::refresh::{RefreshReport, RefreshStatus, refresh_outdated};
use crate::daemon::config::DaemonConfig;
use crate::daemon::repo_map::{discover_nested_stores, read_known_roots};
use crate::error::{Error, Result};

/// The stores `root="*"` can read: the known roots and the stores nested under
/// them. Refreshing the same set is what makes the footer's outdated line go
/// away.
fn stores_in_scope() -> Vec<PathBuf> {
    let known = read_known_roots(&DaemonConfig::daemon_home().join("repos.json"));
    discover_nested_stores(&known).into_iter().collect()
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
    let reports = refresh_outdated(&stores_in_scope());
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
                    Some(v) if v >= crate::store::schema::SCHEMA_VERSION => {
                        format!(" — the store IS at schema v{v}: the migration committed")
                    }
                    Some(v) => format!(" — the store is at schema v{v}, as before"),
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
