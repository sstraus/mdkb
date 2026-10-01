//! `mdkb doctor`: collect the facts [`crate::domain::doctor`] judges, and
//! render its findings for a person, a script or the model at SessionStart.

use std::path::Path;

use crate::core::Context;
use crate::domain::doctor::{Facts, Finding, Quarantine, Severity};

/// Collect every fact the checks read. `full` adds the live distiller probe,
/// which runs an external CLI and can take seconds; SessionStart never asks
/// for it. `ctx` is `None` when there is no store to read.
pub fn collect(root: &Path, ctx: Option<&Context>, full: bool) -> Facts {
    let config_path = root.join(".mdkb/config.toml");
    let (config, config_error) = crate::config::Config::load_or_report(&config_path);
    let priors = crate::config::effective_priors(&config_path);
    let drift = crate::cli::setup::detect_hook_drift_for_repo(root, None);
    let hook_fixes = crate::cli::setup::hook_fix_commands(root, None);
    let week_ago = chrono::Utc::now().timestamp() - 7 * 86_400;

    let mut facts = Facts {
        hooks_missing: drift.missing,
        hooks_duplicated: drift.duplicated,
        hooks_missing_fix: Some(hook_fixes.missing),
        hooks_duplicated_fix: Some(hook_fixes.duplicated),
        config_error,
        require_sigil: config.hooks.user_prompt_submit_require_sigil,
        shadow_enabled: config.hooks.user_prompt_submit_shadow,
        mining_enabled: priors.mining_enabled,
        distiller_program: priors.distiller_program.clone(),
        ..Facts::default()
    };

    let store_dir = ctx
        .and_then(|c| c.db_path.parent().map(Path::to_path_buf))
        .or_else(|| crate::store::namespace::store_dir(root).ok());
    if let Some(dir) = &store_dir {
        facts.quarantine = crate::store::heal::quarantine_reports(dir)
            .into_iter()
            .map(|r| Quarantine {
                date: chrono::DateTime::from_timestamp(r.quarantined_at, 0).map_or_else(
                    || "an unknown date".to_string(),
                    |d| d.format("%Y-%m-%d").to_string(),
                ),
                memory_entries_salvaged: r.memory_entries_salvaged as u64,
                file: r.corrupt_file,
            })
            .collect();
    }
    if let Some(ctx) = ctx {
        facts.doc_count = crate::store::search::get_status(&ctx.conn)
            .map_or(0, |s| i64::try_from(s.documents).unwrap_or(i64::MAX));
        facts.projection = crate::core::memory_sync::projection_file_and_row_counts(ctx).ok();
        facts.prompts_7d =
            crate::store::stats::count_calls_since(&ctx.conn, "user_prompt_submit", week_ago)
                .unwrap_or(0);
        facts.ledger_prompts_7d =
            crate::store::recall_ledger::prompts_since(&ctx.conn, week_ago).unwrap_or(0);
    }
    if full
        && let crate::cli::setup::DistillerCheck::Fail { program, detail } =
            crate::cli::setup::check_distiller(root)
    {
        facts.distiller_failure = Some(format!("{program}: {detail}"));
    }
    facts
}

/// One line per finding: `- [severity] id: message → fix: `cmd``.
pub fn render_line(f: &Finding) -> String {
    let severity = match f.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
    };
    match &f.fix {
        Some(fix) => format!("- [{severity}] {}: {} → fix: `{fix}`", f.id, f.message),
        None => format!("- [{severity}] {}: {}", f.id, f.message),
    }
}

/// The `mdkb doctor` text report.
pub fn render(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "mdkb doctor: no problems found\n".to_string();
    }
    let mut out = format!("mdkb doctor: {} finding(s)\n", findings.len());
    for f in findings {
        out.push_str(&render_line(f));
        out.push('\n');
    }
    out
}

/// The most findings SessionStart lists before pointing at `mdkb doctor`.
pub const SESSION_START_MAX_FINDINGS: usize = 5;

/// The SessionStart block: errors and warnings only, at most
/// [`SESSION_START_MAX_FINDINGS`] lines, and nothing at all when healthy.
/// `info` findings describe settings, not problems, and are paid for on every
/// session — they stay on the CLI.
pub fn session_block(findings: &[Finding]) -> Option<String> {
    let problems: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.severity != Severity::Info)
        .collect();
    if problems.is_empty() {
        return None;
    }
    let mut out = String::from("## mdkb doctor\n\n");
    for f in problems.iter().take(SESSION_START_MAX_FINDINGS) {
        out.push_str(&render_line(f));
        out.push('\n');
    }
    if let Some(more) = problems
        .len()
        .checked_sub(SESSION_START_MAX_FINDINGS)
        .filter(|n| *n > 0)
    {
        out.push_str(&format!("- … {more} more: run `mdkb doctor`\n"));
    }
    Some(out)
}

/// Whether the findings should fail a script: any error.
pub fn has_errors(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Error)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SessionStart pays for `collect(.., false)` on every session. It reads
    /// two settings files, a config file and a handful of indexed counts;
    /// anything slower has leaked a full check into the cheap path.
    #[test]
    fn cheap_checks_stay_under_budget() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        let ctx = Context::open(root).unwrap();
        collect(root, Some(&ctx), false); // first run pays for file-system caches
        let t0 = std::time::Instant::now();
        let facts = collect(root, Some(&ctx), false);
        let elapsed = t0.elapsed();
        assert!(facts.distiller_failure.is_none(), "the probe is full-only");
        assert!(
            elapsed < std::time::Duration::from_millis(20),
            "cheap doctor checks took {elapsed:?}"
        );
    }

    fn warning(id: &'static str) -> Finding {
        Finding {
            id,
            severity: Severity::Warning,
            message: format!("{id} is off"),
            fix: None,
        }
    }

    #[test]
    fn a_healthy_or_info_only_store_adds_nothing_to_session_start() {
        assert_eq!(session_block(&[]), None);
        let info = Finding {
            severity: Severity::Info,
            ..warning("recall.sigil_only")
        };
        assert_eq!(
            session_block(&[info]),
            None,
            "info is charged on every session"
        );
    }

    #[test]
    fn session_start_lists_problems_one_line_each() {
        let block = session_block(&[warning("index.quarantine")]).unwrap();
        assert_eq!(
            block,
            "## mdkb doctor\n\n- [warning] index.quarantine: index.quarantine is off\n"
        );
    }

    #[test]
    fn session_start_caps_the_list_and_points_at_the_command() {
        let many: Vec<Finding> = ["a", "b", "c", "d", "e", "f", "g"]
            .into_iter()
            .map(warning)
            .collect();
        let block = session_block(&many).unwrap();
        let findings = block.lines().filter(|l| l.starts_with("- [")).count();
        assert_eq!(findings, SESSION_START_MAX_FINDINGS);
        assert!(block.contains("2 more: run `mdkb doctor`"), "{block}");
    }

    #[test]
    fn a_line_carries_the_fix_only_when_there_is_one() {
        let with = Finding {
            id: "hooks.drift",
            severity: Severity::Error,
            message: "Stop is not registered".into(),
            fix: Some("mdkb setup hooks claude".into()),
        };
        assert_eq!(
            render_line(&with),
            "- [error] hooks.drift: Stop is not registered → fix: `mdkb setup hooks claude`"
        );
        let without = Finding { fix: None, ..with };
        assert_eq!(
            render_line(&without),
            "- [error] hooks.drift: Stop is not registered"
        );
    }
}
