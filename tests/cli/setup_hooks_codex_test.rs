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
    assert_eq!(result.events_skipped, [] as [String; 0]);
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
    assert_eq!(result.events_registered.len(), 4);
    assert!(
        result
            .events_registered
            .contains(&"UserPromptSubmit".to_string())
    );
    assert!(result.events_registered.contains(&"PreToolUse".to_string()));
    assert!(
        result
            .events_registered
            .contains(&"PostToolUseFailure".to_string())
    );
    assert!(result.events_registered.contains(&"Stop".to_string()));
    assert_eq!(result.events_skipped.len(), 2);
    assert!(result.events_skipped.contains(&"SessionStart".to_string()));
    assert!(result.events_skipped.contains(&"PostToolUse".to_string()));

    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let v = read_json(&codex_hooks_path(&home));
    assert_eq!(
        mdkb_entries(&v, "SessionStart"),
        [] as [&serde_json::Value; 0]
    );
    assert_eq!(
        mdkb_entries(&v, "PostToolUse"),
        [] as [&serde_json::Value; 0]
    );
    assert_eq!(mdkb_entries(&v, "UserPromptSubmit").len(), 1);
    assert_eq!(mdkb_entries(&v, "PreToolUse").len(), 1);
    assert_eq!(mdkb_entries(&v, "PostToolUseFailure").len(), 1);
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
        // See setup_hooks_test.rs: the shell fallback was unreachable by
        // construction and is now in-process (story 021-0636).
        assert!(
            !cmd.contains("MDKB_NO_DAEMON=1"),
            "{event_name}: the unreachable shell fallback must not be generated: {cmd}"
        );
        assert!(
            !cmd.contains("if !"),
            "{event_name}: no conditional retry belongs in the wiring: {cmd}"
        );

        let actual_matcher = managed[0].get("matcher").and_then(|v| v.as_str());
        assert_eq!(
            actual_matcher, *expected_matcher,
            "{event_name}: matcher mismatch"
        );
    }
}

/// Codex 0.166 `/hooks` titles a handler with its trimmed `statusMessage` and
/// shows "Unnamed hook" without it (`codex-rs/tui/src/bottom_pane/hooks_browser_view.rs`).
/// Catches: setup writes handlers without `statusMessage`, or a reinstall over an
/// install from an older mdkb duplicates entries instead of relabelling them.
#[test]
fn codex_handlers_carry_a_label_and_reinstall_relabels_old_entries() {
    let (_guard, _project, home) = isolated_project();

    let hooks_path = codex_hooks_path(home.path());
    fs::create_dir_all(hooks_path.parent().unwrap()).unwrap();
    // Shape written by mdkb before the label existed (copied from a live ~/.codex/hooks.json).
    let mut old_install = serde_json::json!({"hooks": {}});
    for (event_name, cli_event, _) in HOOK_EVENTS {
        old_install["hooks"][*event_name] = serde_json::json!([{
            "_managedBy": "mdkb",
            "hooks": [{"command": format!("'/old/mdkb' hook {cli_event}"), "type": "command"}]
        }]);
    }
    fs::write(&hooks_path, old_install.to_string()).unwrap();

    handle_setup_hooks_codex("", false).expect("first reinstall");
    handle_setup_hooks_codex("", false).expect("second reinstall");

    let v = read_json(&hooks_path);
    for (event_name, cli_event, _) in HOOK_EVENTS {
        let managed = mdkb_entries(&v, event_name);
        assert_eq!(
            managed.len(),
            1,
            "{event_name}: one mdkb entry: {managed:?}"
        );
        let label = managed[0]["hooks"][0]
            .get("statusMessage")
            .and_then(|s| s.as_str())
            .unwrap_or_default();
        assert_eq!(label, format!("mdkb {cli_event}"), "{event_name}: label");
    }
}

