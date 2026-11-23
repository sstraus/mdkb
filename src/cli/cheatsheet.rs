//! `mdkb cheatsheet`: the one page a model reads to use the CLI.
//!
//! It holds the start guide, the task-to-command table, and the "MCP <-> CLI"
//! map that `mdkb surface` used to print. A command that changes name changes
//! here in the same commit; `tests/cli_errors.rs` runs every `mdkb` line below
//! under `# Search` and `# Code` to prove it still parses.

/// The cheatsheet text.
pub fn render() -> String {
    format!("{GUIDE}{COMMANDS}\n{}", crate::core::surface::render())
}

const GUIDE: &str = "\
# Start
mdkb init                                               # once, inside the project; then read .mdkb/config.toml
mdkb --help                                             # the commands; `mdkb <command> --help` lists the flags of one
mdkb schema <command>                                   # the flags of one command as JSON
mdkb setup --help                                       # install the MCP server and the hooks
# Every setting is in .mdkb/config.toml as a commented line. Leave defaults commented; uncomment only what you change.
# A wrong command prints one line: `error: <what is wrong>. Run: <the command that works>`, and exits 2.

";

const COMMANDS: &str = "\
# Search: pick the command by what you look for
mdkb search <words>                                     # docs and memory, two groups, each up to --limit; unquoted words are one query
mdkb search <query> --docs                              # documents only
mdkb search <query> -c <collection>                     # one collection (documents only)
mdkb search <query> --memory                            # memory only; same as `mdkb memory search <query>`
mdkb search <query> --entry-type decision               # memory of one type (implies --memory)
mdkb search <query> --root NAME                         # another repo, a comma list, or '*' for every known repo
mdkb status --root NAME                                 # every other command takes --root too, in any position, but names exactly one repo
mdkb get <id|path|collection/path|collection:path|slug> # id, path (with or without .md), or memory slug
mdkb get <id> --lines 10:50                             # line range
mdkb get 'docs/*.md' -c <collection>                    # several documents: glob or comma list
mdkb code search parse_config                           # symbols by name, fuzzy, exact matches first
mdkb code search parse_config --exact                   # symbols named exactly this
mdkb code search <name> -k function --path src/cli      # narrow by kind and by path prefix (not a glob)
mdkb code search 'read config from disk' --semantic     # symbols by meaning (needs code embeddings)
mdkb code grep 'fn main'                                # live source lines
mdkb code grep -e TODO -e FIXME -i --path src/          # OR alternatives, case-insensitive, one path prefix
mdkb code grep 'fn \\w+_config' --regex                 # regular expression
mdkb code grep 'Fork from here' --literals              # indexed AST literal values: JSX text, i18n strings, across line breaks
# `search --format json` shape: {\"documents\": [...], \"memory\": [...]}; a group you did not ask for is [].
# `code search --format json`: an array of symbols. `code grep --format json`: {hits, status, coverage, truncated}.
# No hits in `search` for a code name? It prints the `code search` command on stderr.

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
mdkb memory search <query> --entry-type TYPE            # memory only
mdkb memory warmup                                      # compact index to load at session start
mdkb memory history <id>                                # revisions, including versions a conflict superseded

# Memory projection (entries live in the database; .mdkb/memory/entries/*.md is
# the git-tracked copy, and the only copy that survives losing the database)
mdkb memory export                                      # write missing entries to disk; add --overwrite to refresh all
mdkb memory sync                                        # reconcile database and disk both ways
mdkb memory import <path>                               # load a JSON file or a folder of markdown
mdkb memory import --journal <file|dir>                 # load Claude Code journal entries

# Code intelligence
mdkb code callers <symbol>                              # who calls this?
mdkb code calls <symbol>                                # what does this call?
mdkb code impact <symbol>                               # transitive dependency graph
mdkb update --code [--force]                            # source index only; --force rebuilds it from scratch

# Audits (both read the code index)
mdkb code dup                                           # duplication sweep; structural pass only, no model
mdkb code dup --semantic                                # add the embedding pass: minutes, not seconds ([code.duplication] semantic = true makes it standing)
mdkb code dup --since HEAD                              # review mode: only clusters the change touched
mdkb code dup --file src/x.rs                           # clones of this file anywhere in the repo
mdkb code coupling                                      # files that change together without referencing each other
mdkb graph coupling                                     # documents that change with the source they describe

