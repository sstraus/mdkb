//! Integration tests for `mdkb setup hooks codex` — ~/.codex/hooks.json writer.

use std::fs;
use std::sync::MutexGuard;

use mdkb::cli::setup::{HOOK_EVENTS, handle_setup_hooks_codex};
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
    (guard, project, home)
}

fn codex_hooks_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".codex").join("hooks.json")
}

fn codex_config_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".codex").join("config.toml")
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let raw = fs::read_to_string(path).expect("hooks file exists");
    serde_json::from_str(&raw).expect("hooks is valid JSON")
}

fn mdkb_entries<'a>(value: &'a serde_json::Value, event: &str) -> Vec<&'a serde_json::Value> {
    value
        .get("hooks")
        .and_then(|h| h.get(event))
        .and_then(|arr| arr.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|item| item.get("_managedBy").and_then(|v| v.as_str()) == Some("mdkb"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn fresh_hooks_file_gets_managed_entries_for_each_event() {
    let (_guard, _project, home) = isolated_project();

    let result = handle_setup_hooks_codex("", false).expect("setup hooks codex ok");
    assert!(result.success);
    assert_eq!(result.events_registered.len(), HOOK_EVENTS.len());
    assert!(result.events_skipped.is_empty());
    assert!(!result.dry_run);

    let path = codex_hooks_path(home.path());
    assert!(path.exists(), "hooks.json must be written");

    let v = read_json(&path);
    for (event_name, ..) in HOOK_EVENTS {
        let managed = mdkb_entries(&v, event_name);
        assert_eq!(
            managed.len(),
            1,
            "event {event_name} must have exactly one mdkb-managed entry"
        );
        let cmd = managed[0]
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|a| a.first())
            .and_then(|e| e.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or_default();
        assert!(
            cmd.contains("hook "),
            "entry for {event_name} must include `hook` subcommand, got: {cmd}"
        );
    }
}

#[test]
fn rerunning_codex_is_idempotent() {
    let (_guard, _project, _home) = isolated_project();

    handle_setup_hooks_codex("", false).expect("first run");
    handle_setup_hooks_codex("", false).expect("second run");
    handle_setup_hooks_codex("", false).expect("third run");

    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let v = read_json(&codex_hooks_path(&home));
    for (event_name, ..) in HOOK_EVENTS {
        let managed = mdkb_entries(&v, event_name);
        assert_eq!(
            managed.len(),
            1,
            "event {event_name} must stay at exactly one mdkb entry after 3 runs"
        );
    }
}

#[test]
fn disable_flag_skips_named_events() {
    let (_guard, _project, _home) = isolated_project();

    let result =
        handle_setup_hooks_codex("session-start,post-tool-use", false).expect("setup hooks codex");
    assert_eq!(result.events_registered.len(), 3);
    assert!(
        result
            .events_registered
            .contains(&"UserPromptSubmit".to_string())
    );
    assert!(result.events_registered.contains(&"PreToolUse".to_string()));
    assert!(result.events_registered.contains(&"Stop".to_string()));
    assert_eq!(result.events_skipped.len(), 2);
    assert!(result.events_skipped.contains(&"SessionStart".to_string()));
    assert!(result.events_skipped.contains(&"PostToolUse".to_string()));

    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let v = read_json(&codex_hooks_path(&home));
    assert!(mdkb_entries(&v, "SessionStart").is_empty());
    assert!(mdkb_entries(&v, "PostToolUse").is_empty());
    assert_eq!(mdkb_entries(&v, "UserPromptSubmit").len(), 1);
    assert_eq!(mdkb_entries(&v, "PreToolUse").len(), 1);
    assert_eq!(mdkb_entries(&v, "Stop").len(), 1);
}

#[test]
fn dry_run_codex_does_not_write_file() {
    let (_guard, _project, home) = isolated_project();

    let result = handle_setup_hooks_codex("", true).expect("dry run ok");
    assert!(result.dry_run);
    assert!(result.success);
    assert_eq!(result.events_registered.len(), HOOK_EVENTS.len());

    assert!(
        !codex_hooks_path(home.path()).exists(),
        "dry-run must not create hooks.json"
    );
    assert!(result.merged_json.is_object());
    assert!(result.merged_json.get("hooks").is_some());
}

#[test]
fn preserves_non_mdkb_codex_hook_entries() {
    let (_guard, _project, home) = isolated_project();

    let hooks_path = codex_hooks_path(home.path());
    fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();

    let preexisting = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "_managedBy": "some-other-tool",
                    "hooks": [{"type": "command", "command": "echo other"}]
                }
            ],
            "PostToolUse": [
                {
                    "hooks": [{"type": "command", "command": "echo untagged"}]
                }
            ]
        }
    });
    fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&preexisting).unwrap(),
    )
    .unwrap();

    handle_setup_hooks_codex("", false).expect("setup hooks codex ok");

    let v = read_json(&hooks_path);

    let other_tool_count = v
        .get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|i| i.get("_managedBy").and_then(|v| v.as_str()) == Some("some-other-tool"))
                .count()
        })
        .unwrap_or(0);
    assert_eq!(other_tool_count, 1, "other-tool entry must survive");
    assert_eq!(mdkb_entries(&v, "SessionStart").len(), 1);

    let untagged_count = v
        .get("hooks")
        .and_then(|h| h.get("PostToolUse"))
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|i| {
                    i.get("_managedBy").is_none()
                        && i.get("hooks")
                            .and_then(|h| h.as_array())
                            .and_then(|a| a.first())
                            .and_then(|e| e.get("command"))
                            .and_then(|c| c.as_str())
                            == Some("echo untagged")
                })
                .count()
        })
        .unwrap_or(0);
    assert_eq!(untagged_count, 1, "untagged entry must survive");
}

