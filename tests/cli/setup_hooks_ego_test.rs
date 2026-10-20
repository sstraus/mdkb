//! Integration tests for `mdkb setup hooks ego` — ego `config.toml` writer.

use std::fs;
use std::sync::MutexGuard;

use mdkb::cli::setup_ego::{EgoSetupOutcome, handle_setup_hooks_ego};
use tempfile::TempDir;

use super::common::env_lock;

const KEY: &str = "mdkb_prompt_command";

/// Point EGO_HOME at a scratch dir so Boss's real ~/.ego is never touched.
fn isolated_ego_home() -> (MutexGuard<'static, ()>, TempDir) {
    let guard = env_lock();
    let ego_home = tempfile::tempdir().expect("tempdir ego home");
    unsafe {
        std::env::set_var("EGO_HOME", ego_home.path());
        std::env::set_var("HOME", ego_home.path().join("unused-home"));
        std::env::set_var("MDKB_BINARY_OVERRIDE", env!("CARGO_BIN_EXE_mdkb"));
    }
    (guard, ego_home)
}

fn config_path(ego_home: &TempDir) -> std::path::PathBuf {
    ego_home.path().join("config.toml")
}

fn key_value(path: &std::path::Path) -> Option<String> {
    let doc: toml::Value = fs::read_to_string(path).unwrap().parse().unwrap();
    doc.get(KEY).and_then(|v| v.as_str()).map(str::to_owned)
}

const USER_CONFIG: &str = "# my ego settings\nmodel = \"ollama/qwen3\"   # keep this comment\n\n[ui.statusline]\ntemplate = \"{{ prompt }}\"\n\n# trailing note\n[mcp.servers.docs]\ncommand = \"/usr/bin/docs\"\n";

/// Catches: the installer rewrites or drops user config.
#[test]
fn unrelated_tables_and_comments_stay_byte_identical() {
    let (_guard, home) = isolated_ego_home();
    fs::write(config_path(&home), USER_CONFIG).unwrap();

    let result = handle_setup_hooks_ego(false, false).expect("setup ok");
    assert_eq!(result.outcome, EgoSetupOutcome::Written);

    let written = fs::read_to_string(config_path(&home)).unwrap();
    let line = format!("{KEY} = \"{}\"\n", env!("CARGO_BIN_EXE_mdkb"));
    assert_eq!(written.replacen(&line, "", 1), USER_CONFIG);
    assert_eq!(
        key_value(&config_path(&home)).as_deref(),
        Some(env!("CARGO_BIN_EXE_mdkb"))
    );
}

/// Catches: a second run appends a duplicate key (invalid TOML) or rewrites the file.
#[test]
fn second_run_leaves_one_entry_and_the_file_untouched() {
    let (_guard, home) = isolated_ego_home();
    fs::write(config_path(&home), USER_CONFIG).unwrap();

    handle_setup_hooks_ego(false, false).unwrap();
    let first = fs::read_to_string(config_path(&home)).unwrap();
    let result = handle_setup_hooks_ego(false, false).unwrap();
    let second = fs::read_to_string(config_path(&home)).unwrap();

    assert_eq!(result.outcome, EgoSetupOutcome::Unchanged);
    assert_eq!(first, second);
    assert_eq!(second.matches(KEY).count(), 1);
}

/// Catches: setup clobbers a path the user chose.
#[test]
fn user_edited_value_survives_without_force() {
    let (_guard, home) = isolated_ego_home();
    let edited = format!("{KEY} = \"/opt/custom/mdkb\"\n[ui]\nx = 1\n");
    fs::write(config_path(&home), &edited).unwrap();

    let result = handle_setup_hooks_ego(false, false).unwrap();

    assert_eq!(
        result.outcome,
        EgoSetupOutcome::KeptExisting("/opt/custom/mdkb".into())
    );
    assert_eq!(fs::read_to_string(config_path(&home)).unwrap(), edited);
}

/// Catches: --force is ignored, or it rewrites more than the one value.
#[test]
fn force_replaces_only_the_key_value() {
    let (_guard, home) = isolated_ego_home();
    fs::write(
        config_path(&home),
        format!("# top\n{KEY} = \"/opt/custom/mdkb\"  # old\n[ui]\nx = 1\n"),
    )
    .unwrap();

    let result = handle_setup_hooks_ego(true, false).unwrap();

    assert_eq!(result.outcome, EgoSetupOutcome::Written);
    let written = fs::read_to_string(config_path(&home)).unwrap();
    assert!(written.starts_with("# top\n"), "{written}");
    assert!(written.ends_with("[ui]\nx = 1\n"), "{written}");
    assert_eq!(written.matches(KEY).count(), 1);
    assert_eq!(
        key_value(&config_path(&home)).as_deref(),
        Some(env!("CARGO_BIN_EXE_mdkb"))
    );
}

/// Catches: a missing config file (fresh ego install) errors instead of being created.
#[test]
fn absent_config_file_is_created_with_only_the_key() {
    let (_guard, home) = isolated_ego_home();

    handle_setup_hooks_ego(false, false).unwrap();

    assert_eq!(
        fs::read_to_string(config_path(&home)).unwrap(),
        format!("{KEY} = \"{}\"\n", env!("CARGO_BIN_EXE_mdkb"))
    );
}

/// Catches: --dry-run writes the file.
#[test]
fn dry_run_writes_nothing() {
    let (_guard, home) = isolated_ego_home();

    let result = handle_setup_hooks_ego(false, true).unwrap();

    assert!(result.dry_run);
    assert!(result.merged_toml.contains(KEY));
    assert!(!config_path(&home).exists());
}

/// Catches: running setup from a build registers an ephemeral binary instead of the installed one.
#[test]
fn setup_from_build_registers_installed_binary_not_build_artifact() {
    let root = tempfile::tempdir().unwrap();
    let installed_dir = root.path().join("installed");
    fs::create_dir(&installed_dir).unwrap();
    let installed = installed_dir.join(if cfg!(windows) { "mdkb.exe" } else { "mdkb" });
    // Use the real executable, not a mock command, as the installed PATH entry.
    fs::copy(env!("CARGO_BIN_EXE_mdkb"), &installed).unwrap();
    let mut paths = vec![installed_dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let home = root.path().join("ego");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["setup", "hooks", "ego"])
        .env("EGO_HOME", &home)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env_remove("MDKB_BINARY_OVERRIDE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        key_value(&home.join("config.toml")),
        Some(installed.to_string_lossy().into_owned()),
        "setup must register the installed PATH binary, not the current build artifact"
    );
}

/// Catches: unparsable user config is overwritten instead of reported.
#[test]
fn unparsable_config_is_an_error_and_left_alone() {
    let (_guard, home) = isolated_ego_home();
    fs::write(config_path(&home), "this = = broken").unwrap();

    assert!(handle_setup_hooks_ego(false, false).is_err());
    assert_eq!(
        fs::read_to_string(config_path(&home)).unwrap(),
        "this = = broken"
    );
}
