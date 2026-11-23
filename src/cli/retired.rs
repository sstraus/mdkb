//! The removed-name table of the status/update/top-level consolidation: what
//! each old command line becomes, built from the arguments it was given so the
//! suggested command runs as printed.

use crate::cli::usage_errors::shell_quote;

/// Join arguments into one shell-quoted tail, each preceded by a space.
pub fn quoted_tail<'a>(args: impl IntoIterator<Item = &'a String>) -> String {
    args.into_iter()
        .map(|a| format!(" {}", shell_quote(a)))
        .collect()
}

/// The replacement command for a removed name, rebuilt from the arguments it received.
pub fn consolidated_replacement(old: &str, args: &[String]) -> String {
    let tail = || quoted_tail(args);
    let value_of = |flag: &str| -> Option<&String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    let has = |flags: &[&str]| args.iter().any(|a| flags.contains(&a.as_str()));
    match old {
        "stats" | "code info" | "daemon status" | "metrics status" => {
            let keep = if old == "stats" {
                tail()
            } else {
                String::new()
            };
            format!("mdkb status{keep}")
        }
        "embed" => format!("mdkb update{}", tail()),
        "dup" => format!("mdkb code dup{}", tail()),
        "coupling" if has(&["--docs"]) => {
            let rest: Vec<&String> = args.iter().filter(|a| *a != "--docs").collect();
            format!("mdkb graph coupling{}", quoted_tail(rest))
        }
        "coupling" => format!("mdkb code coupling{}", tail()),
        "code index" => {
            let force = if has(&["--force"]) { " --force" } else { "" };
            format!("mdkb update --code{force}")
        }
        "code init" => "mdkb update --code".to_string(),
        "setup check" => "mdkb doctor --full".to_string(),
        "session" => {
            let rest: Vec<&String> = args.iter().filter(|a| *a != "index").collect();
            format!("mdkb update --sessions{}", quoted_tail(rest))
        }
        "journal" => {
            let import_all = args.first().is_some_and(|a| a == "import-all");
            let path = if import_all {
                value_of("--dir")
                    .cloned()
                    .unwrap_or_else(|| ".claude/journal".to_string())
            } else {
                args.iter()
                    .skip(1)
                    .find(|a| !a.starts_with('-'))
                    .cloned()
                    .unwrap_or_else(|| ".claude/journal".to_string())
            };
            let dry = if has(&["--dry-run", "-n", "-d"]) {
                " --dry-run"
            } else {
                ""
            };
            format!("mdkb memory import --journal {}{dry}", shell_quote(&path))
        }
        verb @ ("evolve supersedes" | "evolve updates" | "evolve corrects" | "evolve retracts"
        | "evolve extends") => {
            let relation = verb.trim_start_matches("evolve ");
            let mut positional = args.iter().filter(|a| !a.starts_with('-'));
            let (new, old_doc) = (positional.next(), positional.next());
            let rest: Vec<&String> = match (new, old_doc) {
                (Some(n), Some(o)) => args.iter().filter(|a| *a != n && *a != o).collect(),
                _ => Vec::new(),
            };
            format!(
                "mdkb evolve link {} {relation} {}{}",
                new.map_or("NEW".to_string(), |n| shell_quote(n)),
                old_doc.map_or("OLD".to_string(), |o| shell_quote(o)),
                quoted_tail(rest)
            )
        }
        other => format!("mdkb {other} --help"),
    }
}

/// The argv of a hidden legacy alias rewritten to the command that replaces it,
/// or `None` when `command` is not one. `stats`, `embed` and `code index` keep
/// working, unlisted in help, docs and the changelog, so scripts that call them
/// (the TUICommander dashboard before 1.5.0) survive the rename.
///
/// DEFERRED (2026-11-23) — remove in February 2027 (Boss): drop this function,
/// its call in `Cli::parse_args`, and `legacy_aliases_still_run_and_stay_out_of_help`.
pub fn legacy_alias_argv(command: &super::Command, argv: &[String]) -> Option<Vec<String>> {
    use super::{CodeCommand, Command};
    let (word, args, replacement): (&str, &[String], Vec<String>) = match command {
        Command::Stats(a) => ("stats", &a.args, vec!["status".into()]),
        Command::Embed(a) => ("embed", &a.args, vec!["update".into()]),
        Command::Code(CodeCommand::Index(a)) => {
            ("index", &a.args, vec!["update".into(), "--code".into()])
        }
        _ => return None,
    };
    // The retired variants take every trailing argument, so the subcommand word
    // sits right before them.
    let at = argv.len().checked_sub(args.len() + 1)?;
    if argv[at] != word {
        return None;
    }
    let mut head: Vec<String> = argv[..at].to_vec();
    let mut tail: Vec<String> = args.to_vec();
    if word == "index" {
        let code = head.iter().rposition(|a| a == "code")?;
        head.remove(code);
        // `code index` took paths; `update --code` indexes the whole repo.
        let mut kept = Vec::new();
        for a in tail {
            let value_of_global = kept
                .last()
                .is_some_and(|p: &String| p == "--format" || p == "--root");
            if a.starts_with('-') || value_of_global {
                kept.push(a);
            }
        }
        tail = kept;
    }
    head.extend(replacement);
    head.extend(tail);
    Some(head)
}
