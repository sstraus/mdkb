//! What is wrong with this mdkb installation, and the fix for each problem.
//!
//! The checks are pure functions over [`Facts`]; collecting the facts is the
//! caller's job (`cli::doctor`), so SessionStart can collect only the cheap
//! ones and `mdkb doctor --full` everything. Story 186-446e.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Something that should work does not.
    Error,
    /// Something works worse than it should, or will stop working.
    Warning,
    /// A setting whose effect is easy to misread. Never shown at SessionStart.
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub id: &'static str,
    pub severity: Severity,
    pub message: String,
    /// The command or edit that fixes it, when the next step is not obvious.
    pub fix: Option<String>,
}

/// What a quarantined index left behind.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Quarantine {
    pub date: String,
    pub memory_entries_salvaged: u64,
    pub file: String,
}

/// Everything the checks read. `None` means "not collected".
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub hooks_missing: Vec<String>,
    pub hooks_duplicated: Vec<String>,
    /// Why `.mdkb/config.toml` does not load.
    pub config_error: Option<String>,
    pub quarantine: Vec<Quarantine>,
    pub doc_count: i64,
    /// `(entry files, active rows)` of the memory projection.
    pub projection: Option<(usize, usize)>,
    pub require_sigil: bool,
    pub shadow_enabled: bool,
    /// `user_prompt_submit` calls in the last 7 days.
    pub prompts_7d: u32,
    /// Recall ledger prompts in the last 7 days.
    pub ledger_prompts_7d: u32,
    pub mining_enabled: bool,
    pub distiller_program: Option<String>,
    /// The live distiller probe's error, when it ran and failed (`--full`).
    pub distiller_failure: Option<String>,
}

/// Prompts a week must see before a silent shadow counts as broken.
pub const SHADOW_SILENT_MIN_PROMPTS: u32 = 20;

