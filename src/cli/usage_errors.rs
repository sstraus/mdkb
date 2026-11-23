//! One-line usage errors that name the command that works.
//!
//! A model reads the error and retries. So every usage error is a single line,
//! `error: <what is wrong>. Run: <exact command>`, where the command reuses
//! every argument that was given and drops only the wrong one. clap's own
//! rendering is a usage block that names no working command.
//!
//! Rules (story 091-ba1b, final.md "Regole degli errori"):
//! - exit 2 for every usage error, exit 1 for runtime failures;
//! - a flag that belongs to another command names that command;
//! - an unknown flag shows the nearest flag and the flags of the command;
//! - an unknown subcommand goes through a table of typical guesses first;
//! - an invalid enum value lists the valid values on the same line;
//! - under `--format json` the same error is `{"error": "...", "run": "..."}`.

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Command, CommandFactory};

use super::Cli;

/// A usage error ready to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageReport {
    /// What is wrong, without the `error:` prefix or the `Run:` suffix.
    pub message: String,
    /// A command line that works.
    pub run: String,
}

impl UsageReport {
    /// The text form: one line on stderr.
    pub fn text(&self) -> String {
        format!("error: {}. Run: {}", self.message, self.run)
    }

    /// The `--format json` form.
    pub fn json(&self) -> String {
        serde_json::json!({"error": self.message, "run": self.run}).to_string()
    }

    /// Print in the form the arguments ask for.
    pub fn render(&self, json: bool) -> String {
        if json { self.json() } else { self.text() }
    }
}

/// Typical first guesses of a command name, and the commands they mean. The
/// first path that exists in the command tree wins.
const GUESSES: &[(&str, &[&[&str]])] = &[
    ("grep", &[&["code", "grep"]]),
    ("mget", &[&["get"]]),
    ("history", &[&["evolve", "history"]]),
    ("current", &[&["evolve", "current"]]),
    ("superseded-by", &[&["evolve", "superseded-by"]]),
    ("find", &[&["code", "search"]]),
    ("symbols", &[&["code", "search"]]),
    ("def", &[&["code", "search"]]),
    ("index", &[&["update"]]),
    ("reindex", &[&["update"]]),
    ("health", &[&["status"]]),
    ("show", &[&["get"]]),
    ("read", &[&["get"]]),
    ("cat", &[&["get"]]),
    ("delete", &[&["memory", "rm"], &["collection", "rm"]]),
    ("remove", &[&["memory", "rm"], &["collection", "rm"]]),
    ("rm", &[&["memory", "rm"], &["collection", "rm"]]),
];

/// A flag that several commands define belongs, for the suggestion, to this one.
const PREFERRED_OWNERS: &[(&str, &[&str])] = &[
    ("--entry-type", &["memory", "search"]),
    ("--path", &["code", "search"]),
    // `search -i` moved to `code grep`; `memory condense -i` is `--interactive`.
    ("-i", &["code", "grep"]),
];

/// True when the arguments ask for JSON output.
pub fn wants_json(argv: &[String]) -> bool {
    argv.windows(2)
        .any(|w| w[0] == "--format" && w[1] == "json")
        || argv.iter().any(|a| a == "--format=json")
}

/// Quote one argument for a POSIX shell, only when it needs it.
pub fn shell_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@%+".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// `mdkb` followed by the quoted tokens.
pub fn command_line<S: AsRef<str>>(tokens: &[S]) -> String {
    let mut parts = vec!["mdkb".to_string()];
    parts.extend(tokens.iter().map(|t| shell_quote(t.as_ref())));
    parts.join(" ")
}

/// The command tree with global flags propagated to every level.
fn built_root() -> Command {
    let mut root = Cli::command();
    root.build();
    root
}

/// Walk `path` down the tree.
fn find<'a>(root: &'a Command, path: &[String]) -> Option<&'a Command> {
    let mut cmd = root;
    for name in path {
        cmd = cmd.find_subcommand(name)?;
    }
    Some(cmd)
}

/// Where the command words sit in `tail` (the arguments after `mdkb`).
struct Located {
    /// Canonical names of the command path that was found.
    path: Vec<String>,
    /// Indexes in `tail` of the tokens that named those commands.
    path_idx: Vec<usize>,
    /// Index in `tail` of the first word the deepest command could not take as
    /// a subcommand, when that command has subcommands.
    unknown_idx: Option<usize>,
}

