//! Phase-4 candidate detector: the cheap, no-LLM gate that decides whether an
//! [`Episode`] plausibly contains a reusable behavioral lesson worth spending an
//! LLM distillation call on. The overwhelming majority of episodes return
//! `None` here — that is the point: cut LLM calls by an order of magnitude vs.
//! distilling every session.
//!
//! Two signals (per the GPT-5.5 design):
//! - **ErrorFixed** — an error occurred, a corrective action followed, and the
//!   episode ended clean. The reusable lesson is "when <error>, do <fix>".
//! - **UserCorrection** — the user explicitly corrected course ("no, do X
//!   instead", "don't …", "remember …"). The user is the ground-truth signal.
//!
//! A clean run with no error and no correction teaches nothing; an unresolved
//! error has no fix to distill. Both return `None`.

use crate::domain::prior_episode::Episode;

/// Why an episode was flagged as a distillation candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateReason {
    /// Error → corrective action → clean ending.
    ErrorFixed,
    /// The user explicitly corrected the assistant.
    UserCorrection,
}

/// A flagged episode plus the raw material the Phase-5 distiller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSignal {
    pub reason: CandidateReason,
    pub error_tool: Option<String>,
    pub error_signature: Option<String>,
    /// Tool names that ran after the first error (the recovery attempt).
    pub corrective_tools: Vec<String>,
    /// The user message that signaled a correction, if any.
    pub correction_text: Option<String>,
}

/// Standalone words that signal an explicit user correction.
const CORRECTION_WORDS: &[&str] = &[
    "no", "nope", "wrong", "instead", "don't", "dont", "actually", "stop", "revert", "undo",
];
/// Multi-word correction phrases (substring match on lowercased text).
const CORRECTION_PHRASES: &[&str] = &["next time", "remember", "should have", "do not"];

/// Whether a user message reads as an explicit correction.
fn is_correction(text: &str) -> bool {
    let lower = text.to_lowercase();
    if CORRECTION_PHRASES.iter().any(|p| lower.contains(p)) {
        return true;
    }
    // Word-boundary match so "no" does not fire inside "know"/"note".
    lower
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .any(|w| CORRECTION_WORDS.contains(&w))
}

/// Detect whether an episode is a distillation candidate. `None` = skip (no LLM).
pub fn detect_candidate(ep: &Episode) -> Option<CandidateSignal> {
    let error_fixed = !ep.errors.is_empty() && ep.had_corrective_action && ep.ended_clean;
    let correction_text = ep.user_messages.iter().find(|m| is_correction(m)).cloned();

    if !error_fixed && correction_text.is_none() {
        return None;
    }

    // Corrective tools = tool names after the first error's tool_use.
    let corrective_tools = ep
        .errors
        .first()
        .and_then(|first_err| ep.tools.iter().position(|t| t.id == first_err.tool_use_id))
        .map(|idx| ep.tools[idx + 1..].iter().map(|t| t.name.clone()).collect())
        .unwrap_or_default();

    let (error_tool, error_signature) = ep
        .errors
        .first()
        .map(|e| (Some(e.tool.clone()), Some(e.signature.clone())))
        .unwrap_or((None, None));

    let reason = if error_fixed {
        CandidateReason::ErrorFixed
    } else {
        CandidateReason::UserCorrection
    };

    Some(CandidateSignal {
        reason,
        error_tool,
        error_signature,
        corrective_tools,
        correction_text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::prior_episode::{ToolError, ToolUse};

    fn tool(id: &str, name: &str) -> ToolUse {
        ToolUse {
            id: id.into(),
            name: name.into(),
            file_path: None,
            command: None,
        }
    }

    #[test]
    fn error_fixed_episode_is_a_candidate_with_corrective_tools() {
        let ep = Episode {
            tools: vec![tool("t1", "Bash"), tool("t2", "Edit"), tool("t3", "Bash")],
            errors: vec![ToolError {
                tool: "Bash".into(),
                signature: "error[E0433]".into(),
                tool_use_id: "t1".into(),
            }],
            user_messages: vec![],
            ended_clean: true,
            had_corrective_action: true,
        };
        let sig = detect_candidate(&ep).expect("error→fix→clean is a candidate");
        assert_eq!(sig.reason, CandidateReason::ErrorFixed);
        assert_eq!(sig.error_tool.as_deref(), Some("Bash"));
        assert_eq!(
            sig.corrective_tools,
            vec!["Edit".to_string(), "Bash".to_string()]
        );
    }

    #[test]
    fn user_correction_is_a_candidate() {
        let ep = Episode {
            tools: vec![tool("t1", "Edit")],
            errors: vec![],
            user_messages: vec!["No, edit the generator template instead.".into()],
            ended_clean: true,
            had_corrective_action: false,
        };
        let sig = detect_candidate(&ep).expect("explicit user correction is a candidate");
        assert_eq!(sig.reason, CandidateReason::UserCorrection);
        assert!(sig.correction_text.unwrap().contains("instead"));
    }

    #[test]
    fn clean_run_without_error_or_correction_is_not_a_candidate() {
        let ep = Episode {
            tools: vec![tool("t1", "Read"), tool("t2", "Read")],
            errors: vec![],
            user_messages: vec!["Thanks, looks good".into()],
            ended_clean: true,
            had_corrective_action: false,
        };
        assert!(detect_candidate(&ep).is_none(), "nothing was learned");
    }

    #[test]
    fn unresolved_error_is_not_a_candidate() {
        let ep = Episode {
            tools: vec![tool("t1", "Bash")],
            errors: vec![ToolError {
                tool: "Bash".into(),
                signature: "boom".into(),
                tool_use_id: "t1".into(),
            }],
            user_messages: vec![],
            ended_clean: false,
            had_corrective_action: false,
        };
        assert!(detect_candidate(&ep).is_none(), "no fix to distill");
    }

    #[test]
    fn correction_word_does_not_false_fire_inside_other_words() {
        // "know"/"note" contain "no" but must not trip the detector.
        assert!(!is_correction("I know this looks fine, note the change"));
        assert!(is_correction("no, use ripgrep"));
        assert!(is_correction("next time prefer the generator"));
    }
}
