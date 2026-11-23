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
