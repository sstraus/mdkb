//! MCP and CLI are one surface, and it has to be checked rather than believed.
//!
//! Story 024-0c7e. The two expose overlapping capability through independent
//! code paths and nothing asserted they agree. The concrete symptom that started
//! it: `mdkb memory-write` does not exist as a CLI command — it is
//! `mdkb hook memory-write`, reachable only if you already know — while the MCP
//! tool is `memory_write`. A caller who knows one surface cannot guess the
//! other.
//!
//! The map in `core::surface` is the single inventory. These tests exist to make
//! it impossible to add a tool on one side and forget the other: the map is
//! checked against the MCP tool list the server actually advertises, and against
//! the CLI commands clap actually parses. Neither is a copy of the other.
//!
//! Story 049-11af added the second half: not only the names, the *behaviour*.
//! `mdkb dup` and `search(scope="duplicates")` run the real binary and the real
//! MCP dispatch over one repository and must render byte-identical reports.
//!
//! Story 067-5ab6 generalised that gate to the two paths that had already
//! diverged in production: the memory write (`mdkb memory add` against
//! `memory_write`) and `search`. The write tests compare the persisted row —
//! every column, the typed edges, the embedding, the revision count and the
//! markdown projection — not the acknowledgement line. Some of them are red on
//! purpose and stay red until story 068-db06 unifies the two write paths; each
//! red assertion names that story and the divergence it encodes, so an expected
//! red can be told from a regression. Every process spawned here is hermetic:
//! throwaway `HOME`, no daemon, no developer socket.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Mutex;

use mdkb::config::Config;
use mdkb::core::Context;
use mdkb::core::surface::{SURFACE_MAP, SurfaceEntry};
use mdkb::daemon::registry::RepoHandle;
use mdkb::mcp::tools::{MemoryWriteBatchEntry, RelatesInput, SearchParams};
use mdkb::store::memory::{EntryType, PRIOR_TTL_SECS};

#[path = "common/cli.rs"]
mod cli;

// ── Shared harness ───────────────────────────────────────────────────────────

