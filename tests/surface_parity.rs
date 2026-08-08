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

use mdkb::core::surface::{SURFACE_MAP, SurfaceEntry};

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
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
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
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .arg("cheatsheet")
        .output()
        .expect("run mdkb cheatsheet");
    assert!(out.status.success(), "`mdkb cheatsheet` must exit 0");
    let text = String::from_utf8_lossy(&out.stdout);

    // The cheatsheet substitutes the real binary path, so each command line
    // starts with it rather than a placeholder. Checked explicitly: an earlier
    // version of this test looked for a `{0} ` prefix that the output never
    // contains, so it scanned nothing and passed for the wrong reason.
    let bin = env!("CARGO_BIN_EXE_mdkb");
    let mut checked = 0usize;
    let mut broken = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix(bin) else {
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
    use mdkb::store::memory::EntryType;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().canonicalize().expect("canonicalize");
    mdkb::cli::handlers::handle_init(&root).expect("init");

    // CLI surface: clap rejects before dispatch.
    let cli = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
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