fn locate(root: &Command, tail: &[String]) -> Located {
    let mut cmd = root;
    let mut found = Located {
        path: Vec::new(),
        path_idx: Vec::new(),
        unknown_idx: None,
    };
    let mut i = 0;
    while i < tail.len() {
        let token = &tail[i];
        if token == "--format" {
            i += 2;
            continue;
        }
        if token.starts_with('-') {
            i += 1;
            continue;
        }
        if cmd.has_subcommands() {
            match cmd.find_subcommand(token) {
                Some(next) => {
                    found.path.push(next.get_name().to_string());
                    found.path_idx.push(i);
                    cmd = next;
                }
                None => {
                    found.unknown_idx = Some(i);
                    break;
                }
            }
        }
        i += 1;
    }
    found
}

/// Whether `cmd` defines `flag` (`--long` or `-s`).
fn arg_named<'a>(cmd: &'a Command, flag: &str) -> Option<&'a clap::Arg> {
    cmd.get_arguments().find(|a| match flag.strip_prefix("--") {
        Some(long) => {
            a.get_long() == Some(long) || a.get_all_aliases().is_some_and(|al| al.contains(&long))
        }
        None => {
            let short = flag.trim_start_matches('-');
            short.chars().count() == 1 && a.get_short() == short.chars().next()
        }
    })
}

/// Every spelling of the flag `flag` names on `cmd`: `--long`, `-s`, aliases.
fn spellings(cmd: Option<&Command>, flag: &str) -> Vec<String> {
    let mut names = vec![flag.to_string()];
    if let Some(arg) = cmd.and_then(|c| arg_named(c, flag)) {
        if let Some(long) = arg.get_long() {
            names.push(format!("--{long}"));
        }
        if let Some(short) = arg.get_short() {
            names.push(format!("-{short}"));
        }
        names.extend(
            arg.get_all_aliases()
                .into_iter()
                .flatten()
                .map(|a| format!("--{a}")),
        );
    }
    names
}

/// `tail` without the flag `flag` and, when that flag takes a value, its value.
fn drop_flag(cmd: Option<&Command>, tail: &[String], flag: &str) -> Vec<String> {
    let takes_value = cmd
        .and_then(|c| arg_named(c, flag))
        .is_none_or(|a| a.get_action().takes_values());
    let names = spellings(cmd, flag);
    let mut out = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        let t = &tail[i];
        let name = t.split('=').next().unwrap_or(t);
        if names.iter().any(|n| n == name) {
            if t.contains('=') || !takes_value {
                i += 1;
            } else {
                i += 2;
            }
            continue;
        }
        out.push(t.clone());
        i += 1;
    }
    out
}

/// The flag an error is about, without any `=value`, e.g. `--entry-type`.
fn flag_of(token: &str) -> String {
    token.split('=').next().unwrap_or(token).to_string()
}

/// Parse `tokens` against the command tree and report the clap error, if any.
fn try_parse(tokens: &[String]) -> Result<(), clap::Error> {
    let mut full = vec!["mdkb".to_string()];
    full.extend(tokens.iter().cloned());
    Cli::command().try_get_matches_from(full).map(|_| ())
}

/// Make `tokens` parse by dropping every flag clap does not know, one at a
/// time. A suggestion that does not run is worse than none.
fn runnable(mut tokens: Vec<String>) -> Vec<String> {
    for _ in 0..8 {
        let Err(err) = try_parse(&tokens) else {
            return tokens;
        };
        let bad = match err.get(ContextKind::InvalidArg) {
            Some(ContextValue::String(s)) => flag_of(s),
            _ => return tokens,
        };
        if !bad.starts_with('-') {
            return tokens;
        }
        let root = built_root();
        let at = locate(&root, &tokens);
        tokens = drop_flag(find(&root, &at.path), &tokens, &bad);
    }
    tokens
}

/// Levenshtein distance, for "did you mean".
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != *cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// The closest of `names` to `word`, when it is close enough to be a typo.
fn nearest<'a>(word: &str, names: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let limit = (word.chars().count() / 3).max(2);
    names
        .map(|n| (distance(word, n), n))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, n)| n)
}