/// Catches remove-then-append shifting trust-key positions after another tool
/// adds a group. Exercise all events, first/middle/last positions and both hosts.
#[test]
fn setup_preserves_hook_positions_and_bytes_for_both_hosts() {
    use mdkb::cli::setup::handle_setup_hooks_claude;

    let (_guard, project, home) = isolated_project();
    let mut failures = Vec::new();
    for codex in [true, false] {
        let setup = || {
            if codex {
                handle_setup_hooks_codex("", false).unwrap();
            } else {
                handle_setup_hooks_claude(project.path(), "local", "", false, None).unwrap();
            }
        };
        let path = if codex {
            codex_hooks_path(home.path())
        } else {
            project.path().join(".claude/settings.local.json")
        };
        setup();
        let canonical = read_json(&path);
        for position in 0..3 {
            let mut seeded = canonical.clone();
            for (event, ..) in HOOK_EVENTS {
                let mut groups = vec![
                    serde_json::json!({"hooks": [{"type": "command", "command": "echo first"}]}),
                    serde_json::json!({"hooks": [{"type": "command", "command": "echo second"}]}),
                ];
                groups.insert(position, canonical["hooks"][*event][0].clone());
                seeded["hooks"][*event] = serde_json::json!(groups);
            }
            let before = serde_json::to_vec_pretty(&seeded).unwrap();
            fs::write(&path, &before).unwrap();
            setup();
            let after = read_json(&path);
            for (event, ..) in HOOK_EVENTS {
                if after["hooks"][*event] != seeded["hooks"][*event] {
                    failures.push(format!(
                        "codex={codex}, {event}, position={position}: moved or changed groups"
                    ));
                }
            }
            if fs::read(&path).unwrap() != before {
                failures.push(format!("codex={codex}, position={position}: changed bytes"));
            }
            setup();
            assert_eq!(
                fs::read(&path).unwrap(),
                serde_json::to_vec_pretty(&after).unwrap()
            );
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Catches a binary-path update moving groups or changing fields other than
/// command text. Uses the persisted output of each real host setup as the oracle.
#[test]
fn setup_binary_path_change_only_updates_commands_for_both_hosts() {
    use mdkb::cli::setup::handle_setup_hooks_claude;

    let (_guard, project, home) = isolated_project();
    for codex in [true, false] {
        let setup = || {
            if codex {
                handle_setup_hooks_codex("", false).unwrap();
            } else {
                handle_setup_hooks_claude(project.path(), "local", "", false, None).unwrap();
            }
        };
        let path = if codex {
            codex_hooks_path(home.path())
        } else {
            project.path().join(".claude/settings.local.json")
        };
        // SAFETY: isolated_project holds the shared environment lock.
        unsafe { std::env::set_var("MDKB_BINARY_OVERRIDE", "/old/mdkb") };
        setup();
        let mut before = read_json(&path);
        for (event, ..) in HOOK_EVENTS {
            before["hooks"][*event].as_array_mut().unwrap().push(
                serde_json::json!({"hooks": [{"type": "command", "command": "echo later tool"}]}),
            );
        }
        fs::write(&path, serde_json::to_vec_pretty(&before).unwrap()).unwrap();
        unsafe { std::env::set_var("MDKB_BINARY_OVERRIDE", "/new/mdkb") };
        setup();
        let after = read_json(&path);
        for (event, cli_event, _) in HOOK_EVENTS {
            before["hooks"][*event][0]["hooks"][0]["command"] =
                serde_json::json!(format!("'/new/mdkb' hook {cli_event}"));
        }
        assert_eq!(after, before, "codex={codex}: only command text changes");
    }
}

/// Catches appending before existing groups or retaining duplicate legacy
/// registrations. Missing installs append once; repair keeps the first slot.
#[test]
fn setup_appends_missing_and_deduplicates_in_place_for_all_events() {
    let (_guard, _project, home) = isolated_project();
    let path = codex_hooks_path(home.path());
    handle_setup_hooks_codex("", false).unwrap();
    let canonical = read_json(&path);
    let foreign = serde_json::json!({"hooks": [{"type": "command", "command": "echo foreign"}]});
    let mut seeded = canonical.clone();
    for (event, ..) in HOOK_EVENTS {
        seeded["hooks"][*event] = serde_json::json!([foreign.clone(), foreign.clone()]);
    }
    fs::write(&path, seeded.to_string()).unwrap();
    handle_setup_hooks_codex("", false).unwrap();
    let appended = read_json(&path);
    for (event, ..) in HOOK_EVENTS {
        assert_eq!(
            appended["hooks"][*event],
            serde_json::json!([
                foreign.clone(),
                foreign.clone(),
                canonical["hooks"][*event][0].clone()
            ]),
            "{event}: append after unrelated groups"
        );
    }
    let bytes = fs::read(&path).unwrap();
    handle_setup_hooks_codex("", false).unwrap();
    assert_eq!(fs::read(&path).unwrap(), bytes, "append only once");

    for (event, cli_event, _) in HOOK_EVENTS {
        seeded["hooks"][*event] = serde_json::json!([
            foreign.clone(),
            {"hooks": [{"type": "command", "command": format!("mdkb hook {cli_event}")}]},
            foreign.clone(), canonical["hooks"][*event][0].clone()
        ]);
    }
    fs::write(&path, seeded.to_string()).unwrap();
    handle_setup_hooks_codex("", false).unwrap();
    let repaired = read_json(&path);
    for (event, ..) in HOOK_EVENTS {
        assert_eq!(
            repaired["hooks"][*event],
            serde_json::json!([
                foreign.clone(),
                canonical["hooks"][*event][0].clone(),
                foreign.clone()
            ]),
            "{event}: replace first legacy registration and remove later duplicate"
        );
    }
}