#[test]
fn reports_codex_hooks_flag_missing_when_config_absent() {
    let (_guard, _project, _home) = isolated_project();

    let result = handle_setup_hooks_codex("", false).expect("setup hooks codex ok");
    assert!(
        !result.codex_hooks_flag_present,
        "missing config.toml must report codex_hooks flag as absent"
    );
}

#[test]
fn reports_codex_hooks_flag_present_when_configured() {
    let (_guard, _project, home) = isolated_project();

    let cfg = codex_config_path(home.path());
    fs::create_dir_all(cfg.parent().unwrap()).unwrap();
    fs::write(&cfg, "codex_hooks = true\n").unwrap();

    let result = handle_setup_hooks_codex("", false).expect("setup hooks codex ok");
    assert!(
        result.codex_hooks_flag_present,
        "config.toml with codex_hooks=true must report flag as present"
    );
}

/// Codex-side: same story 016 contract — daemon path first, `MDKB_NO_DAEMON=1`
/// fallback second, wrapped in `if ! …; then …; fi`.
#[test]
fn codex_generated_command_has_daemon_then_fallback_guard() {
    let (_guard, _project, home) = isolated_project();

    handle_setup_hooks_codex("", false).expect("setup hooks codex ok");
    let v = read_json(&codex_hooks_path(home.path()));

    for (event_name, cli_event, expected_matcher) in HOOK_EVENTS {
        let managed = mdkb_entries(&v, event_name);
        let cmd = managed[0]
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|a| a.first())
            .and_then(|e| e.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or_default();

        assert!(
            cmd.contains(&format!("hook {cli_event}")),
            "{event_name}: primary invocation missing: {cmd}"
        );
        assert!(
            cmd.contains("MDKB_NO_DAEMON=1"),
            "{event_name}: MDKB_NO_DAEMON=1 fallback missing: {cmd}"
        );
        assert!(
            cmd.contains("if !") && cmd.contains("; fi"),
            "{event_name}: fallback must be wrapped in `if ! …; then …; fi`: {cmd}"
        );

        let actual_matcher = managed[0].get("matcher").and_then(|v| v.as_str());
        assert_eq!(
            actual_matcher, *expected_matcher,
            "{event_name}: matcher mismatch"
        );
    }
}
