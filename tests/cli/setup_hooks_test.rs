//! Integration tests for `mdkb setup hooks claude` — settings.json writer.

use std::fs;
use std::sync::MutexGuard;

use mdkb::cli::setup::{HOOK_EVENTS, handle_setup_hooks_claude};
use tempfile::TempDir;

use super::common::env_lock;

/// Build a fresh temp project root with $HOME pointed at a sibling dir so that
/// `user`-scope tests don't touch the real ~/.claude. The guard serializes
/// HOME-mutating tests across this binary.
fn isolated_project() -> (MutexGuard<'static, ()>, TempDir, TempDir) {
    let guard = env_lock();
    let project = tempfile::tempdir().expect("tempdir project");
    let home = tempfile::tempdir().expect("tempdir home");
    // SAFETY: env mutation serialized by `guard`; HOME is overwritten per test.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::set_var("MDKB_BINARY_OVERRIDE", env!("CARGO_BIN_EXE_mdkb"));
    }
    (guard, project, home)
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
    let (_guard, project, _home) = isolated_project();

    let result = handle_setup_hooks_claude(project.path(), "local", "", false, None)
        .expect("setup hooks ok");
    assert!(result.success);
    assert_eq!(result.events_registered.len(), HOOK_EVENTS.len());
    assert!(result.events_skipped.is_empty());
    assert!(!result.dry_run);

    let path = local_settings_path(project.path());
    assert!(path.exists(), "settings file must be written");

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
fn rerunning_is_idempotent_no_duplicates() {
    let (_guard, project, _home) = isolated_project();

    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("first run");
    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("second run");
    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("third run");

    let v = read_json(&local_settings_path(project.path()));
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
fn disable_skips_named_events() {
    let (_guard, project, _home) = isolated_project();

    let result = handle_setup_hooks_claude(
        project.path(),
        "local",
        "session-start,post-tool-use",
        false,
        None,
    )
    .expect("setup hooks ok");
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

    let v = read_json(&local_settings_path(project.path()));
    assert!(mdkb_entries(&v, "SessionStart").is_empty());
    assert!(mdkb_entries(&v, "PostToolUse").is_empty());
    assert_eq!(mdkb_entries(&v, "UserPromptSubmit").len(), 1);
    assert_eq!(mdkb_entries(&v, "PreToolUse").len(), 1);
    assert_eq!(mdkb_entries(&v, "Stop").len(), 1);
}

#[test]
fn dry_run_does_not_write_file() {
    let (_guard, project, _home) = isolated_project();

    let result =
        handle_setup_hooks_claude(project.path(), "local", "", true, None).expect("dry run ok");
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
    let (_guard, project, _home) = isolated_project();

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

    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("setup hooks ok");

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

/// Concurrent invocations writing the same settings file must not lose each
/// other's entries. Without the advisory lock two writers could interleave:
///
///   T1: read (empty)     T2: read (empty)
///   T1: merge A          T2: merge A
///   T1: write {A}        T2: write {A}  ← T1's write is clobbered
///
/// With the lock, T2 waits until T1 finishes and then sees T1's output when
/// it reads inside the critical section — the result is still idempotent (one
/// mdkb entry per event), but both writers complete without data loss.
#[test]
fn concurrent_invocations_preserve_each_others_entries() {
    use std::thread;

    let (_guard, project, _home) = isolated_project();
    let project_path = project.path().to_path_buf();

    // Use the same project dir from N threads. Each spawns handle_setup_hooks_claude
    // for "local" scope, which writes .claude/settings.local.json.
    const N: usize = 8;
    let handles: Vec<_> = (0..N)
        .map(|_| {
            let p = project_path.clone();
            thread::spawn(move || {
                handle_setup_hooks_claude(&p, "local", "", false, None)
                    .expect("concurrent setup hooks ok")
            })
        })
        .collect();

    for h in handles {
        h.join().expect("thread panicked");
    }

    let v = read_json(&local_settings_path(&project_path));
    for (event_name, ..) in HOOK_EVENTS {
        let managed = mdkb_entries(&v, event_name);
        assert_eq!(
            managed.len(),
            1,
            "event {event_name} must have exactly one mdkb entry after {N} concurrent writes"
        );
    }
}

/// Generated command must route through the daemon (`mdkb hook <event>`)
/// and fall back to the in-process path on failure (`MDKB_NO_DAEMON=1 …`).
/// This is the story 016 contract — legacy raw-CLI invocations are gone.
#[test]
fn generated_command_has_daemon_then_fallback_guard() {
    let (_guard, project, _home) = isolated_project();

    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("setup hooks ok");
    let v = read_json(&local_settings_path(project.path()));

    for (event_name, cli_event, expected_matcher) in HOOK_EVENTS {
        let managed = mdkb_entries(&v, event_name);
        let cmd = managed[0]
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|a| a.first())
            .and_then(|e| e.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or_default();

        let expected_primary = format!("hook {cli_event}");
        assert!(
            cmd.contains(&expected_primary),
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