/// Every finding, most severe first.
pub fn findings(facts: &Facts) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut push = |id, severity, message: String, fix: Option<&str>| {
        out.push(Finding {
            id,
            severity,
            message,
            fix: fix.map(str::to_string),
        });
    };

    if !facts.hooks_missing.is_empty() {
        push(
            "hooks.drift",
            Severity::Error,
            format!(
                "hook events not registered, so they never fire: {}",
                facts.hooks_missing.join(", ")
            ),
            Some("mdkb setup hooks claude"),
        );
    }
    if !facts.hooks_duplicated.is_empty() {
        push(
            "hooks.drift",
            Severity::Error,
            format!(
                "hook events registered twice, so they fire twice per turn: {}",
                facts.hooks_duplicated.join(", ")
            ),
            Some("mdkb setup hooks claude"),
        );
    }
    if let Some(error) = &facts.config_error {
        push(
            "config.invalid",
            Severity::Error,
            format!(
                ".mdkb/config.toml does not load ({error}); the last config that did is still in use"
            ),
            None,
        );
    }
    if let Some(failure) = &facts.distiller_failure {
        push(
            "priors.distiller_failed",
            Severity::Error,
            format!("the prior distiller ran and failed: {failure}"),
            Some("mdkb setup check"),
        );
    }
    for q in &facts.quarantine {
        push(
            "index.quarantine",
            Severity::Warning,
            format!(
                "the index was corrupt and rebuilt on {}; {} memory entries were salvaged from .mdkb/{}",
                q.date, q.memory_entries_salvaged, q.file
            ),
            (facts.doc_count == 0).then_some("mdkb update"),
        );
    }
    if let Some((files, rows)) = facts.projection
        && files != rows
    {
        push(
            "memory.projection_drift",
            Severity::Warning,
            format!("{files} memory entry file(s) on disk but {rows} active row(s) in the index"),
            Some("mdkb memory sync"),
        );
    }
    if facts.shadow_enabled
        && facts.prompts_7d >= SHADOW_SILENT_MIN_PROMPTS
        && facts.ledger_prompts_7d == 0
    {
        push(
            "recall.shadow_silent",
            Severity::Warning,
            format!(
                "shadow recall is on but recorded nothing from {} prompts in 7 days; the running daemon may predate this build",
                facts.prompts_7d
            ),
            Some("mdkb daemon restart"),
        );
    }
    if facts.mining_enabled && facts.distiller_program.is_none() {
        push(
            "priors.distiller_unset",
            Severity::Warning,
            "prior mining is on but no distiller_program is set, so nothing is mined".to_string(),
            Some("set [priors] distiller_program in ~/.mdkb/daemon.toml"),
        );
    }
    if facts.require_sigil {
        push(
            "recall.sigil_only",
            Severity::Info,
            "memory recall runs only on prompts that start with `*`".to_string(),
            Some("[hooks] user_prompt_submit_require_sigil = false"),
        );
    }

    out.sort_by_key(|f| f.severity);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(f: &Facts) -> Vec<&'static str> {
        findings(f).into_iter().map(|x| x.id).collect()
    }

    fn one(f: &Facts, id: &str) -> Finding {
        findings(f)
            .into_iter()
            .find(|x| x.id == id)
            .unwrap_or_else(|| panic!("no {id} in {:?}", findings(f)))
    }

    #[test]
    fn a_healthy_installation_has_no_finding_worse_than_info() {
        let facts = Facts {
            projection: Some((3, 3)),
            require_sigil: false,
            ..Facts::default()
        };
        assert!(findings(&facts).is_empty(), "{:?}", findings(&facts));
    }

    #[test]
    fn a_missing_hook_is_an_error_with_the_registration_command() {
        let f = one(
            &Facts {
                hooks_missing: vec!["PostToolUseFailure".into()],
                ..Facts::default()
            },
            "hooks.drift",
        );
        assert_eq!(f.severity, Severity::Error);
        assert!(f.message.contains("PostToolUseFailure"), "{}", f.message);
        assert_eq!(f.fix.as_deref(), Some("mdkb setup hooks claude"));
    }

    #[test]
    fn an_unreadable_config_is_an_error_that_quotes_the_parser() {
        let f = one(
            &Facts {
                config_error: Some("expected `]` at line 1".into()),
                ..Facts::default()
            },
            "config.invalid",
        );
        assert_eq!(f.severity, Severity::Error);
        assert!(
            f.message.contains("expected `]` at line 1"),
            "{}",
            f.message
        );
    }

    #[test]
    fn a_quarantine_asks_for_an_update_only_while_docs_are_missing() {
        let q = Quarantine {
            date: "2026-09-20".into(),
            memory_entries_salvaged: 113,
            file: "index.sqlite.corrupt-1".into(),
        };
        let empty = one(
            &Facts {
                quarantine: vec![q.clone()],
                doc_count: 0,
                ..Facts::default()
            },
            "index.quarantine",
        );
        assert_eq!(empty.fix.as_deref(), Some("mdkb update"));
        let reindexed = one(
            &Facts {
                quarantine: vec![q],
                doc_count: 726,
                ..Facts::default()
            },
            "index.quarantine",
        );
        assert_eq!(reindexed.fix, None, "nothing to do once docs are back");
        assert!(reindexed.message.contains("113"), "{}", reindexed.message);
    }

    #[test]
    fn projection_drift_points_at_memory_sync() {
        let f = one(
            &Facts {
                projection: Some((10, 7)),
                ..Facts::default()
            },
            "memory.projection_drift",
        );
        assert_eq!(f.severity, Severity::Warning);
        assert!(
            f.message.contains("10") && f.message.contains('7'),
            "{}",
            f.message
        );
        assert_eq!(f.fix.as_deref(), Some("mdkb memory sync"));
    }

    /// The state this repo was in on 2026-09-29: shadow on (after the fix it
    /// would be served), prompts arriving, nothing recorded.
    #[test]
    fn shadow_that_records_nothing_for_a_busy_week_is_reported() {
        let busy = Facts {
            shadow_enabled: true,
            prompts_7d: SHADOW_SILENT_MIN_PROMPTS,
            ..Facts::default()
        };
        let f = one(&busy, "recall.shadow_silent");
        assert_eq!(f.fix.as_deref(), Some("mdkb daemon restart"));
        let quiet = Facts {
            prompts_7d: SHADOW_SILENT_MIN_PROMPTS - 1,
            ..busy.clone()
        };
        assert!(
            !ids(&quiet).contains(&"recall.shadow_silent"),
            "a quiet week proves nothing"
        );
        let recording = Facts {
            ledger_prompts_7d: 1,
            ..busy
        };
        assert!(!ids(&recording).contains(&"recall.shadow_silent"));
    }

    #[test]
    fn mining_without_a_distiller_is_a_warning() {
        let f = one(
            &Facts {
                mining_enabled: true,
                ..Facts::default()
            },
            "priors.distiller_unset",
        );
        assert_eq!(f.severity, Severity::Warning);
        let set = Facts {
            mining_enabled: true,
            distiller_program: Some("codex".into()),
            ..Facts::default()
        };
        assert!(!ids(&set).contains(&"priors.distiller_unset"));
    }

    #[test]
    fn a_failed_distiller_probe_is_an_error_quoting_its_output() {
        let f = one(
            &Facts {
                distiller_failure: Some("unexpected status 400".into()),
                ..Facts::default()
            },
            "priors.distiller_failed",
        );
        assert_eq!(f.severity, Severity::Error);
        assert!(f.message.contains("400"), "{}", f.message);
    }

    /// Boss read recall as automatic on 2026-09-29; the default says otherwise.
    #[test]
    fn a_required_sigil_is_explained_as_info() {
        let f = one(
            &Facts {
                require_sigil: true,
                ..Facts::default()
            },
            "recall.sigil_only",
        );
        assert_eq!(f.severity, Severity::Info);
        let shadowing = Facts {
            require_sigil: true,
            shadow_enabled: true,
            ..Facts::default()
        };
        assert_eq!(
            one(&shadowing, "recall.sigil_only").severity,
            Severity::Info
        );
    }

    #[test]
    fn findings_are_ordered_most_severe_first() {
        let facts = Facts {
            require_sigil: true,
            projection: Some((2, 1)),
            hooks_missing: vec!["Stop".into()],
            ..Facts::default()
        };
        let severities: Vec<Severity> = findings(&facts).iter().map(|f| f.severity).collect();
        let mut sorted = severities.clone();
        sorted.sort();
        assert_eq!(severities, sorted);
        assert_eq!(severities.first(), Some(&Severity::Error));
    }
}