/// Run the hermetic binary and require exit 0.
fn run(args: &[&str], cwd: &Path) -> Output {
    let out = cli::run(args, cwd);
    assert!(
        out.status.success(),
        "`mdkb {}` exit={:?}\nstdout: {}\nstderr: {}",
        args.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    out
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The MCP side of every parity test: the same `RepoHandle` the daemon would
/// build for this repository, over the store the CLI just wrote.
fn handle(root: &Path) -> Arc<RepoHandle> {
    Arc::new(RepoHandle::from_shared(
        root.to_path_buf(),
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(None)),
        Config::load_or_default(root.join(".mdkb/config.toml")),
        Vec::new(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ))
}

/// An initialised repository with the given `.mdkb/config.toml`.
struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Repo {
    fn init(config: &str) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonicalize");
        run(&["init"], &root);
        std::fs::write(root.join(".mdkb/config.toml"), config).expect("write config");
        Self { _dir: dir, root }
    }

    fn handle(&self) -> Arc<RepoHandle> {
        handle(&self.root)
    }
}

fn params(scope: &str, query: &str, file: Option<&str>) -> SearchParams {
    SearchParams {
        query: query.to_string(),
        root: None,
        limit: 10,
        collection: None,
        include_superseded: false,
        scope: Some(scope.to_string()),
        kind: None,
        threshold: None,
        file: file.map(str::to_string),
        min_confidence: None,
        since: None,
    }
}

// ── The inventory (story 024-0c7e) ───────────────────────────────────────────

/// Every tool the MCP server advertises must appear in the map.
///
/// Read from the server's own tool router rather than a list written by hand,
/// so a tool added tomorrow fails this test rather than quietly going
/// undocumented.
#[test]
fn every_mcp_tool_is_in_the_map() {
    let advertised = mdkb::mcp::server::advertised_tool_names();
    assert!(
        !advertised.is_empty(),
        "the check must actually read the server's tool list"
    );

    let mapped: Vec<&str> = SURFACE_MAP.iter().map(|e| e.mcp_tool).collect();
    let missing: Vec<&String> = advertised
        .iter()
        .filter(|t| !mapped.contains(&t.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "MCP tools missing from core::surface::SURFACE_MAP — add them with their \
         CLI equivalent, or with an explicit reason for having none: {missing:?}"
    );
}

/// And the reverse: the map must not describe tools that no longer exist.
#[test]
fn the_map_describes_no_phantom_tools() {
    let advertised = mdkb::mcp::server::advertised_tool_names();
    let phantom: Vec<&str> = SURFACE_MAP
        .iter()
        .map(|e| e.mcp_tool)
        .filter(|t| !advertised.iter().any(|a| a == t))
        .collect();
    assert!(
        phantom.is_empty(),
        "SURFACE_MAP names tools the MCP server does not advertise — a removed \
         tool must be removed from the map too: {phantom:?}"
    );
}

/// Every CLI equivalent named in the map must actually parse.
///
/// A map entry claiming `mdkb memory add` is worthless if the command is
/// spelled differently, and that is exactly the drift this story is about.
#[test]
fn every_cli_equivalent_actually_exists() {
    let mut broken = Vec::new();
    for entry in SURFACE_MAP {
        let Some(cli) = entry.cli_command else {
            continue;
        };
        if !mdkb::core::surface::cli_command_exists(cli) {
            broken.push((entry.mcp_tool, cli));
        }
    }
    assert!(
        broken.is_empty(),
        "SURFACE_MAP names CLI commands that clap does not define: {broken:?}"
    );
}

/// A tool with no CLI equivalent must say why. "None" without a reason is
/// indistinguishable from "nobody got round to it", which is how the gap this
/// story reports survived.
#[test]
fn a_missing_cli_equivalent_carries_a_reason() {
    let unexplained: Vec<&str> = SURFACE_MAP
        .iter()
        .filter(|e| e.cli_command.is_none() && e.note.is_empty())
        .map(|e| e.mcp_tool)
        .collect();
    assert!(
        unexplained.is_empty(),
        "these tools have no CLI equivalent and no stated reason: {unexplained:?}"
    );
}

/// The discoverability path, in both directions. An agent holding one name must
/// be able to find the other without reading the source.
#[test]
fn the_map_resolves_names_in_both_directions() {
    let entry: &SurfaceEntry = SURFACE_MAP
        .iter()
        .find(|e| e.mcp_tool == "memory_write")
        .expect("memory_write must be mapped");
    assert_eq!(
        entry.cli_command,
        Some("memory add"),
        "the reported gap: an agent holding the MCP name must find the CLI one"
    );
    assert_eq!(
        mdkb::core::surface::cli_to_mcp("memory add"),
        Some("memory_write"),
        "and the reverse"
    );
}

/// `mdkb surface` prints the inventory, so the answer is a command rather than a
/// grep through the source.
#[test]
fn the_cli_can_print_the_surface_map() {
    let out = cli::command()
        .arg("surface")
        .output()
        .expect("run mdkb surface");
    assert!(out.status.success(), "`mdkb surface` must exit 0");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("memory_write") && text.contains("memory add"),
        "the output must pair the two names: {text}"
    );
}

/// The cheatsheet is hand-maintained prose listing commands. It drifted before
/// (it advertised `pattern` as an entry type for years), so it is checked.
#[test]
fn the_cheatsheet_names_only_commands_that_exist() {
    let out = cli::command()
        .arg("cheatsheet")
        .output()
        .expect("run mdkb cheatsheet");
    assert!(out.status.success(), "`mdkb cheatsheet` must exit 0");
    let text = String::from_utf8_lossy(&out.stdout);

    // The cheatsheet substitutes the real binary path, so each command line
    // starts with it rather than a placeholder. Checked explicitly: an earlier
    // version of this test looked for a `{0} ` prefix that the output never
    // contains, so it scanned nothing and passed for the wrong reason.
    //
    // Matched as a PATH, never as a string. The cheatsheet prints
    // `std::env::current_exe()` while this test holds the path cargo composed,
    // and on Windows those are two spellings of one file: cargo joins whatever
    // separator `CARGO_TARGET_DIR` used, while the OS reports backslashes
    // throughout. A string compare then finds no lines and the test fails for a
    // reason unrelated to the cheatsheet. `Path` compares by component, and on
    // Windows both separators are separators.
    let bin = Path::new(env!("CARGO_BIN_EXE_mdkb"));
    let mut checked = 0usize;
    let mut broken = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        // The executable path may contain spaces. Find the first separator for
        // which the whole prefix is the same native path instead of splitting
        // at the first space in the string.
        let Some(rest) = line
            .match_indices(' ')
            .find(|(i, _)| Path::new(&line[..*i]) == bin)
            .map(|(i, _)| &line[i + 1..])
        else {
            continue;
        };
        let words: Vec<&str> = rest
            .split_whitespace()
            // Subcommand words only: stop at the first flag (`--scope`),
            // placeholder (`<query>`) or comment (`#`). A hyphen is legal
            // INSIDE a subcommand name (`memory-write`), so the test is "starts
            // with a letter", not "contains no hyphen".
            .take_while(|w| {
                w.starts_with(|c: char| c.is_ascii_lowercase())
                    && w.chars()
                        .all(|c| c.is_ascii_lowercase() || c == '-' || c.is_ascii_digit())
            })
            .collect();
        if words.is_empty() {
            continue;
        }
        checked += 1;
        let candidate = words.join(" ");
        if !mdkb::core::surface::cli_command_exists(&candidate) {
            broken.push(candidate);
        }
    }

    assert!(
        checked > 20,
        "the cheatsheet check scanned only {checked} command lines — it is not \
         reading the output it thinks it is"
    );
    assert!(
        broken.is_empty(),
        "the cheatsheet names commands clap does not define: {broken:?}"
    );
}

/// The same rejected input must carry the same facts on both surfaces.
///
/// Not the same *wording*: the CLI formats a clap usage error and MCP returns a
/// JSON-RPC error, and forcing those into one string would make both worse. What
/// must match is the content — the value that was rejected, and the set that
/// would have been accepted. An agent that gets "invalid entry type" from one
/// surface and a list of alternatives from the other has to learn each surface
/// separately, which is the drift this story is about.
#[test]
fn a_rejected_enum_value_names_the_same_set_on_both_surfaces() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");

    // CLI surface: clap rejects before dispatch.
    let cli = cli::command()
        .args([
            "memory",
            "add",
            "x",
            "--title",
            "T",
            "--content",
            "C",
            "--entry-type",
            "pattern",
        ])
        .current_dir(&root)
        .output()
        .expect("run cli");
    let cli_text = format!(
        "{}{}",
        String::from_utf8_lossy(&cli.stdout),
        String::from_utf8_lossy(&cli.stderr)
    );

    // Core surface: what MCP's memory_write reaches when it parses the same
    // value. Both go through EntryType::from_str, which is the single source.
    let core_err = "pattern"
        .parse::<EntryType>()
        .expect_err("`pattern` is not a variant on either surface");

    for variant in EntryType::ALL {
        let v = variant.as_str();
        assert!(
            cli_text.contains(v),
            "the CLI rejection must name `{v}`: {cli_text}"
        );
        assert!(
            core_err.contains(v),
            "the shared rejection must name `{v}`: {core_err}"
        );
    }
    assert!(
        cli_text.contains("pattern") && core_err.contains("pattern"),
        "both must name the value that was rejected"
    );
}

