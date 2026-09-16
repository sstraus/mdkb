//! Integration tests for `mdkb setup hooks claude` — settings.json writer.

use std::fs;
use std::sync::MutexGuard;

use mdkb::cli::setup::{
    HOOK_EVENTS, check_hooks, claude_settings_path, detect_hook_drift_for_repo,
    handle_setup_hooks_claude, handle_setup_hooks_claude_with_http,
};
use tempfile::TempDir;

use super::common::env_lock;

/// Build a fresh temp project root with $HOME pointed at a sibling dir so that
/// `user`-scope tests don't touch the real ~/.claude. The guard serializes
/// HOME-mutating tests across this binary.
///
/// `CLAUDE_CONFIG_DIR` is cleared for the same reason: it outranks $HOME in
/// `claude_settings_path`, and a developer running the suite from a session
/// under `CLAUDE_CONFIG_DIR=~/.claude-private` would otherwise have these tests
/// read and write their real settings file.
fn isolated_project() -> (MutexGuard<'static, ()>, TempDir, TempDir) {
    let guard = env_lock();
    let project = tempfile::tempdir().expect("tempdir project");
    let home = tempfile::tempdir().expect("tempdir home");
    // SAFETY: env mutation serialized by `guard`; HOME is overwritten per test.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::remove_var("CLAUDE_CONFIG_DIR");
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
fn http_registration_uses_native_handlers_except_for_session_start() {
    let (_guard, project, _home) = isolated_project();

    handle_setup_hooks_claude_with_http(
        project.path(),
        "local",
        "",
        false,
        None,
        Some("http://127.0.0.1:8080/"),
    )
    .expect("HTTP hook setup ok");

    let settings = read_json(&local_settings_path(project.path()));
    for (event_name, cli_event, _) in HOOK_EVENTS {
        let handler = &mdkb_entries(&settings, event_name)[0]["hooks"][0];
        if *event_name == "SessionStart" {
            assert_eq!(handler["type"], "command");
            assert!(
                handler["command"]
                    .as_str()
                    .is_some_and(|command| command.contains("hook session-start"))
            );
        } else {
            assert_eq!(handler["type"], "http");
            assert_eq!(
                handler["url"],
                format!("http://127.0.0.1:8080/hook/{}", cli_event.replace('-', "_"))
            );
            assert_eq!(
                handler["headers"]["Authorization"],
                "Bearer $MDKB_HOOK_TOKEN"
            );
            assert_eq!(
                handler["allowedEnvVars"],
                serde_json::json!(["MDKB_HOOK_TOKEN"])
            );
        }
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

/// Regression for the live audit shape: SessionStart/UserPromptSubmit/PostToolUse
/// each registered twice (a tagged `mdkb` entry + a legacy untagged absolute-path
/// entry), Stop missing entirely, alongside a foreign rtk hook. Re-running setup
/// must collapse each event to exactly one mdkb entry, register the missing Stop,
/// and leave the rtk hook untouched.
#[test]
fn live_shape_dedupes_to_one_per_event_and_registers_stop() {
    let (_guard, project, _home) = isolated_project();
    let settings_path = local_settings_path(project.path());
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();

    let live = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook session-start"}]},
                {"hooks": [{"type": "command", "command": "/Users/x/.local/bin/mdkb hook session-start"}]}
            ],
            "UserPromptSubmit": [
                {"_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook user-prompt-submit"}]},
                {"hooks": [{"type": "command", "command": "/Users/x/.local/bin/mdkb hook user-prompt-submit"}]}
            ],
            "PostToolUse": [
                {"matcher": "Edit", "_managedBy": "mdkb", "hooks": [{"type": "command", "command": "mdkb hook post-tool-use"}]},
                {"matcher": "Edit", "hooks": [{"type": "command", "command": "/Users/x/.local/bin/mdkb hook post-tool-use"}]}
            ],
            "PreToolUse": [
                {"matcher": "Grep", "hooks": [{"type": "command", "command": "/Users/x/.local/bin/mdkb hook pre-tool-use"}]},
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "rtk hook claude"}]}
            ]
        }
    });
    fs::write(&settings_path, serde_json::to_string_pretty(&live).unwrap()).unwrap();

    // Pre-condition: drift detection sees 3 duplicated events + Stop missing.
    let before = detect_hook_drift_for_repo(project.path(), None);
    assert!(!before.is_clean(), "seeded live shape must report drift");
    assert_eq!(
        before.duplicated,
        vec!["SessionStart", "UserPromptSubmit", "PostToolUse"]
    );
    assert_eq!(before.missing, vec!["Stop"]);

    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("setup hooks ok");

    let v = read_json(&settings_path);
    for (event_name, ..) in HOOK_EVENTS {
        assert_eq!(
            mdkb_entries(&v, event_name).len(),
            1,
            "event {event_name} must collapse to exactly one mdkb entry"
        );
    }

    // rtk survives untouched.
    let pre = v
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let rtk = pre.iter().filter(|i| {
        i.get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|a| a.first())
            .and_then(|e| e.get("command"))
            .and_then(|c| c.as_str())
            == Some("rtk hook claude")
    });
    assert_eq!(rtk.count(), 1, "foreign rtk hook must survive");

    // Post-condition: no drift after setup.
    let after = detect_hook_drift_for_repo(project.path(), None);
    assert!(after.is_clean(), "setup must clear all drift: {after:?}");
}