# Knowledge graph (frontmatter + wikilink edges; refs accept collection-prefixed paths)
mdkb graph links <entity>                               # outgoing edges (endpoints shown as paths, with 'via' relation)
mdkb graph backlinks <entity>                           # incoming edges; entity = path, id: or alias
mdkb graph links <memory-id> --scope memory             # memory edges (links/backlinks only)
mdkb graph links <entity> --root NAME                   # another repo (one repo, not a fan-out)
mdkb graph neighbors <entity> --depth 2                 # adjacent entities (undirected), each with 'via'
mdkb graph path <a> <b>                                 # shortest path between two entities
mdkb graph dangling [-c <collection>]                   # refs resolving to no doc (full scan; explicit only)
mdkb graph hubs --relation owner --limit 20             # entities by degree centrality (full scan; explicit only)
mdkb graph relations [--apply]                          # keys whose values name real docs; --apply writes them (no-op in auto mode)

# Document evolution (newer -> older; supersedes/retracts hide the older doc)
mdkb evolve link <new> supersedes <old> -r REASON        # also updates|corrects|retracts|extends; cycles refused
mdkb evolve history <doc>                               # edges with their #id; also evolve current|superseded-by
mdkb evolve unlink <id>                                 # undo one edge (alias rm); the older doc becomes current again

# Collections
mdkb collection list                                    # name, path, pattern, doc count per collection
mdkb collection add <name> <path> -p '**/*.md'          # register a collection
mdkb collection rm <name>                               # unregister it (aliases: remove, delete)
mdkb collection update <name> --path <path> -p '**/*.md' # change path/pattern without dropping matching embeddings

# Developer feedback (local; prompt text is never stored)
mdkb setup developer                                    # enable private per-repository recall telemetry
mdkb metrics quality --period 7                         # zero results, repeated searches, score bands
mdkb metrics latency --period 7                         # recall count and latency
mdkb metrics purge --yes                                # delete all stored query telemetry

# Maintenance
mdkb update                                             # reindex all (auto-embeds docs + backfills memory)
mdkb update --sessions                                  # index Claude Code transcripts only
mdkb update --reembed                                   # rebuild every embedding for the selected model
mdkb update --collection claude_sessions                # embed a specific collection (sessions excluded by default)
mdkb status                                             # one page: index, memory, code, sessions, hooks, daemon, telemetry; problems end with their fix
mdkb doctor                                             # problems and their fixes; non-zero exit on an error (--full adds the distiller probe)
mdkb compact                                            # vacuum both databases
mdkb compact --prune-sessions --older-than 90d --export dir  # hard-delete archived transcripts (exports first)
mdkb memory prune --days 90 --dry-run                   # preview: expired entries + reminders/priors/handoffs unread for 90d
mdkb memory audit                                       # entries worth re-reading (dead refs, drifted source, duplicates); decides nothing

# Daemon (the daemon owns every write; the CLI routes mutations to it)
mdkb daemon restart                                     # after upgrading the binary
mdkb repos list                                         # every known repo: counts, freshness, health (MCP: `repos`)
mdkb repos refresh --only outdated                      # migrate every store older than this binary, each after a verified backup copy
MDKB_NO_DAEMON=1 mdkb <cmd>                             # run in-process instead, for debugging
MDKB_NAMESPACE=<name> mdkb <cmd>                        # use .mdkb/namespaces/<name>/ instead; test runners get `test` unasked

# Repo selector (the MCP `root` parameter and CLI `--root`)
(omitted)                                               # the declared workspace, and every store nested under it
root=\"/abs/path\"                                        # nearest existing store or known repo containing this path
root=\".\"                                                # relative to the CLI cwd or the first declared MCP workspace
root=\"./dir\"                                          # explicit path; bare dir stays a repo name even if ./dir exists
root=\"name\"                                             # one repo by name, the last component of a known root
root=\"parent/name\"                                     # the same, by trailing path components: how a name several repos share is told apart
root=\"name,/abs/path\"                                   # several repos, comma-separated, names and paths mixed
root=\"*\"                                                # every known repo in the caller's scope, plus repos with no scope; the footer counts the rest (Excluded by scope: N)
root=\"*:all\"                                            # every known repo, scopes mixed, and the claude_sessions collections `*` leaves out
root=\"scope:home\"                                       # the known repos whose scope is home
# A caller with no scope (no MCP roots, or a directory under no prefix) is not filtered: `*` returns every scope's repos.
# Scope: `[[scopes]] prefix = \"~/Gits/home\"` / `scope = \"home\"` in daemon.toml, or `\"scope\"` on a repos.json entry (wins over the prefix).
# Only `search` fans out; every other command needs a selector naming one repo —
# with `root` omitted that is the declared workspace itself, when it is a store.
# A comma always separates repos, so a path containing one is refused, not split.

# A store carries a schema version. When the STORE is newer than the binary,
# every command refuses rather than touch it. When the store is OLDER, a read
# command migrates it and says so on stderr — that migration also rewrites
# memory entries and prior clusters, so back the store up first if it matters.
# `root=\"*\"` does not migrate: it names the stores it skipped, and `mdkb repos refresh
# --only outdated` migrates them all, each after a verified backup copy.
";