/// An MCP client must be able to find the CLI name without leaving MCP. The map
/// is rendered into the server instructions for that reason.
#[test]
fn the_mcp_instructions_carry_the_cli_equivalents() {
    let instructions = mdkb::mcp::server::surface_instructions();
    assert!(
        instructions.contains("memory_write") && instructions.contains("memory add"),
        "an agent holding an MCP tool name must find the CLI command without \
         reading source: {instructions}"
    );
}

// ── The memory write (story 067-5ab6, gate for 068-db06) ─────────────────────

/// One logical write, spelled for whichever surface receives it.
#[derive(Clone)]
struct MemoryInput {
    id: &'static str,
    title: &'static str,
    content: &'static str,
    entry_type: &'static str,
    tags: Vec<&'static str>,
    ttl: Option<u64>,
    due_in: Option<u64>,
    source_type: Option<&'static str>,
}

impl MemoryInput {
    fn of(entry_type: &'static str) -> Self {
        Self {
            id: "parity-entry",
            title: "Parity entry",
            content: "The same words through both doors.",
            entry_type,
            tags: vec!["parity", "gate"],
            ttl: None,
            due_in: None,
            source_type: None,
        }
    }

    /// `mdkb memory add` with exactly this input. Exit 0 required.
    fn via_cli(&self, repo: &Repo) {
        let tags = self.tags.join(",");
        let mut args: Vec<String> = vec![
            "memory".into(),
            "add".into(),
            self.id.into(),
            "--title".into(),
            self.title.into(),
            "--content".into(),
            self.content.into(),
            "--entry-type".into(),
            self.entry_type.into(),
        ];
        if !self.tags.is_empty() {
            args.extend(["--tags".to_string(), tags]);
        }
        if let Some(ttl) = self.ttl {
            args.extend(["--ttl".to_string(), ttl.to_string()]);
        }
        if let Some(due_in) = self.due_in {
            args.extend(["--due-in".to_string(), due_in.to_string()]);
        }
        if let Some(st) = self.source_type {
            args.extend(["--source-type".to_string(), st.to_string()]);
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        run(&args, &repo.root);
    }

    /// The `memory_write` tool with exactly this input, plus whatever only the
    /// MCP schema can express.
    fn as_mcp(&self, relates: Vec<RelatesInput>, agent: Option<&str>) -> MemoryWriteBatchEntry {
        MemoryWriteBatchEntry {
            id: self.id.to_string(),
            title: self.title.to_string(),
            content: self.content.to_string(),
            source_file: None,
            entry_type: self.entry_type.to_string(),
            tags: self.tags.iter().map(|t| t.to_string()).collect(),
            // The schema default, which is also what the CLI applies when
            // `--source-type` is omitted.
            source_type: Some(self.source_type.unwrap_or("user_statement").to_string()),
            ttl: self.ttl,
            due_in: self.due_in,
            relates,
            agent: agent.map(str::to_string),
            on_conflict: None,
        }
    }

    async fn via_mcp(&self, repo: &Repo) -> String {
        self.via_mcp_with(repo, Vec::new(), None, None).await
    }

    async fn via_mcp_with(
        &self,
        repo: &Repo,
        relates: Vec<RelatesInput>,
        agent: Option<&str>,
        session: Option<&str>,
    ) -> String {
        mdkb::mcp::dispatch::memory_write_impl(
            &repo.handle(),
            &self.as_mcp(relates, agent),
            session,
            false,
        )
        .await
        .expect("memory_write")
    }
}

/// What one write left behind, read back through the store — every column of
/// `memory_entries`, the typed edges, the embedding, the revisions and the
/// markdown projection.
///
/// Wall-clock columns are held as offsets from `created_at`: two writes made a
/// second apart must still compare equal, and a TTL is a duration, not an
/// instant. The projection hash covers bytes that include those instants, so it
/// is reduced to "recorded or not"; the projection text itself is compared with
/// its three timestamp lines (`created_at`, `updated_at`, `due_at`) removed —
/// the offsets above already cover what those lines carry.
#[derive(Debug, PartialEq, Eq)]
struct Row {
    id: String,
    title: String,
    content: String,
    entry_type: String,
    tags: String,
    status: String,
    updated_after_created: bool,
    superseded_by: Option<String>,
    access_count: i64,
    last_accessed: Option<i64>,
    source_path: Option<String>,
    confirmations: i64,
    corrections: i64,
    last_confirmed_at: Option<i64>,
    source_type: String,
    /// `expires_at - created_at`.
    expires_in: Option<i64>,
    /// `due_at - created_at`.
    due_in: Option<i64>,
    created_session: Option<String>,
    created_agent: Option<String>,
    projection_recorded: bool,
    edges: Vec<(String, String, String)>,
    embedded: bool,
    revisions: i64,
    projection: Option<String>,
}

fn row(repo: &Repo, id: &str) -> Row {
    let ctx = Context::open_read_only(&repo.root).expect("open store read-only");
    let conn = &ctx.conn;

    type Scalar = (
        String,
        String,
        String,
        String,
        String,
        String,
        i64,
        i64,
        Option<String>,
        i64,
        Option<i64>,
        Option<String>,
        i64,
        i64,
        Option<i64>,
        String,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
    );
    let s: Scalar = conn
        .query_row(
            "SELECT id, title, content, entry_type, tags, status, created_at, updated_at,
                    superseded_by, access_count, last_accessed, source_path, confirmations,
                    corrections, last_confirmed_at, source_type, expires_at, due_at,
                    created_session, created_agent, projected_at, projected_hash
             FROM memory_entries WHERE id = ?1",
            [id],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get(12)?,
                    r.get(13)?,
                    r.get(14)?,
                    r.get(15)?,
                    r.get(16)?,
                    r.get(17)?,
                    r.get(18)?,
                    r.get(19)?,
                    r.get(20)?,
                    r.get(21)?,
                ))
            },
        )
        .unwrap_or_else(|e| panic!("row {id} must exist in {}: {e}", repo.root.display()));
    let created_at = s.6;

    let edges = mdkb::store::memory_graph::outgoing(conn, id, None)
        .expect("edges")
        .into_iter()
        .map(|e| (e.target_ref, e.target_kind, e.relation))
        .collect();
    let embedded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_embeddings
             WHERE memory_rowid = (SELECT rowid FROM memory_entries WHERE id = ?1)",
            [id],
            |r| r.get(0),
        )
        .expect("embedding count");
    let revisions: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_revisions WHERE memory_id = ?1",
            [id],
            |r| r.get(0),
        )
        .expect("revision count");
    let projection = std::fs::read_to_string(
        repo.root
            .join(".mdkb/memory/entries")
            .join(format!("{id}.md")),
    )
    .ok()
    .map(|md| {
        md.lines()
            .filter(|l| {
                !l.starts_with("created_at:")
                    && !l.starts_with("updated_at:")
                    && !l.starts_with("due_at:")
            })
            .collect::<Vec<_>>()
            .join("\n")
    });

    Row {
        id: s.0,
        title: s.1,
        content: s.2,
        entry_type: s.3,
        tags: s.4,
        status: s.5,
        updated_after_created: s.7 >= created_at,
        superseded_by: s.8,
        access_count: s.9,
        last_accessed: s.10,
        source_path: s.11,
        confirmations: s.12,
        corrections: s.13,
        last_confirmed_at: s.14,
        source_type: s.15,
        expires_in: s.16.map(|t| t - created_at),
        due_in: s.17.map(|t| t - created_at),
        created_session: s.18,
        created_agent: s.19,
        projection_recorded: s.20.is_some() && s.21.is_some(),
        edges,
        embedded: embedded > 0,
        revisions,
        projection,
    }
}

