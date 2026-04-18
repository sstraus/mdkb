//! Integration tests for `mdkb setup hooks claude` — settings.json writer.

use std::fs;

use mdkb::cli::setup::{HOOK_EVENTS, handle_setup_hooks_claude};
use tempfile::TempDir;

/// Build a fresh temp project root with $HOME pointed at a sibling dir so that
/// `user`-scope tests don't touch the real ~/.claude. Returns (project_root, home_dir).
fn isolated_project() -> (TempDir, TempDir) {
    let project = tempfile::tempdir().expect("tempdir project");
    let home = tempfile::tempdir().expect("tempdir home");
    // SAFETY: tests are single-threaded per binary wrt env mutation here, and we
    // always overwrite HOME before each call so earlier state is irrelevant.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("MDKB_BINARY_OVERRIDE", env!("CARGO_BIN_EXE_mdkb"));
    }
    (project, home)
}

fn local_settings_path(project: &std::path::Path) -> std::path::PathBuf {
    project.join(".claude").join("settings.local.json")
}

fn read_json(path: &std::path::Path) -> serde_json::Value {
    let raw = fs::read_to_string(path).expect("settings file exists");
    serde_json::from_str(&raw).expect("settings is valid JSON")
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
fn fresh_settings_gets_three_managed_hook_entries() {
    let (project, _home) = isolated_project();

    let result =
        handle_setup_hooks_claude(project.path(), "local", "", false).expect("setup hooks ok");
    assert!(result.success);
    assert_eq!(result.events_registered.len(), HOOK_EVENTS.len());
    assert!(result.events_skipped.is_empty());
    assert!(!result.dry_run);

    let path = local_settings_path(project.path());
    assert!(path.exists(), "settings file must be written");

    let v = read_json(&path);
    for (event_name, _) in HOOK_EVENTS {
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
fn rerunning_is_idempotent_no_duplicates() {
    let (project, _home) = isolated_project();

    handle_setup_hooks_claude(project.path(), "local", "", false).expect("first run");
    handle_setup_hooks_claude(project.path(), "local", "", false).expect("second run");
    handle_setup_hooks_claude(project.path(), "local", "", false).expect("third run");

    let v = read_json(&local_settings_path(project.path()));
    for (event_name, _) in HOOK_EVENTS {
        let managed = mdkb_entries(&v, event_name);
        assert_eq!(
            managed.len(),
            1,
            "event {event_name} must stay at exactly one mdkb entry after 3 runs"
        );
    }
}

#[test]
fn disable_skips_named_events() {
    let (project, _home) = isolated_project();

    let result = handle_setup_hooks_claude(
        project.path(),
        "local",
        "session-start,post-tool-use",
        false,
    )
    .expect("setup hooks ok");
    assert_eq!(result.events_registered, vec!["UserPromptSubmit"]);
    assert_eq!(result.events_skipped.len(), 2);
    assert!(result.events_skipped.contains(&"SessionStart".to_string()));
    assert!(result.events_skipped.contains(&"PostToolUse".to_string()));

    let v = read_json(&local_settings_path(project.path()));
    assert!(mdkb_entries(&v, "SessionStart").is_empty());
    assert!(mdkb_entries(&v, "PostToolUse").is_empty());
    assert_eq!(mdkb_entries(&v, "UserPromptSubmit").len(), 1);
}

#[test]
fn dry_run_does_not_write_file() {
    let (project, _home) = isolated_project();

    let result = handle_setup_hooks_claude(project.path(), "local", "", true).expect("dry run ok");
    assert!(result.dry_run);
    assert!(result.success);
    assert_eq!(result.events_registered.len(), HOOK_EVENTS.len());

    assert!(
        !local_settings_path(project.path()).exists(),
        "dry-run must not create settings file"
    );
    assert!(result.merged_json.is_object());
    assert!(result.merged_json.get("hooks").is_some());
}

#[test]
fn preserves_non_mdkb_hook_entries() {
    let (project, _home) = isolated_project();

    let settings_path = local_settings_path(project.path());
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

    let preexisting = serde_json::json!({
        "someOtherKey": { "keep": "me" },
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
        &settings_path,
        serde_json::to_string_pretty(&preexisting).unwrap(),
    )
    .unwrap();

    handle_setup_hooks_claude(project.path(), "local", "", false).expect("setup hooks ok");

    let v = read_json(&settings_path);

    assert_eq!(
        v.get("someOtherKey").and_then(|k| k.get("keep")),
        Some(&serde_json::Value::String("me".to_string())),
        "unrelated top-level keys must be preserved"
    );

    let ss = v
        .get("hooks")
        .and_then(|h| h.get("SessionStart"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let other_tool = ss
        .iter()
        .filter(|i| i.get("_managedBy").and_then(|v| v.as_str()) == Some("some-other-tool"));
    assert_eq!(
        other_tool.count(),
        1,
        "other-tool SessionStart entry must survive"
    );
    assert_eq!(
        mdkb_entries(&v, "SessionStart").len(),
        1,
        "mdkb SessionStart entry must be added alongside"
    );

    let ptu = v
        .get("hooks")
        .and_then(|h| h.get("PostToolUse"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let untagged = ptu.iter().filter(|i| {
        i.get("_managedBy").is_none()
            && i.get("hooks")
                .and_then(|h| h.as_array())
                .and_then(|a| a.first())
                .and_then(|e| e.get("command"))
                .and_then(|c| c.as_str())
                == Some("echo untagged")
    });
    assert_eq!(
        untagged.count(),
        1,
        "untagged PostToolUse entry must survive"
    );
}