/// Every visible command path with a visible `flag`, for "belongs to".
fn owners_of(root: &Command, flag: &str) -> Vec<Vec<String>> {
    fn walk(cmd: &Command, path: &mut Vec<String>, flag: &str, out: &mut Vec<Vec<String>>) {
        for sub in cmd.get_subcommands().filter(|s| !s.is_hide_set()) {
            path.push(sub.get_name().to_string());
            if arg_named(sub, flag).is_some_and(|a| !a.is_global_set() && !a.is_hide_set()) {
                out.push(path.clone());
            }
            walk(sub, path, flag, out);
            path.pop();
        }
    }
    let mut out = Vec::new();
    walk(root, &mut Vec::new(), flag, &mut out);
    out
}

fn flags_of(cmd: &Command) -> Vec<String> {
    cmd.get_arguments()
        .filter(|a| {
            !a.is_hide_set()
                && !a.is_global_set()
                && !a.is_positional()
                && !matches!(a.get_id().as_str(), "help" | "version")
        })
        .filter_map(|a| a.get_long().map(|l| format!("--{l}")))
        .collect()
}

fn visible_names(cmd: &Command) -> Vec<String> {
    cmd.get_subcommands()
        .filter(|s| !s.is_hide_set())
        .map(|s| s.get_name().to_string())
        .collect()
}

fn help_command(path: &[String]) -> String {
    let mut tokens: Vec<&str> = path.iter().map(String::as_str).collect();
    tokens.push("--help");
    command_line(&tokens)
}

fn context_string(err: &clap::Error, kind: ContextKind) -> Option<String> {
    match err.get(kind)? {
        ContextValue::String(s) => Some(s.clone()),
        ContextValue::Strings(v) => Some(v.join(", ")),
        ContextValue::StyledStr(s) => Some(s.to_string()),
        _ => None,
    }
}