/// Two fresh stores, one write each, one through each door.
async fn rows_after(write: &MemoryInput) -> (Row, Row) {
    let cli_repo = Repo::init("");
    let mcp_repo = Repo::init("");
    write.via_cli(&cli_repo);
    write.via_mcp(&mcp_repo).await;
    (row(&cli_repo, write.id), row(&mcp_repo, write.id))
}

async fn assert_same_row_for(entry_type: &'static str) {
    let mut write = MemoryInput::of(entry_type);
    if entry_type == "reminder" {
        write.due_in = Some(3_600);
    }
    let (cli, mcp) = rows_after(&write).await;
    assert!(
        cli.projection_recorded && cli.projection.is_some(),
        "the fixture must project, or the projection columns compare two blanks: {cli:?}"
    );
    assert_eq!(
        cli, mcp,
        "`mdkb memory add` and `memory_write` must persist the same row for a `{entry_type}`"
    );
}

#[tokio::test]
async fn memory_add_and_memory_write_persist_the_same_row_for_a_topic() {
    assert_same_row_for("topic").await;
}

#[tokio::test]
async fn memory_add_and_memory_write_persist_the_same_row_for_a_problem() {
    assert_same_row_for("problem").await;
}

#[tokio::test]
async fn memory_add_and_memory_write_persist_the_same_row_for_a_decision() {
    assert_same_row_for("decision").await;
}

#[tokio::test]
async fn memory_add_and_memory_write_persist_the_same_row_for_a_reminder() {
    assert_same_row_for("reminder").await;
}

#[tokio::test]
async fn memory_add_and_memory_write_persist_the_same_row_for_a_handoff() {
    assert_same_row_for("handoff").await;
}

/// The one type the two paths disagree on. RED until story 068-db06.
///
/// `mcp/dispatch.rs` gives a prior with no `ttl` the default `PRIOR_TTL_SECS`;
/// `core/memory.rs` gives it `expires_at = NULL`, so a prior written from the
/// CLI never expires. Schema migration v22 retro-dates only the
/// `auto_extracted` ones, so a CLI-written `user_statement` prior stays
/// permanent for good.
#[tokio::test]
async fn memory_add_and_memory_write_persist_the_same_row_for_a_prior() {
    let (cli, mcp) = rows_after(&MemoryInput::of("prior")).await;
    assert_eq!(
        cli.expires_in, mcp.expires_in,
        "CLI prior has no expires_at; MCP sets PRIOR_TTL_SECS — unified by story 068-db06"
    );
    assert_eq!(cli, mcp, "unified by story 068-db06");
}

/// The CLI half of the prior rule on its own, so a regression names the rule
/// and not just the disagreement: a prior with no `--ttl` expires after
/// `PRIOR_TTL_SECS`, whichever door wrote it.
#[test]
fn a_cli_prior_without_ttl_expires_after_the_default_ttl() {
    let repo = Repo::init("");
    MemoryInput::of("prior").via_cli(&repo);
    assert_eq!(
        row(&repo, "parity-entry").expires_in,
        Some(PRIOR_TTL_SECS),
        "a prior written by `mdkb memory add` must carry the default TTL"
    );
}

/// The write-level columns the CLI does expose, all set at once.
#[tokio::test]
async fn an_explicit_ttl_due_time_and_source_type_land_identically() {
    let mut write = MemoryInput::of("reminder");
    write.ttl = Some(7_200);
    write.due_in = Some(600);
    write.source_type = Some("official_docs");
    let (cli, mcp) = rows_after(&write).await;
    assert_eq!(
        cli.expires_in,
        Some(7_200),
        "the fixture ttl must reach the row"
    );
    assert_eq!(cli.source_type, "official_docs");
    assert_eq!(cli, mcp);
}

