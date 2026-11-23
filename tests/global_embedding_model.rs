//! `[embedding] model` in `~/.mdkb/daemon.toml` selects the model of every
//! repository that does not choose one (story 094-9a63).

use std::path::Path;
use std::process::Command;

fn mdkb(home: &Path, repo: &Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(args)
        .current_dir(repo)
        .env("HOME", home)
        .env("MDKB_NO_DAEMON", "1")
        .env_remove("MDKB_IGNORE_DAEMON_TOML")
        .output()
        .expect("run mdkb");
    assert!(
        out.status.success(),
        "mdkb {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn selected_model(stats: &str) -> &str {
    let line = stats
        .lines()
        .find(|l| l.starts_with("Embedding: selected "))
        .expect("stats prints the selected embedding model");
    line["Embedding: selected ".len()..]
        .split([':', ' '])
        .next()
        .unwrap()
}

fn setup(daemon_toml: &str) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(home.join(".mdkb")).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(home.join(".mdkb/daemon.toml"), daemon_toml).unwrap();
    mdkb(&home, &repo, &["init"]);
    (dir, home, repo)
}

#[test]
fn daemon_toml_model_selects_granite_with_and_without_a_repo_config() {
    // catches: the global model read only on one load path (file present or absent)
    let (_dir, home, repo) = setup("[embedding]\nmodel = \"granite107m\"\n");
    assert_eq!(
        selected_model(&mdkb(&home, &repo, &["status"])),
        "granite107m"
    );
    std::fs::remove_file(repo.join(".mdkb/config.toml")).unwrap();
    assert_eq!(
        selected_model(&mdkb(&home, &repo, &["status"])),
        "granite107m"
    );
}

#[test]
fn repo_config_model_wins_over_daemon_toml() {
    // catches: daemon.toml overriding a repository's explicit choice
    let (_dir, home, repo) = setup("[embedding]\nmodel = \"granite107m\"\n");
    let config = repo.join(".mdkb/config.toml");
    let mut content = std::fs::read_to_string(&config).unwrap();
    content.push_str("\n[embedding]\nmodel = \"minilm\"\n");
    std::fs::write(&config, content).unwrap();
    assert_eq!(
        selected_model(&mdkb(&home, &repo, &["status"])),
        "AllMiniLML6V2"
    );
}

/// The real model cache, so a test with an isolated HOME does not download weights.
fn model_cache() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".cache/fastembed")
}

fn run(home: &Path, repo: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkb"));
    cmd.args(args)
        .current_dir(repo)
        .env("HOME", home)
        .env("FASTEMBED_CACHE_DIR", model_cache())
        .env("MDKB_NO_DAEMON", "1")
        .env_remove("MDKB_IGNORE_DAEMON_TOML");
    let out = cmd.output().expect("run mdkb");
    assert!(
        out.status.success(),
        "mdkb {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn current_model(stats: &str) -> String {
    let line = stats
        .lines()
        .find(|l| l.starts_with("Embedding: selected "))
        .expect("stats prints the embedding identity");
    let current = line.split("| current ").nth(1).unwrap();
    current
        .split(['"', ':'])
        .nth(1)
        .unwrap_or("none")
        .to_string()
}

/// A MiniLM store whose repository then drops its config.toml, under a
/// daemon.toml that selects Granite.
fn minilm_store_without_config() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let (dir, home, repo) = setup("[embedding]\nmodel = \"granite107m\"\n");
    std::fs::write(
        repo.join("notes.md"),
        "# SQLite recovery\n\nRestore the WAL first.\n",
    )
    .unwrap();
    let config = repo.join(".mdkb/config.toml");
    let mut content = std::fs::read_to_string(&config).unwrap();
    content.push_str("\n[embedding]\nmodel = \"minilm\"\n");
    std::fs::write(&config, content).unwrap();
    run(&home, &repo, &["update"]);
    assert_eq!(
        current_model(&run(&home, &repo, &["status"])),
        "AllMiniLML6V2"
    );
    std::fs::remove_file(&config).unwrap();
    (dir, home, repo)
}

#[test]
#[ignore = "requires cached Granite and MiniLM weights"]
fn reembed_without_a_repo_config_uses_the_daemon_toml_model() {
    // catches: `update --reembed` rebuilding with the built-in MiniLM when config.toml is absent
    let (_dir, home, repo) = minilm_store_without_config();
    run(&home, &repo, &["update", "--reembed"]);
    assert_eq!(
        current_model(&run(&home, &repo, &["status"])),
        "granite107m"
    );
}

#[test]
#[ignore = "requires cached Granite and MiniLM weights"]
fn daemon_migrates_a_store_without_a_repo_config_to_the_daemon_toml_model() {
    // catches: the daemon's background migration checking the model against the
    // built-in MiniLM when config.toml is absent, so it refuses forever
    let (dir, home, repo) = minilm_store_without_config();
    std::fs::write(
        home.join(".mdkb/daemon.toml"),
        format!(
            "whitelist_dirs = [{:?}]\n[embedding]\nmodel = \"granite107m\"\n",
            dir.path().display().to_string()
        ),
    )
    .unwrap();
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_mdkb"))
        .args(["serve", "--daemon"])
        .env("HOME", &home)
        .env("FASTEMBED_CACHE_DIR", model_cache())
        .env_remove("MDKB_IGNORE_DAEMON_TOML")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let socket = home.join(".mdkb/daemon.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !socket.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut current = String::new();
    while std::time::Instant::now() < deadline {
        // CLI commands run in-process; a hook reaches the daemon, which opens the store.
        let payload = format!(
            r#"{{"session_id":"migrate","prompt":"sqlite recovery","cwd":{:?},"hook_event_name":"UserPromptSubmit"}}"#,
            repo.display().to_string()
        );
        let mut hook = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .args(["hook", "user-prompt-submit"])
            .current_dir(&repo)
            .env("HOME", &home)
            .env_remove("MDKB_IGNORE_DAEMON_TOML")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn hook");
        std::io::Write::write_all(&mut hook.stdin.take().unwrap(), payload.as_bytes()).unwrap();
        hook.wait().unwrap();
        current = current_model(&run(&home, &repo, &["status"]));
        if current == "granite107m" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert_eq!(current, "granite107m");
}
