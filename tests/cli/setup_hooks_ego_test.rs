//! CLI regressions for ego generic hook registration.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Stop",
];

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(args)
        .env("EGO_HOME", home)
        .env("MDKB_BINARY_OVERRIDE", env!("CARGO_BIN_EXE_mdkb"))
        .output()
        .unwrap()
}
fn ok(home: &Path, args: &[&str]) -> Output {
    let output = run(home, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn read(home: &Path) -> String {
    fs::read_to_string(home.join("config.toml")).unwrap()
}
fn assert_hooks(raw: &str) {
    let doc: toml::Value = raw.parse().unwrap();
    assert!(doc.get("mdkb_prompt_command").is_none(), "{raw}");
    let hooks = doc["hooks"].as_array().expect("generic hooks required");
    assert_eq!(hooks.len(), EVENTS.len());
    let binary = env!("CARGO_BIN_EXE_mdkb");
    assert!(
        doc["exec_paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str() == Some(binary))
    );
    let allow = doc["permissions"]["allow"].as_array().unwrap();
    for (hook, event) in hooks.iter().zip(EVENTS) {
        assert_eq!(hook["event"].as_str(), Some(*event));
        assert_eq!(hook["handler"]["type"].as_str(), Some("command"));
        assert_eq!(hook["handler"]["command"].as_str(), Some(binary));
        assert_eq!(hook["handler"]["timeout"].as_integer(), Some(10));
        let args = hook["handler"]["args"].as_array().unwrap();
        assert_eq!(args[0].as_str(), Some("hook"));
        let event_arg = match *event {
            "SessionStart" => "session-start",
            "UserPromptSubmit" => "user-prompt-submit",
            "PreToolUse" => "pre-tool-use",
            "Stop" => "stop",
            _ => "post-tool-use",
        };
        assert_eq!(args[1].as_str(), Some(event_arg));
        assert!(
            allow
                .iter()
                .any(|v| v.as_str() == Some(&format!("exec({binary} hook {event_arg})")))
        );
    }
    assert_eq!(allow.len(), 5);
}

/// Catches: setup still writes the removed vendor key instead of generic hooks.
#[test]
fn generic_setup_replaces_legacy_key_and_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.toml"),
        "# owner migration note\nmdkb_prompt_command = '/old/mdkb' # keep this note\n",
    )
    .unwrap();
    ok(home.path(), &["setup", "hooks", "ego"]);
    let first = read(home.path());
    assert_hooks(&first);
    assert!(first.contains("# owner migration note"));
    assert!(first.contains("# keep this note"));
    ok(home.path(), &["setup", "hooks", "ego"]);
    assert_eq!(first, read(home.path()));
}

/// Catches: uninstall removes unrelated hooks or leaves duplicates and execution rules.
#[test]
fn generic_remove_preserves_unrelated_config_and_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let original = "# owner note\nmodel = 'test/model' # keep\nexec_paths = ['/other']\n[permissions]\nallow = ['exec(/other)'] # keep rule\n[[hooks]]\nevent = 'Stop'\n[hooks.handler]\ntype = 'command'\ncommand = '/other'\nargs = []\n";
    fs::write(home.path().join("config.toml"), original).unwrap();
    ok(home.path(), &["setup", "hooks", "ego"]);
    let installed = read(home.path());
    assert!(installed.contains("# owner note"));
    assert!(installed.contains("model = 'test/model' # keep"));
    assert!(installed.contains("# keep rule"));
    ok(home.path(), &["setup", "remove", "hooks", "ego"]);
    let removed = read(home.path());
    let doc: toml::Value = removed.parse().unwrap();
    assert_eq!(doc["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(
        doc["hooks"][0]["handler"]["command"].as_str(),
        Some("/other")
    );
    assert_eq!(doc["exec_paths"].as_array().unwrap().len(), 1);
    assert_eq!(doc["permissions"]["allow"].as_array().unwrap().len(), 1);
    assert!(removed.contains("# owner note"));
    ok(home.path(), &["setup", "remove", "hooks", "ego"]);
    assert_eq!(removed, read(home.path()));
}

/// Catches: dry-run or invalid TOML partially mutates the config.
#[test]
fn generic_setup_dry_run_and_invalid_config_never_write() {
    let home = tempfile::tempdir().unwrap();
    let preview = ok(home.path(), &["setup", "hooks", "ego", "--dry-run"]);
    assert_hooks(&String::from_utf8(preview.stdout).unwrap());
    assert!(!home.path().join("config.toml").exists());
    fs::write(home.path().join("config.toml"), "broken = = true").unwrap();
    assert!(
        !run(home.path(), &["setup", "hooks", "ego"])
            .status
            .success()
    );
    assert_eq!(read(home.path()), "broken = = true");
}

/// Catches: atomic replacement destroys a symlink instead of updating its target.
#[cfg(unix)]
#[test]
fn generic_setup_and_remove_preserve_symlink_target() {
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join("owner.toml");
    fs::write(&target, "# owner\nmodel = 'test/model'\n").unwrap();
    std::os::unix::fs::symlink(&target, home.path().join("config.toml")).unwrap();
    ok(home.path(), &["setup", "hooks", "ego"]);
    assert_hooks(&fs::read_to_string(&target).unwrap());
    ok(home.path(), &["setup", "remove", "hooks", "ego"]);
    assert!(
        fs::symlink_metadata(home.path().join("config.toml"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(fs::read_to_string(target).unwrap().contains("# owner"));
}

/// Catches: the valid empty hooks array used by fresh configs is refused.
#[test]
fn generic_setup_accepts_empty_hook_array() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.toml"),
        "# owner hooks\nhooks = []\n",
    )
    .unwrap();
    ok(home.path(), &["setup", "hooks", "ego"]);
    let raw = read(home.path());
    assert_hooks(&raw);
    assert!(raw.contains("# owner hooks"));
}

/// Catches: removal matches only the event and deletes user-modified hooks.
#[test]
fn generic_remove_preserves_modified_hook() {
    let home = tempfile::tempdir().unwrap();
    ok(home.path(), &["setup", "hooks", "ego"]);
    let raw = read(home.path()).replacen("timeout = 10", "timeout = 20 # owner timeout", 1);
    fs::write(home.path().join("config.toml"), raw).unwrap();
    ok(home.path(), &["setup", "remove", "hooks", "ego"]);
    let raw = read(home.path());
    let doc: toml::Value = raw.parse().unwrap();
    assert_eq!(doc["hooks"].as_array().unwrap().len(), 1);
    assert_eq!(doc["hooks"][0]["handler"]["timeout"].as_integer(), Some(20));
    assert!(raw.contains("# owner timeout"));
}

/// Catches: a wrong array/table type panics or partially writes registration.
#[test]
fn generic_invalid_field_types_are_reported_without_writes() {
    for initial in [
        "hooks = 'wrong'\n",
        "exec_paths = 42\n",
        "permissions = []\n",
        "[permissions]\nallow = 'wrong'\n",
    ] {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), initial).unwrap();
        assert!(
            !run(home.path(), &["setup", "hooks", "ego"])
                .status
                .success()
        );
        assert_eq!(read(home.path()), initial);
    }
}