/// Types are checked one by one above; this pins the list itself, so a new
/// variant fails here until it gets its own parity case.
#[test]
fn every_entry_type_has_a_parity_case() {
    let covered = [
        "topic", "problem", "decision", "reminder", "prior", "handoff",
    ];
    let all: Vec<&str> = EntryType::ALL.iter().map(|t| t.as_str()).collect();
    assert_eq!(
        all, covered,
        "EntryType::ALL changed — add a `memory_add_and_memory_write_persist_the_same_row_for_*` \
         case for the new variant"
    );
}

/// The second write to an id is an update on both surfaces: one revision,
/// new content, `updated_at` moved, provenance kept.
#[tokio::test]
async fn a_rewrite_of_the_same_entry_lands_identically() {
    let first = MemoryInput::of("topic");
    let mut second = first.clone();
    second.title = "Parity entry, revised";
    second.content = "Different words through both doors.";
    second.tags = vec!["parity", "revised"];

    let cli_repo = Repo::init("");
    let mcp_repo = Repo::init("");
    first.via_cli(&cli_repo);
    second.via_cli(&cli_repo);
    first.via_mcp(&mcp_repo).await;
    second.via_mcp(&mcp_repo).await;

    let (cli, mcp) = (row(&cli_repo, first.id), row(&mcp_repo, first.id));
    assert_eq!(
        cli.revisions, 1,
        "the fixture must produce a revision: {cli:?}"
    );
    assert_eq!(cli.content, second.content);
    assert_eq!(cli, mcp);
}

/// RED until story 068-db06.
///
/// A re-write that names a `source_type` is a change of trust level. The CLI
/// applies it (`core/memory.rs`: "only override provenance when the caller
/// explicitly passed --source-type"); `memory_write` never touches
/// `source_type` on an existing row, so the same call leaves the MCP store at
/// the old level.
#[tokio::test]
async fn a_rewrite_with_an_explicit_source_type_lands_identically() {
    let first = MemoryInput::of("topic");
    let mut second = first.clone();
    second.source_type = Some("official_docs");

    let cli_repo = Repo::init("");
    let mcp_repo = Repo::init("");
    first.via_cli(&cli_repo);
    second.via_cli(&cli_repo);
    first.via_mcp(&mcp_repo).await;
    second.via_mcp(&mcp_repo).await;

    let (cli, mcp) = (row(&cli_repo, first.id), row(&mcp_repo, first.id));
    assert_eq!(
        cli.source_type, mcp.source_type,
        "a re-write with an explicit source_type changes provenance through `mdkb memory add` \
         (core/memory.rs) and is ignored by `memory_write` (mcp/dispatch.rs) — unified by \
         story 068-db06"
    );
    assert_eq!(cli, mcp, "unified by story 068-db06");
}

/// RED until story 068-db06.
///
/// `memory_write` records who wrote the entry: the MCP session in
/// `created_session` and the `agent` parameter in `created_agent`. `mdkb memory
/// add` has no `--agent` and no session, so a CLI-written entry has no author
/// — `memory link --agent` can add one afterwards, which is a second command
/// for the same fact.
#[tokio::test]
async fn the_same_authored_write_records_the_same_provenance() {
    let write = MemoryInput::of("decision");
    let mcp_repo = Repo::init("");
    write
        .via_mcp_with(&mcp_repo, Vec::new(), Some("codex"), Some("session-1"))
        .await;
    let mcp = row(&mcp_repo, write.id);
    assert_eq!(
        mcp.created_agent.as_deref(),
        Some("codex"),
        "the fixture must record provenance on the MCP side: {mcp:?}"
    );

    let cli_repo = Repo::init("");
    let out = cli::run(
        &[
            "memory",
            "add",
            write.id,
            "--title",
            write.title,
            "--content",
            write.content,
            "--entry-type",
            write.entry_type,
            "--tags",
            "parity,gate",
            "--agent",
            "codex",
        ],
        &cli_repo.root,
    );
    assert!(
        out.status.success(),
        "`memory_write` records `agent` as created_agent; `mdkb memory add` has no `--agent` \
         and records no author — unified by story 068-db06: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cli = row(&cli_repo, write.id);
    assert_eq!(
        cli.created_agent, mcp.created_agent,
        "unified by story 068-db06"
    );
}

/// RED until story 068-db06.
///
/// `memory_write` takes `relates` and writes the typed edges in the same
/// transaction as the row. `mdkb memory add` has no `--relates`; the edge needs
/// a second command (`memory link`) and a second transaction.
#[tokio::test]
async fn typed_edges_land_on_both_surfaces() {
    // Distinct words on purpose: with model weights present, `memory_write`
    // refuses a second entry that reads like the first ("Near-duplicate entry
    // exists") — a rejection the CLI does not have, and one this test does not
    // encode because it depends on the machine having the model.
    let target = MemoryInput {
        id: "parity-target",
        title: "Edge target",
        content: "A separate note for the edge to point at.",
        ..MemoryInput::of("topic")
    };
    let write = MemoryInput::of("topic");
    let relates = vec![RelatesInput {
        relation: "relates_to".to_string(),
        target: target.id.to_string(),
        target_kind: "memory".to_string(),
    }];

    let mcp_repo = Repo::init("");
    target.via_mcp(&mcp_repo).await;
    write.via_mcp_with(&mcp_repo, relates, None, None).await;
    let mcp = row(&mcp_repo, write.id);
    assert_eq!(
        mcp.edges,
        vec![(
            target.id.to_string(),
            "memory".to_string(),
            "relates_to".to_string()
        )],
        "the fixture must write an edge on the MCP side: {mcp:?}"
    );

    let cli_repo = Repo::init("");
    target.via_cli(&cli_repo);
    let out = cli::run(
        &[
            "memory",
            "add",
            write.id,
            "--title",
            write.title,
            "--content",
            write.content,
            "--tags",
            "parity,gate",
            "--relates",
            "relates_to:parity-target",
        ],
        &cli_repo.root,
    );
    assert!(
        out.status.success(),
        "`memory_write` writes typed edges from `relates` in the entry's transaction; \
         `mdkb memory add` has no `--relates` — unified by story 068-db06: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cli = row(&cli_repo, write.id);
    assert_eq!(cli.edges, mcp.edges, "unified by story 068-db06");
}

/// RED until story 068-db06.
///
/// The inputs `memory_write` accepts that `mdkb memory add` cannot spell at all.
/// One assertion, one list: the message names every missing flag rather than
/// the first, so the unification story can read its scope off the failure.
#[test]
fn every_memory_write_input_has_a_cli_spelling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let help = text(&run(&["memory", "add", "--help"], dir.path()));
    let missing: Vec<&str> = [
        ("agent", "--agent"),
        ("relates", "--relates"),
        ("on_conflict", "--on-conflict"),
        ("dry_run", "--dry-run"),
    ]
    .into_iter()
    .filter(|(_, flag)| !help.contains(flag))
    .map(|(param, _)| param)
    .collect();
    assert!(
        missing.is_empty(),
        "`memory_write` parameters with no `mdkb memory add` flag: {missing:?} — the two \
         paths take different inputs, so they cannot persist the same row for them; \
         unified by story 068-db06.\n{help}"
    );
}

// ── Search (story 067-5ab6) ──────────────────────────────────────────────────

/// The result identifiers in the order a surface printed them.
///
/// The two renderers differ on purpose — the MCP one carries `get()` hints and
/// token estimates for an agent, the CLI one has `--format` — so bytes are not
/// the contract for these scopes. The hits are: which documents, entries or
/// symbols came back, and in which order. Docs and memory print `[id]` at the
/// start of each result line (`- [id]` on the MCP memory renderer); symbols
/// print `sym#N`.
fn bracketed_ids(rendered: &str) -> Vec<String> {
    rendered
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let line = line.strip_prefix("- ").unwrap_or(line);
            let rest = line.strip_prefix('[')?;
            let end = rest.find(']')?;
            Some(rest[..end].to_string())
        })
        .collect()
}