/// Turn a clap error into the one-line report. `None` for help and version,
/// which are not errors.
pub fn from_clap(err: &clap::Error, argv: &[String]) -> Option<UsageReport> {
    let tail: Vec<String> = argv.iter().skip(1).cloned().collect();
    let root = built_root();
    let at = locate(&root, &tail);
    let cmd = find(&root, &at.path).unwrap_or(&root);
    let rest_without_path = |skip: &[usize]| -> Vec<String> {
        tail.iter()
            .enumerate()
            .filter(|(i, _)| !at.path_idx.contains(i) && !skip.contains(i))
            .map(|(_, t)| t.clone())
            .collect()
    };
    let with_path = |path: &[String], rest: Vec<String>| -> Vec<String> {
        path.iter().cloned().chain(rest).collect()
    };

    match err.kind() {
        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => None,
        ErrorKind::InvalidSubcommand => {
            let unknown = at
                .unknown_idx
                .and_then(|i| tail.get(i).cloned())
                .or_else(|| context_string(err, ContextKind::InvalidSubcommand))?;
            let skip: Vec<usize> = at.unknown_idx.into_iter().collect();
            let rest = rest_without_path(&skip);
            let guess = GUESSES
                .iter()
                .find(|(word, _)| *word == unknown)
                .and_then(|(_, targets)| {
                    targets.iter().find(|t| {
                        let p: Vec<String> = t.iter().map(|s| (*s).to_string()).collect();
                        // A guess names a path from the top, or inside the group
                        // the caller already typed (`code symbols`).
                        at.path.is_empty() && find(&root, &p).is_some()
                            || at.path.len() == 1
                                && p.len() == 2
                                && p[0] == at.path[0]
                                && find(&root, &p).is_some()
                    })
                })
                .map(|t| t.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
            let names = visible_names(cmd);
            let guess = guess.or_else(|| {
                nearest(&unknown, names.iter().map(String::as_str)).map(|n| {
                    let mut p = at.path.clone();
                    p.push(n.to_string());
                    p
                })
            });
            let group = if at.path.is_empty() {
                "mdkb".to_string()
            } else {
                format!("mdkb {}", at.path.join(" "))
            };
            Some(match guess {
                Some(target) => {
                    let line = command_line(&runnable(with_path(&target, rest)));
                    UsageReport {
                        message: format!(
                            "unknown command '{unknown}' in '{group}'; use `{}`",
                            command_line(&target)
                        ),
                        run: line,
                    }
                }
                None => UsageReport {
                    message: format!(
                        "unknown command '{unknown}' in '{group}'; commands: {}",
                        names.join(", ")
                    ),
                    run: help_command(&at.path),
                },
            })
        }
        ErrorKind::UnknownArgument => {
            let given = context_string(err, ContextKind::InvalidArg)?;
            if !given.starts_with('-') {
                return Some(extra_positional(&given, &at.path, &tail, &at));
            }
            let flag = flag_of(&given);
            let rest = drop_flag(Some(cmd), &rest_without_path(&[]), &flag);
            let owners = owners_of(&root, &flag);
            let preferred = PREFERRED_OWNERS
                .iter()
                .find(|(f, _)| *f == flag)
                .map(|(_, path)| path.iter().map(|s| (*s).to_string()).collect::<Vec<_>>());
            let owner = preferred
                .as_ref()
                .filter(|p| owners.contains(p))
                .or_else(|| owners.iter().find(|o| o.first() == at.path.first()))
                .or_else(|| owners.first());
            if let Some(owner) = owner {
                // Rewrite for the owner, keeping the flag and its value.
                let kept = rest_without_path(&[]);
                let candidate = runnable_keeping(with_path(owner, kept), &flag);
                return Some(UsageReport {
                    message: format!("{flag} belongs to {}", owner.join(" ")),
                    run: command_line(&candidate),
                });
            }
            let flags = flags_of(cmd);
            let near = nearest(
                flag.trim_start_matches('-'),
                flags.iter().map(|f| f.trim_start_matches('-')),
            )
            .map(|n| format!("; nearest --{n}"))
            .unwrap_or_default();
            let listing = if flags.is_empty() {
                " (it takes no flags)".to_string()
            } else {
                format!("; flags: {}", flags.join(" "))
            };
            let line = command_line(&runnable(with_path(&at.path, rest)));
            Some(UsageReport {
                message: format!("unknown flag {flag}{near}{listing}"),
                run: line,
            })
        }
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            let flag = context_string(err, ContextKind::InvalidArg)?;
            let flag = flag.split_whitespace().next().unwrap_or(&flag).to_string();
            let value = context_string(err, ContextKind::InvalidValue).unwrap_or_default();
            let valid = context_string(err, ContextKind::ValidValue).unwrap_or_default();
            let rest = drop_flag(Some(cmd), &rest_without_path(&[]), &flag);
            let detail = if valid.is_empty() {
                // `invalid value 'abc' for '--limit <LIMIT>': invalid digit ...`
                let text = err.to_string();
                let first = text.lines().next().unwrap_or_default();
                first
                    .split_once("': ")
                    .map(|(_, reason)| format!(": {}", reason.trim_end_matches('.')))
                    .unwrap_or_default()
            } else {
                format!("; valid: {valid}")
            };
            Some(UsageReport {
                message: format!("invalid value '{value}' for {flag}{detail}"),
                run: command_line(&runnable(with_path(&at.path, rest))),
            })
        }
        ErrorKind::ArgumentConflict => {
            let bad = context_string(err, ContextKind::InvalidArg)?;
            let flag = bad.split_whitespace().next().unwrap_or(&bad).to_string();
            let prior = context_string(err, ContextKind::PriorArg).unwrap_or_default();
            let prior = prior
                .split_whitespace()
                .next()
                .unwrap_or(&prior)
                .to_string();
            let rest = drop_flag(Some(cmd), &rest_without_path(&[]), &flag);
            Some(UsageReport {
                message: format!("{flag} cannot be combined with {prior}"),
                run: command_line(&runnable(with_path(&at.path, rest))),
            })
        }
        ErrorKind::MissingRequiredArgument => {
            let missing = context_string(err, ContextKind::InvalidArg).unwrap_or_default();
            let missing = missing.trim_end_matches('.').to_string();
            Some(UsageReport {
                message: format!("missing required argument {missing}"),
                run: help_command(&at.path),
            })
        }
        ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            let names = visible_names(cmd);
            let group = if at.path.is_empty() {
                "mdkb".to_string()
            } else {
                format!("mdkb {}", at.path.join(" "))
            };
            Some(UsageReport {
                message: format!("'{group}' needs a command: {}", names.join(", ")),
                run: help_command(&at.path),
            })
        }
        _ => {
            let first = err
                .to_string()
                .lines()
                .next()
                .unwrap_or("invalid arguments")
                .trim_start_matches("error: ")
                .trim_end_matches('.')
                .to_string();
            Some(UsageReport {
                message: first,
                run: help_command(&at.path),
            })
        }
    }
}

