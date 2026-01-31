//! Integration tests for `mdkb setup mcp codex` — ~/.codex/config.toml MCP registration.

use std::fs;
use std::sync::MutexGuard;

use mdkb::cli::setup::handle_setup_mcp_codex;
use tempfile::TempDir;

use super::common::env_lock;

/// Build a fresh temp project root with $HOME pointed at a sibling dir so we
/// don't touch the real ~/.codex. Returns (guard, project, home) — keep guard alive.
fn isolated_project() -> (MutexGuard<'static, ()>, TempDir, TempDir) {
    let guard = env_lock();
    let project = tempfile::tempdir().expect("tempdir project");
    let home = tempfile::tempdir().expect("tempdir home");
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("MDKB_BINARY_OVERRIDE", env!("CARGO_BIN_EXE_mdkb"));
    }
    // Pre-create ~/.codex so setup doesn't fail the "not installed" check.
    fs::create_dir_all(home.path().join(".codex")).expect("mkdir .codex");
    (guard, project, home)
}

fn codex_config_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".codex").join("config.toml")
}

fn read_config(path: &std::path::Path) -> toml_edit::DocumentMut {
    let raw = fs::read_to_string(path).expect("config.toml exists");
    raw.parse::<toml_edit::DocumentMut>().expect("valid TOML")
}

#[test]
fn fresh_config_gets_mdkb_mcp_server_entry() {
    let (_guard, _project, home) = isolated_project();

    let result = handle_setup_mcp_codex(false).expect("setup mcp codex ok");
    assert!(result.success);
    assert!(!result.dry_run);

    let path = codex_config_path(home.path());
    assert!(path.exists(), "config.toml must be written");

    let doc = read_config(&path);
    let mdkb = doc
        .get("mcp_servers")
        .and_then(|m| m.get("mdkb"))
        .and_then(|v| v.as_table_like())
        .expect("[mcp_servers.mdkb] must exist");

    let command = mdkb.get("command").and_then(|v| v.as_str()).unwrap_or("");
    assert!(!command.is_empty(), "command must be set (got empty)");

    let args = mdkb
        .get("args")
        .and_then(|v| v.as_array())
        .expect("args array present");
    let args_vec: Vec<&str> = args.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(args_vec, vec!["mcp"]);
}

#[test]
fn rerunning_is_idempotent() {
    let (_guard, _project, home) = isolated_project();

    handle_setup_mcp_codex(false).expect("first run");
    let first = fs::read_to_string(codex_config_path(home.path())).expect("first contents");
    handle_setup_mcp_codex(false).expect("second run");
    let second = fs::read_to_string(codex_config_path(home.path())).expect("second contents");
    handle_setup_mcp_codex(false).expect("third run");
    let third = fs::read_to_string(codex_config_path(home.path())).expect("third contents");

    assert_eq!(first, second, "second run must produce zero diff");
    assert_eq!(second, third, "third run must produce zero diff");
}

#[test]
fn preserves_other_mcp_servers_and_unrelated_sections() {
    let (_guard, _project, home) = isolated_project();

    let path = codex_config_path(home.path());
    let preexisting = r#"# Codex config — do not delete this comment
model = "gpt-5"

[mcp_servers.serena]
command = "uvx"
args = ["serena", "mcp"]

[experimental]
feature_flag = true
"#;
    fs::write(&path, preexisting).expect("write preexisting config");

    handle_setup_mcp_codex(false).expect("setup mcp codex ok");

    let raw = fs::read_to_string(&path).expect("read merged");
    assert!(
        raw.contains("# Codex config — do not delete this comment"),
        "top-level comment must be preserved: {raw}"
    );
    assert!(
        raw.contains("model = \"gpt-5\""),
        "unrelated top-level keys must survive: {raw}"
    );

    let doc = raw.parse::<toml_edit::DocumentMut>().expect("valid TOML");

    let serena = doc
        .get("mcp_servers")
        .and_then(|m| m.get("serena"))
        .and_then(|v| v.as_table_like())
        .expect("serena server must survive");
    assert_eq!(
        serena.get("command").and_then(|v| v.as_str()),
        Some("uvx"),
        "serena command must be unchanged"
    );

    let mdkb = doc
        .get("mcp_servers")
        .and_then(|m| m.get("mdkb"))
        .and_then(|v| v.as_table_like())
        .expect("mdkb server must be added");
    assert!(mdkb.get("command").and_then(|v| v.as_str()).is_some());

    let experimental = doc
        .get("experimental")
        .and_then(|v| v.as_table_like())
        .expect("experimental section must survive");
    assert_eq!(
        experimental.get("feature_flag").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[test]
fn dry_run_does_not_write_file() {
    let (_guard, _project, home) = isolated_project();

    let result = handle_setup_mcp_codex(true).expect("dry run ok");
    assert!(result.dry_run);
    assert!(result.success);

    assert!(
        !codex_config_path(home.path()).exists(),
        "dry-run must not create config.toml"
    );
    assert!(
        result.merged_toml.contains("[mcp_servers.mdkb]"),
        "merged_toml must include mdkb section: {}",
        result.merged_toml
    );
    assert!(
        result.merged_toml.contains("serve"),
        "merged_toml must include serve arg: {}",
        result.merged_toml
    );
}

#[test]
fn errors_when_codex_dir_missing() {
    // Do NOT use isolated_project (which creates .codex). Build a home without .codex.
    let _guard = env_lock();
    let home = tempfile::tempdir().expect("tempdir home");
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("MDKB_BINARY_OVERRIDE", env!("CARGO_BIN_EXE_mdkb"));
    }

    let result = handle_setup_mcp_codex(false);
    assert!(
        result.is_err(),
        "missing ~/.codex must surface as an error (Codex not installed)"
    );
}

#[test]
fn rerunning_replaces_stale_mdkb_command() {
    let (_guard, _project, home) = isolated_project();

    let path = codex_config_path(home.path());
    // Pre-seed with a stale mdkb entry pointing at a non-existent binary.
    let stale = r#"[mcp_servers.mdkb]
command = "/tmp/old-mdkb-path"
args = ["old-arg"]
"#;
    fs::write(&path, stale).expect("write stale config");

    handle_setup_mcp_codex(false).expect("setup mcp codex ok");

    let doc = read_config(&path);
    let mdkb = doc
        .get("mcp_servers")
        .and_then(|m| m.get("mdkb"))
        .and_then(|v| v.as_table_like())
        .expect("mdkb section exists");
    assert_ne!(
        mdkb.get("command").and_then(|v| v.as_str()),
        Some("/tmp/old-mdkb-path"),
        "stale command must be replaced with current binary path"
    );
    let args: Vec<&str> = mdkb
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(args, vec!["mcp"], "args must be reset to [\"mcp\"]");
}