fn symbol_ids(rendered: &str) -> Vec<String> {
    rendered
        .split_whitespace()
        .filter_map(|word| word.strip_prefix("sym#"))
        .map(str::to_string)
        .collect()
}

/// A repository with something to find under every scope.
///
/// Memory embeddings are switched off so the memory scope compares the BM25
/// legs of both surfaces: `memory_write` embeds regardless of that setting and
/// the MCP search would then add a vector leg the CLI does not have, and the
/// test would depend on whether this machine has model weights.
fn search_repo() -> Repo {
    let repo =
        Repo::init("[search]\nauto_embed_memory = false\n\n[code.duplication]\nsemantic = false\n");
    std::fs::create_dir_all(repo.root.join("docs")).expect("mkdir docs");
    std::fs::write(
        repo.root.join("docs/guide.md"),
        "# Setup guide\n\nHow to set up the parity fixture.\n",
    )
    .expect("write guide");
    std::fs::write(
        repo.root.join("docs/faq.md"),
        "# FAQ\n\nQuestions about setup and teardown.\n",
    )
    .expect("write faq");
    run(&["update"], &repo.root);

    for (id, title, content) in [
        (
            "mem-one",
            "Parity note one",
            "the fixture word appears here",
        ),
        (
            "mem-two",
            "Parity note two",
            "the fixture word appears here as well",
        ),
    ] {
        run(
            &["memory", "add", id, "--title", title, "--content", content],
            &repo.root,
        );
    }

    std::fs::create_dir_all(repo.root.join("src")).expect("mkdir src");
    std::fs::write(
        repo.root.join("src/lib.rs"),
        "pub fn greet(name: &str) -> String {\n    format!(\"hello {name}\")\n}\n\
         pub fn greet_all(names: &[&str]) -> Vec<String> {\n    names.iter().map(|n| greet(n)).collect()\n}\n",
    )
    .expect("write lib.rs");
    run(&["code", "index", "src"], &repo.root);
    repo
}

async fn mcp_search(repo: &Repo, scope: &str, query: &str) -> (String, usize) {
    mdkb::mcp::dispatch::search_impl(&repo.handle(), &params(scope, query, None))
        .await
        .unwrap_or_else(|e| panic!("search scope={scope}: {e:?}"))
}

#[tokio::test]
async fn both_surfaces_return_the_same_documents() {
    let repo = search_repo();
    let cli = text(&run(&["search", "setup", "--scope", "docs"], &repo.root));
    let (mcp, count) = mcp_search(&repo, "docs", "setup").await;

    let cli_ids = bracketed_ids(&cli);
    assert_eq!(cli_ids.len(), 2, "both fixture docs mention setup:\n{cli}");
    assert_eq!(count, cli_ids.len(), "MCP said:\n{mcp}");
    assert_eq!(
        bracketed_ids(&mcp),
        cli_ids,
        "the same documents in the same order\nCLI:\n{cli}\nMCP:\n{mcp}"
    );
}