/// Like [`runnable`], but never drops `keep`: the rewrite exists to carry it.
fn runnable_keeping(tokens: Vec<String>, keep: &str) -> Vec<String> {
    let mut tokens = tokens;
    for _ in 0..8 {
        let Err(err) = try_parse(&tokens) else {
            return tokens;
        };
        let bad = match err.get(ContextKind::InvalidArg) {
            Some(ContextValue::String(s)) => flag_of(s),
            _ => return tokens,
        };
        if !bad.starts_with('-') || bad == keep {
            return tokens;
        }
        let root = built_root();
        let at = locate(&root, &tokens);
        tokens = drop_flag(find(&root, &at.path), &tokens, &bad);
    }
    tokens
}

/// A positional the command does not take. `get A B` becomes `get A,B`.
fn extra_positional(given: &str, path: &[String], tail: &[String], at: &Located) -> UsageReport {
    let positionals: Vec<&String> = tail
        .iter()
        .enumerate()
        .filter(|(i, t)| !at.path_idx.contains(i) && !t.starts_with('-'))
        .map(|(_, t)| t)
        .collect();
    let flags: Vec<String> = tail
        .iter()
        .enumerate()
        .filter(|(i, t)| !at.path_idx.contains(i) && t.starts_with('-'))
        .map(|(_, t)| t.clone())
        .collect();
    if path == ["get"] && positionals.len() > 1 {
        let joined = positionals
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let mut tokens = vec!["get".to_string(), joined];
        tokens.extend(flags);
        return UsageReport {
            message: "get takes one ID; for several use a comma list".to_string(),
            run: command_line(&runnable(tokens)),
        };
    }
    let rest: Vec<String> = tail
        .iter()
        .enumerate()
        .filter(|(i, t)| !at.path_idx.contains(i) && t.as_str() != given)
        .map(|(_, t)| t.clone())
        .collect();
    let tokens = path.iter().cloned().chain(rest).collect::<Vec<_>>();
    UsageReport {
        message: format!("unexpected argument '{given}'"),
        run: command_line(&runnable(tokens)),
    }
}

/// Print a clap error the model-friendly way and exit. Help and version keep
/// clap's own output and exit code.
pub fn exit_with(err: clap::Error, argv: &[String]) -> ! {
    match from_clap(&err, argv) {
        Some(report) => {
            eprintln!("{}", report.render(wants_json(argv)));
            std::process::exit(2);
        }
        None => err.exit(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        std::iter::once("mdkb")
            .chain(args.iter().copied())
            .map(String::from)
            .collect()
    }

    fn report(args: &[&str]) -> UsageReport {
        let v = argv(args);
        let err = Cli::command()
            .try_get_matches_from(v.clone())
            .expect_err("these arguments must fail");
        from_clap(&err, &v).expect("a usage error")
    }

    #[test]
    fn quotes_only_what_the_shell_needs() {
        assert_eq!(shell_quote("foo"), "foo");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn json_is_asked_for_by_either_spelling() {
        assert!(wants_json(&argv(&["--format", "json", "search", "x"])));
        assert!(wants_json(&argv(&["search", "--format=json"])));
        assert!(!wants_json(&argv(&["search", "json"])));
    }

    #[test]
    fn distance_counts_edits() {
        assert_eq!(distance("grep", "grep"), 0);
        assert_eq!(distance("serch", "search"), 1);
        assert_eq!(distance("abc", "xyz"), 3);
    }

    #[test]
    fn a_help_request_is_not_an_error() {
        let v = argv(&["--help"]);
        let err = Cli::command().try_get_matches_from(v.clone()).unwrap_err();
        assert!(from_clap(&err, &v).is_none());
    }

    #[test]
    fn report_is_one_line() {
        let r = report(&["search", "foo", "--nosuchflag"]);
        assert!(!r.text().contains('\n'), "{}", r.text());
        assert!(r.text().starts_with("error: "), "{}", r.text());
    }
}
