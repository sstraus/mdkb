//! Critic round 3 for story 221-8fb6: `DaemonConfig::ignored_paths` drops
//! entries that cannot name one directory, and says so once.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mdkb::daemon::config::DaemonConfig;
use mdkb::daemon::registry::RepoRegistry;
use mdkb::daemon::repo_map::discover_nested_stores;

fn config_ignoring(ignore: &[&str]) -> DaemonConfig {
    DaemonConfig {
        ignore: ignore.iter().map(|s| s.to_string()).collect(),
        ..DaemonConfig::default()
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `f` with a thread-local subscriber and return the WARN lines it logged.
fn warnings_during<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let sink = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let out = tracing::subscriber::with_default(subscriber, f);
    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let lines = text.lines().map(str::to_string).collect();
    (out, lines)
}

fn plant(at: &Path) -> PathBuf {
    std::fs::create_dir_all(at.join(".mdkb")).unwrap();
    std::fs::write(at.join(".mdkb/index.sqlite"), b"").unwrap();
    mdkb::domain::canonicalize_plain(at).unwrap()
}

/// Catches: the emptiness check done without `trim` (or the whitespace entry
/// taking the "relative path" arm only by luck): `"  "` and `"\t"` must be
/// dropped, and must be named in the warning.
#[test]
fn whitespace_only_entries_are_dropped_and_reported() {
    let config = config_ignoring(&["  ", "\t", " \n"]);

    let (paths, warnings) = warnings_during(|| config.ignored_paths());

    assert!(paths.is_empty(), "kept {paths:?}");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

/// Catches: `~user` / `~user/x` read as a literal directory name, or
/// expanded against the current user's home (`~user` → `$HOME/user`).
#[test]
fn a_tilde_user_entry_is_dropped_not_joined_to_home() {
    let config = config_ignoring(&["~root", "~root/projects"]);

    let paths = config.ignored_paths();

    assert!(paths.is_empty(), "kept {paths:?}");
}

/// Catches: leading whitespace on an otherwise absolute entry being trimmed
/// into a live path in one place and not another, so `daemon status` and the
/// daemon disagree. The entry is relative as written; it is dropped.
#[test]
fn an_absolute_entry_with_leading_whitespace_is_dropped() {
    let config = config_ignoring(&[" /tmp", " ~/x"]);

    let paths = config.ignored_paths();

    assert!(paths.is_empty(), "kept {paths:?}");
}

/// Catches: a bad entry swallowing the good ones around it (an early return
/// instead of `filter_map`), or the order of the survivors changing.
#[test]
fn good_entries_survive_in_order_around_bad_ones() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let (a_s, b_s) = (
        a.path().to_string_lossy().to_string(),
        b.path().to_string_lossy().to_string(),
    );
    let config = config_ignoring(&["", &a_s, "rel", &b_s, "~nobody"]);

    let paths = config.ignored_paths();

    assert_eq!(
        paths,
        vec![
            mdkb::domain::canonicalize_plain(a.path()).unwrap(),
            mdkb::domain::canonicalize_plain(b.path()).unwrap()
        ]
    );
}

/// Catches: a duplicated entry making the list lie about its content (kept
/// twice is harmless, but the warning must still be one line for the whole
/// config, and a duplicated bad entry must not multiply the lines).
#[test]
fn duplicated_bad_entries_produce_one_warning_line() {
    let config = config_ignoring(&["", "", "rel", "rel"]);

    let (_, warnings) = warnings_during(|| config.ignored_paths());

    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

/// Catches: a warning for a config that has nothing to complain about
/// (`!dropped.is_empty()` inverted, or logged unconditionally).
#[test]
fn a_clean_config_logs_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_ignoring(&[&dir.path().to_string_lossy(), "~/some/where"]);

    let (_, warnings) = warnings_during(|| config.ignored_paths());

    assert!(warnings.is_empty(), "{warnings:?}");
}

/// Catches: an entry that only canonicalizes to `/` (a symlink or `..` chain)
/// being treated as "bad" and dropped, or an absolute `/` being dropped: the
/// operator wrote an absolute path, it is kept — and then really does silence
/// every store, which is what the entry says.
#[test]
fn an_absolute_entry_that_resolves_to_root_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let up = format!("{}/../../../../../../../../../..", dir.path().display());
    let config = config_ignoring(&[&up]);

    let paths = config.ignored_paths();

    // The platform root of the same volume the tempdir lives on: `/` on unix,
    // `C:\` (or whichever drive holds the temp dir) on Windows.
    let root = dir.path().ancestors().last().unwrap().to_path_buf();
    assert_eq!(paths, vec![root]);
}

/// Catches: the warning fired on every discovery call. The daemon asks
/// `discoverable_roots` on every root-less MCP call, and each asks
/// `ignored_paths` twice (walk and filter); one bad entry then logs two lines
/// per request for the life of the process.
#[test]
fn a_bad_entry_is_reported_once_across_repeated_discovery_calls() {
    let state = tempfile::tempdir().unwrap();
    let repos = tempfile::tempdir().unwrap();
    plant(&repos.path().join("a"));
    let registry = RepoRegistry::new(DaemonConfig {
        max_active_repos: 4,
        whitelist_dirs: vec![repos.path().to_string_lossy().to_string()],
        state_dir: Some(state.path().to_path_buf()),
        ignore: vec![String::new(), "relative/dir".to_string()],
        ..DaemonConfig::default()
    });
    let extra = vec![repos.path().to_path_buf()];

    let (_, warnings) = warnings_during(|| {
        for _ in 0..5 {
            registry.discoverable_roots_under(&extra);
        }
    });

    assert!(
        warnings.len() <= 1,
        "{} warnings for one bad config over 5 calls: {warnings:?}",
        warnings.len()
    );
}

/// Catches: dropping the empty entry but still passing the *raw* list to the
/// walk, so a blank entry silences everything through the other consumer.
#[test]
fn a_blank_and_a_relative_entry_hide_no_store_from_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let kept = plant(&dir.path().join("p/kept"));
    let config = config_ignoring(&["", "  ", "kept", "~nobody"]);

    let found = discover_nested_stores(&[dir.path().join("p")], &config.ignored_paths());

    assert_eq!(found, std::collections::BTreeSet::from([kept]));
}