/// An empty settings file (whitespace only) is treated as `{}` and gets the
/// full canonical set — never an error, never a clobber-to-null.
#[test]
fn empty_settings_file_gets_full_canonical_set() {
    let (_guard, project, _home) = isolated_project();
    let settings_path = local_settings_path(project.path());
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    fs::write(&settings_path, "   \n").unwrap();

    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("setup hooks ok");

    let v = read_json(&settings_path);
    for (event_name, ..) in HOOK_EVENTS {
        assert_eq!(mdkb_entries(&v, event_name).len(), 1, "{event_name}");
    }
    assert!(detect_hook_drift_for_repo(project.path(), None).is_clean());
}

/// Running setup on already-canonical settings is a no-op (still one per event).
#[test]
fn already_clean_settings_stay_clean() {
    let (_guard, project, _home) = isolated_project();
    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("first run");
    let first = fs::read_to_string(local_settings_path(project.path())).unwrap();

    handle_setup_hooks_claude(project.path(), "local", "", false, None).expect("second run");
    let second = fs::read_to_string(local_settings_path(project.path())).unwrap();

    assert_eq!(
        first, second,
        "re-running on clean settings must be byte-stable"
    );
    assert!(detect_hook_drift_for_repo(project.path(), None).is_clean());
}

/// Corrupted JSON must error out WITHOUT clobbering the file — the user's
/// (broken but recoverable) settings survive for manual repair.
#[test]
fn corrupted_json_errors_without_clobbering() {
    let (_guard, project, _home) = isolated_project();
    let settings_path = local_settings_path(project.path());
    fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
    let corrupt = "{ \"hooks\": { broken";
    fs::write(&settings_path, corrupt).unwrap();

    let result = handle_setup_hooks_claude(project.path(), "local", "", false, None);
    assert!(
        result.is_err(),
        "corrupted settings must error, not silently overwrite"
    );

    let after = fs::read_to_string(&settings_path).unwrap();
    assert_eq!(after, corrupt, "corrupted file must be left untouched");
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
        // The shell fallback is gone on purpose (story 021-0636). It could never
        // fire: `run_hook` returns Ok(()) on every failure because the host hook
        // must exit 0, so the `if !` branch was unreachable and the settings
        // file advertised a rail that did not exist. The real fallback now runs
        // in-process — see tests/hook_daemon_fallback.rs — so the wiring must
        // describe what actually happens.
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

/// `CLAUDE_CONFIG_DIR` is what Claude Code itself reads to locate its config, so
/// user-scope setup must target that directory. Before this, setup always wrote
/// `$HOME/.claude/settings.json` while the session ran out of another dir, and
/// every hook it registered there was dead.
#[test]
fn user_scope_follows_claude_config_dir() {
    let (_guard, project, home) = isolated_project();
    let private = home.path().join(".claude-private");
    // SAFETY: env mutation serialized by the guard held for this test.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &private) };

    let result =
        handle_setup_hooks_claude(project.path(), "user", "", false, None).expect("setup hooks ok");

    assert_eq!(result.settings_path, private.join("settings.json"));
    let v = read_json(&result.settings_path);
    for (event_name, ..) in HOOK_EVENTS {
        assert_eq!(mdkb_entries(&v, event_name).len(), 1, "{event_name}");
    }
    assert!(
        !home.path().join(".claude").join("settings.json").exists(),
        "the default profile must be left untouched"
    );

    // SAFETY: same guard; restore the isolated default for later tests.
    unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
}

/// With no `CLAUDE_CONFIG_DIR`, user scope still resolves to `$HOME/.claude`.
#[test]
fn user_scope_falls_back_to_home_claude() {
    let (_guard, project, home) = isolated_project();
    let path = claude_settings_path(project.path(), "user", None).expect("path resolves");
    assert_eq!(path, home.path().join(".claude").join("settings.json"));
}

/// An explicit `--profile-dir` outranks the environment: the flag is how a user
/// writes hooks for a profile other than the one they are running under.
#[test]
fn explicit_profile_dir_outranks_claude_config_dir() {
    let (_guard, project, home) = isolated_project();
    let from_env = home.path().join("from-env");
    let explicit = home.path().join("explicit");
    // SAFETY: env mutation serialized by the guard held for this test.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &from_env) };

    let path =
        claude_settings_path(project.path(), "user", Some(&explicit)).expect("path resolves");
    assert_eq!(path, explicit.join("settings.json"));

    // SAFETY: same guard.
    unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
}

/// The shape that made this story: a config dir carrying every event except
/// `Stop`, in the legacy untagged form. `check_hooks` must name the file it read
/// and report `Stop` as missing — a silent pass there is why that profile never
/// mined a prior.
#[test]
fn check_hooks_reports_the_missing_stop_entry() {
    let (_guard, project, home) = isolated_project();
    let private = home.path().join(".claude-private");
    fs::create_dir_all(&private).unwrap();
    // SAFETY: env mutation serialized by the guard held for this test.
    unsafe { std::env::set_var("CLAUDE_CONFIG_DIR", &private) };

    let legacy = |event: &str| {
        serde_json::json!({
            "hooks": [{
                "type": "command",
                "command": format!("'/usr/local/bin/mdkb' hook {event}")
            }]
        })
    };
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [legacy("session-start")],
            "UserPromptSubmit": [legacy("user-prompt-submit")],
            "PostToolUse": [legacy("post-tool-use")],
            "PreToolUse": [legacy("pre-tool-use")]
        }
    });
    fs::write(
        private.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let check = check_hooks(project.path()).expect("check runs");
    assert_eq!(check.user_path, private.join("settings.json"));
    assert_eq!(check.local_path, local_settings_path(project.path()));
    assert_eq!(check.drift.missing, vec!["Stop"]);
    assert!(check.drift.duplicated.is_empty());
    assert!(!check.drift.is_clean());

    // Setup closes the gap, and the check then passes against the same file.
    handle_setup_hooks_claude(project.path(), "user", "", false, None).expect("setup hooks ok");
    let after = check_hooks(project.path()).expect("check runs");
    assert!(
        after.drift.is_clean(),
        "setup must clear the drift: {after:?}"
    );

    // SAFETY: same guard.
    unsafe { std::env::remove_var("CLAUDE_CONFIG_DIR") };
}