#[tokio::test]
async fn both_surfaces_return_the_same_memory_entries() {
    let repo = search_repo();
    let cli = text(&run(
        &["search", "fixture", "--scope", "memory"],
        &repo.root,
    ));
    let (mcp, count) = mcp_search(&repo, "memory", "fixture").await;

    let cli_ids = bracketed_ids(&cli);
    assert_eq!(
        cli_ids.len(),
        2,
        "both fixture entries carry the word:\n{cli}"
    );
    assert_eq!(count, cli_ids.len(), "MCP said:\n{mcp}");
    assert_eq!(
        bracketed_ids(&mcp),
        cli_ids,
        "the same entries in the same order\nCLI:\n{cli}\nMCP:\n{mcp}"
    );
}

#[tokio::test]
async fn both_surfaces_return_the_same_symbols() {
    let repo = search_repo();
    let cli = text(&run(&["search", "gree", "--scope", "symbols"], &repo.root));
    let (mcp, count) = mcp_search(&repo, "symbols", "gree").await;

    let cli_ids = symbol_ids(&cli);
    assert_eq!(
        cli_ids.len(),
        2,
        "both fixture functions match `gree`:\n{cli}"
    );
    assert_eq!(count, cli_ids.len(), "MCP said:\n{mcp}");
    assert_eq!(
        symbol_ids(&mcp),
        cli_ids,
        "the same symbols in the same order\nCLI:\n{cli}\nMCP:\n{mcp}"
    );
}

/// A word nothing contains. Memory and symbols answer with nothing on both
/// sides. Docs is the interesting one: when this machine has model weights the
/// hybrid search still ranks every document by vector distance, so a lexical
/// miss is two hits — on both surfaces, in the same order, or the two have
/// different ideas of what a search is.
#[tokio::test]
async fn both_surfaces_agree_on_a_lexical_miss() {
    let repo = search_repo();
    for scope in ["memory", "symbols"] {
        let cli = text(&run(&["search", "zzyzx", "--scope", scope], &repo.root));
        let (mcp, count) = mcp_search(&repo, scope, "zzyzx").await;
        assert_eq!(count, 0, "scope={scope}: MCP said:\n{mcp}");
        assert!(
            bracketed_ids(&cli).is_empty() && symbol_ids(&cli).is_empty(),
            "scope={scope}: CLI said:\n{cli}"
        );
    }

    let cli = text(&run(&["search", "zzyzx", "--scope", "docs"], &repo.root));
    let (mcp, count) = mcp_search(&repo, "docs", "zzyzx").await;
    let cli_ids = bracketed_ids(&cli);
    assert_eq!(count, cli_ids.len(), "CLI:\n{cli}\nMCP:\n{mcp}");
    assert_eq!(bracketed_ids(&mcp), cli_ids, "CLI:\n{cli}\nMCP:\n{mcp}");
}

// ── Duplicates (story 049-11af) ──────────────────────────────────────────────
//
// `mdkb dup` and `search(scope="duplicates")` are one audit behind two names.
// The duplication report is deliberately NOT a thirteenth MCP tool: every tool
// schema is charged on every turn, and an audit that runs occasionally cannot
// justify that. It rides the existing `search` tool as one more scope value
// instead. The risk that buys is drift — two call sites reading the same index
// through different options and answering differently — so these cases
// require byte equality.

/// A body with enough AST nodes to clear `MIN_BODY_NODES`, parameterised so two
/// copies differ only in the names — which is what the structural pass is for.
fn duplicated_body(name: &str, acc: &str) -> String {
    format!(
        "pub fn {name}(items: &[u32]) -> u32 {{\n\
         \x20   let mut {acc} = 0;\n\
         \x20   for item in items {{\n\
         \x20       if item % 2 == 0 {{\n\
         \x20           {acc} += item * 2;\n\
         \x20       }} else {{\n\
         \x20           {acc} -= item;\n\
         \x20       }}\n\
         \x20   }}\n\
         \x20   {acc}\n\
         }}\n"
    )
}

/// An indexed repository holding one copy-paste pair.
///
/// The model is switched off in the repository config: the structural half of
/// the pass is what both surfaces share, it needs no weights, and a test that
/// downloads a model is a test that fails on a train.
fn dup_repo() -> Repo {
    let repo = Repo::init("[code.duplication]\nsemantic = false\n");
    assert!(
        !Config::load_or_default(repo.root.join(".mdkb/config.toml"))
            .code
            .duplication
            .semantic,
        "the fixture must not reach for a model"
    );
    std::fs::create_dir_all(repo.root.join("src")).expect("mkdir src");
    std::fs::write(repo.root.join("src/a.rs"), duplicated_body("total", "sum"))
        .expect("write a.rs");
    std::fs::write(
        repo.root.join("src/b.rs"),
        duplicated_body("aggregate", "acc"),
    )
    .expect("write b.rs");
    run(&["code", "index", "src"], &repo.root);
    repo
}

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn both_surfaces_report_the_same_clusters_for_the_same_repository() {
    let repo = dup_repo();

    let cli = text(&run(&["dup"], &repo.root));
    let (mcp, count) =
        mdkb::mcp::dispatch::search_impl(&repo.handle(), &params("duplicates", "", None))
            .await
            .expect("duplicates scope");

    // The fixture must actually find something, or equality is the equality of
    // two empty reports and this test cannot fail.
    assert!(
        cli.contains("total") && cli.contains("aggregate"),
        "the fixture pair must cluster; CLI said:\n{cli}"
    );
    assert_eq!(
        count, 1,
        "one cluster, from the one copy-paste pair:\n{mcp}"
    );
    assert_eq!(
        mcp, cli,
        "the two surfaces must render the same audit, byte for byte"
    );
}

#[tokio::test]
async fn the_file_option_scopes_both_surfaces_the_same_way() {
    let repo = dup_repo();

    // Half the pair is out of scope, so there is nothing left to pair with.
    let cli = text(&run(&["dup", "--file", "src/a.rs"], &repo.root));
    let (mcp, count) = mdkb::mcp::dispatch::search_impl(
        &repo.handle(),
        &params("duplicates", "", Some("src/a.rs")),
    )
    .await
    .expect("duplicates scope");

    assert_eq!(count, 0, "one file cannot duplicate itself:\n{mcp}");
    assert!(
        !cli.contains("aggregate"),
        "the CLI must have honoured the same narrowing:\n{cli}"
    );
    assert_eq!(mcp, cli, "and both must say so identically");
}

/// Review mode across both surfaces, on a real git history.
///
/// `src/b.rs` is committed; `src/a.rs` is the change under review. The cluster
/// must survive, because what `a.rs` duplicated is precisely the code that did
/// not change — a candidate filter would have dropped it. Both surfaces must
/// say so identically.
#[tokio::test]
async fn review_mode_scopes_both_surfaces_to_what_the_ref_changed() {
    let repo = dup_repo();
    git(&repo.root, &["init", "-q", "-b", "main"]);
    git(&repo.root, &["add", "src/b.rs"]);
    git(&repo.root, &["commit", "-q", "-m", "b"]);

    // `src/a.rs` is now the only path `git diff --name-only HEAD` reports.
    let cli = text(&run(&["dup", "--since", "HEAD"], &repo.root));
    let mut p = params("duplicates", "", None);
    p.since = Some("HEAD".to_string());
    let (mcp, count) = mdkb::mcp::dispatch::search_impl(&repo.handle(), &p)
        .await
        .expect("duplicates scope");

    assert_eq!(
        count, 1,
        "the cluster touches the changed file and must survive:\n{mcp}"
    );
    assert!(
        cli.contains("aggregate"),
        "and the unchanged member is still reported — that is the finding:\n{cli}"
    );
    assert_eq!(mcp, cli, "both surfaces must narrow identically");
}

/// The other half of the same contract: a ref that changed nothing reports
/// nothing, rather than falling back to the whole-repository sweep.
#[tokio::test]
async fn review_mode_reports_nothing_when_the_ref_changed_nothing() {
    let repo = dup_repo();
    git(&repo.root, &["init", "-q", "-b", "main"]);
    git(&repo.root, &["add", "src/a.rs", "src/b.rs"]);
    git(&repo.root, &["commit", "-q", "-m", "both"]);

    let cli = text(&run(&["dup", "--since", "HEAD"], &repo.root));
    let mut p = params("duplicates", "", None);
    p.since = Some("HEAD".to_string());
    let (mcp, count) = mdkb::mcp::dispatch::search_impl(&repo.handle(), &p)
        .await
        .expect("duplicates scope");

    assert_eq!(
        count, 0,
        "nothing changed, so nothing is under review:\n{mcp}"
    );
    assert!(
        !cli.contains("aggregate"),
        "and the CLI must not fall back to the full sweep:\n{cli}"
    );
    assert_eq!(mcp, cli);
}

#[tokio::test]
async fn an_unindexed_repository_is_reported_rather_than_refused_on_both_surfaces() {
    let repo = Repo::init("[code.duplication]\nenabled = false\n");

    let out = cli::run(&["dup"], &repo.root);
    assert!(
        out.status.success(),
        "an audit with nothing to audit is not a failure: exit={:?}",
        out.status.code()
    );
    let cli = text(&out);
    assert!(cli.contains("No code index"), "CLI said:\n{cli}");

    let (mcp, count) =
        mdkb::mcp::dispatch::search_impl(&repo.handle(), &params("duplicates", "", None))
            .await
            .expect("an absent index is not an MCP error either");
    assert_eq!(count, 0);
    assert_eq!(mcp, cli);
}

/// The scope is rejected per-repo only. Fanning a duplication audit across
/// every registered repo would open every code index at once, which is the
/// reason `code` and `symbols` are already refused here.
#[tokio::test]
async fn the_duplicates_scope_is_refused_across_repositories() {
    let repo = dup_repo();
    let handles = [repo.handle()];

    let err =
        mdkb::mcp::dispatch::cross_repo_search_impl(&handles, &params("duplicates", "", None))
            .await
            .expect_err("cross-repo duplicates must be refused");
    let msg = err.to_string();
    assert!(msg.contains("duplicates"), "msg: {msg}");
    assert!(msg.contains("Specify a root"), "msg: {msg}");
}

/// An unknown scope must name the ones that exist, including the new one — an
/// agent that guessed wrong learns the whole set from the rejection.
#[tokio::test]
async fn an_unknown_scope_names_every_valid_scope() {
    let repo = dup_repo();

    let err = mdkb::mcp::dispatch::search_impl(&repo.handle(), &params("duplicate", "", None))
        .await
        .expect_err("a near-miss must still be rejected");
    let msg = err.to_string();
    for scope in ["docs", "memory", "code", "symbols", "duplicates"] {
        assert!(msg.contains(scope), "rejection must name `{scope}`: {msg}");
    }
    assert!(msg.contains("duplicate'"), "and the value rejected: {msg}");
}

/// The whole point of the scope: no thirteenth tool schema on every turn.
#[test]
fn the_duplication_audit_added_no_mcp_tool() {
    let advertised = mdkb::mcp::server::advertised_tool_names();
    assert_eq!(
        advertised.len(),
        12,
        "the MCP tool count is a budget, not an accident: every schema is \
         charged on every turn of every conversation. Duplication rides \
         `search` as a scope for that reason. If a thirteenth tool is genuinely \
         worth it, raise this number deliberately — do not let it drift: \
         {advertised:?}"
    );
    assert!(
        !advertised.iter().any(|t| t.contains("dup")),
        "and it must not be spelled as a tool: {advertised:?}"
    );
}
