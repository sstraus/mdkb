//! `mdkb code dup`: the duplication audit, independent of how it was asked for.
//!
//! The CLI command and the MCP `scope="duplicates"` both land here, so the two
//! surfaces cannot answer differently for the same repository.

use std::path::Path;

use rusqlite::Connection;

use crate::code::duplication::embed::{BodyEmbedder, get_dup_embedder};
use crate::code::duplication::pipeline::{DupOptions, scan_duplication};
use crate::code::duplication::report::{Cluster, render};
use crate::code::duplication::store::DupDb;
use crate::config::Config;
use crate::error::{Error, Result};

/// What the caller overrode on the command line. `None` keeps the configured
/// value, so a flag left off means "whatever `[code.duplication]` says".
#[derive(Debug, Default, Clone)]
pub struct DupOverrides {
    pub threshold: Option<f32>,
    /// Ask for the semantic pass on this run alone.
    pub semantic: bool,
    pub min_nodes: Option<u32>,
    pub file: Option<String>,
    /// Review mode: a git ref whose changed files the report is narrowed to.
    pub since: Option<String>,
}

/// Whether this run loads a model.
///
/// The pass costs about 2.4 CPU-seconds per body and produced 69 of 767
/// clusters on the run that motivated this, so it is loaded only when asked
/// for: `--semantic`, or a threshold override, which tunes the semantic pass
/// and means nothing without it. Config `semantic = true` is the standing
/// opt-in.
///
/// Pure so both surfaces decide identically and the decision can be tested
/// without a model on disk.
pub fn semantic_requested(
    settings: &crate::config::CodeDuplicationConfig,
    overrides: &DupOverrides,
) -> bool {
    settings.semantic || overrides.semantic || overrides.threshold.is_some()
}

/// The audit, ready to print.
#[derive(Debug)]
pub struct DupReport {
    pub markdown: String,
    /// The ranked findings behind `markdown`.
    ///
    /// Carried rather than counted so a caller that wants JSON or CSV renders
    /// the same findings the prose was rendered from, instead of a second
    /// query that could disagree with it.
    pub findings: Vec<Cluster>,
    pub considered: usize,
    pub ignored: usize,
    /// The structural cut the scan ran with, so a caller rendering JSON or CSV
    /// buckets against the same threshold the prose report and the ranking did.
    pub hamming_threshold: u32,
    /// False when there is no code index. The caller reports that and stops;
    /// it is not an error, it is a repository nobody has indexed yet.
    pub indexed: bool,
}

impl DupReport {
    /// How many clusters the report names.
    pub fn clusters(&self) -> usize {
        self.findings.len()
    }
}

/// Run the duplication audit over the repository at `root`.
///
/// `memory` is the docs/memory connection, for the ignore-list. `None` means
/// every accepted cluster comes back, which is what a caller without an index
/// gets and is the safe direction to be wrong in.
pub fn handle_dup(
    root: &Path,
    memory: Option<&Connection>,
    config: &Config,
    overrides: &DupOverrides,
) -> Result<DupReport> {
    let code_path = root.join(".mdkb/code.sqlite");
    let missing = |root: &Path| DupReport {
        markdown: format!(
            "# Duplication\n\nNo code index in {}. Run `mdkb update --code` first.\n",
            root.display()
        ),
        findings: Vec::new(),
        considered: 0,
        ignored: 0,
        // No scan ran, so no cut was used. Carrying the configured one keeps
        // the field honest for a caller that reads it without checking
        // `indexed` — there are no findings to bucket either way.
        hamming_threshold: config.code.duplication.hamming_threshold,
        indexed: false,
    };
    if !code_path.exists() {
        return Ok(missing(root));
    }

    // Announce the connection before opening it. Autoheal quarantines by
    // renaming the path while surviving connections still derive -wal/-shm
    // from it; the shared live lock is what makes quarantine wait instead.
    let _live = crate::store::mutation_lock::acquire_live_shared(&code_path)?;

    let code = Connection::open_with_flags(&code_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| Error::other(format!("cannot open the code index read-only: {e}")))?;
    code.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| Error::other(format!("cannot configure the code index: {e}")))?;

    // `mdkb init` creates the file, so its existence proves nothing. An index
    // holding no symbols is a repository nobody has indexed, and saying "no
    // clusters found" to that would answer a question nobody asked. A missing
    // table counts the same way: whatever that file is, it is not an index.
    let symbols: i64 = code
        .query_row("SELECT COUNT(*) FROM code_symbols", [], |r| r.get(0))
        .unwrap_or(0);
    if symbols == 0 {
        return Ok(missing(root));
    }

    let dup = DupDb::open(root.join(".mdkb/dup.sqlite"))
        .map_err(|e| Error::other(format!("cannot open the duplication cache: {e}")))?;

    // A ref git cannot resolve is an error here, not an empty report: an empty
    // report reads as "your change duplicated nothing", which is the one
    // answer a typo must never be able to produce.
    let changed = match &overrides.since {
        Some(git_ref) => Some(
            crate::git::changed_files(root, git_ref)?
                .into_iter()
                .collect(),
        ),
        None => None,
    };

    // An anchor that names no indexed file would print "No clusters found",
    // which reads as "this file has no clones". Say what actually happened.
    if let Some(prefix) = &overrides.file {
        let like = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
        let indexed: i64 = code
            .query_row(
                "SELECT COUNT(*) FROM code_symbols WHERE file_path LIKE ?1 ESCAPE '\\'",
                [&like],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if indexed == 0 {
            return Ok(DupReport {
                markdown: format!(
                    "# Duplication\n\nNo indexed symbol under `{prefix}`. Check the path, or run `mdkb update --code`.\n"
                ),
                findings: Vec::new(),
                considered: 0,
                ignored: 0,
                hamming_threshold: config.code.duplication.hamming_threshold,
                indexed: true,
            });
        }
    }

    let settings = &config.code.duplication;
    let options = DupOptions {
        min_nodes: overrides.min_nodes.unwrap_or(settings.min_nodes),
        hamming_threshold: settings.hamming_threshold,
        similarity_threshold: overrides.threshold.unwrap_or(settings.similarity_threshold),
        file: overrides.file.clone(),
        changed,
        ..DupOptions::default()
    };

    // A model that will not load degrades the pass to its structural half
    // rather than failing it. That half needs no weights and, on a repository
    // of copy-paste, finds most of the answer — refusing to report anything
    // because a download failed would be the worse trade.
    let embedder: Option<std::sync::Arc<_>> = if semantic_requested(settings, overrides) {
        match get_dup_embedder(&settings.model) {
            Ok(embedder) => Some(embedder),
            Err(e) => {
                tracing::warn!("duplication model unavailable, structural pass only: {e}");
                None
            }
        }
    } else {
        None
    };

    // Only a run that will actually read vectors reconciles the signature. A
    // structural run never touches the embedding column, so clearing it there
    // would throw away a cache nobody was about to use.
    if let Some(embedder) = &embedder {
        let signature = crate::code::duplication::embed::embedding_signature(embedder.model_name());
        match dup.reconcile_embedding_signature(&signature) {
            Ok(0) => {}
            Ok(cleared) => tracing::info!(
                "duplication embeddings were produced by a different configuration; \
                 {cleared} dropped, re-embedding against {signature}"
            ),
            Err(e) => tracing::warn!("cannot reconcile the duplication embedding signature: {e}"),
        }
    }

    let repo = root.to_path_buf();
    let mut read_file = |path: &str| read_indexed(&repo, path);

    let outcome = scan_duplication(
        &code,
        memory,
        &dup,
        embedder.as_ref().map(|e| e.as_ref() as &dyn BodyEmbedder),
        &mut read_file,
        &options,
        chrono::Utc::now().timestamp(),
    )?;

    let markdown = render(
        &outcome.clusters,
        options.hamming_threshold,
        &mut |candidate| {
            let source = read_indexed(&repo, &candidate.file_path)?;
            crate::code::duplication::body::body_text(
                &source,
                candidate.line_start,
                candidate.line_end,
            )
            .map(str::to_string)
        },
    );

    Ok(DupReport {
        markdown,
        findings: outcome.clusters,
        considered: outcome.considered,
        ignored: outcome.ignored,
        hamming_threshold: options.hamming_threshold,
        indexed: true,
    })
}

/// Read a path the index recorded.
///
/// The index stores whatever path it was given, which is relative for an
/// ordinary `mdkb code index src/` and absolute for a caller that passed one.
/// Trying the path as written after the repository-relative one covers both
/// without the caller having to know which kind its index holds.
fn read_indexed(root: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(root.join(path))
        .or_else(|_| std::fs::read_to_string(path))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repository_with_no_code_index_is_reported_not_refused() {
        let root = tempfile::tempdir().unwrap();

        let report = handle_dup(
            root.path(),
            None,
            &Config::default(),
            &DupOverrides::default(),
        )
        .unwrap();

        assert!(!report.indexed);
        assert_eq!(report.clusters(), 0);
        assert!(
            report.markdown.contains("No code index"),
            "{}",
            report.markdown
        );
        assert!(
            report.markdown.contains("mdkb update --code"),
            "and says what to do about it: {}",
            report.markdown
        );
    }

    /// A code index with the schema and no rows.
    ///
    /// `mdkb init` leaves exactly this behind, so it is the state a first
    /// `mdkb code dup` in a fresh repository actually meets.
    fn empty_index(root: &Path) {
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        let code = Connection::open(root.join(".mdkb/code.sqlite")).unwrap();
        crate::code::storage::schema::init_schema(&code).unwrap();
        drop(code);
    }

    /// An index holding one function, and the file it was read from.
    ///
    /// Enough to get past `handle_dup`'s "nobody has indexed this" guard and
    /// reach the point where it decides whether to build an embedder.
    fn index_with_one_symbol(root: &Path) {
        empty_index(root);
        let body = "fn alpha_total(xs: &[i64]) -> i64 {\n    xs.iter().sum()\n}\n";
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), body).unwrap();

        let code = Connection::open(root.join(".mdkb/code.sqlite")).unwrap();
        code.execute(
            "INSERT INTO code_files (id, path, rel_path, hash, language) \
             VALUES (1, 'src/a.rs', 'src/a.rs', 'h', 'rust')",
            [],
        )
        .unwrap();
        code.execute(
            "INSERT INTO code_symbols \
             (id, name, kind, file_id, file_path, module_path, owner_name, visibility, line_start, line_end) \
             VALUES (1, 'alpha_total', 'Function', 1, 'src/a.rs', 'alpha', NULL, 0, 1, 3)",
            [],
        )
        .unwrap();
    }

    /// No model: these fixtures have nothing worth embedding, and a unit test
    /// must not reach the network to download weights. This is the default
    /// now — the call stays so the intent is on the page rather than resting
    /// on a default that could change.
    fn structural_only() -> Config {
        let mut config = Config::default();
        config.code.duplication.semantic = false;
        config
    }

    #[test]
    fn an_index_holding_no_symbols_is_reported_as_no_index() {
        let root = tempfile::tempdir().unwrap();
        empty_index(root.path());

        let report = handle_dup(
            root.path(),
            None,
            &structural_only(),
            &DupOverrides::default(),
        )
        .unwrap();

        // The file exists — `mdkb init` made it — so existence cannot be the
        // test. "No clusters found" here would answer a question nobody asked.
        assert!(!report.indexed, "{}", report.markdown);
        assert!(
            report.markdown.contains("No code index"),
            "{}",
            report.markdown
        );
    }

    #[test]
    fn an_indexed_repository_with_nothing_duplicated_reports_no_clusters() {
        let root = tempfile::tempdir().unwrap();
        empty_index(root.path());
        let code = Connection::open(root.path().join(".mdkb/code.sqlite")).unwrap();
        code.execute(
            "INSERT INTO code_files (id, path, rel_path, hash, language) \
             VALUES (1, 'src/a.rs', 'src/a.rs', 'h', 'rust')",
            [],
        )
        .unwrap();
        code.execute(
            "INSERT INTO code_symbols (id, name, kind, file_id, file_path, visibility, line_start, line_end) \
             VALUES (1, 'only', 'Function', 1, 'src/a.rs', 0, 0, 6)",
            [],
        )
        .unwrap();
        drop(code);

        let report = handle_dup(
            root.path(),
            None,
            &structural_only(),
            &DupOverrides::default(),
        )
        .unwrap();

        assert!(report.indexed, "{}", report.markdown);
        assert_eq!(report.clusters(), 0);
        assert!(
            report.markdown.contains("No clusters found"),
            "{}",
            report.markdown
        );
    }

    /// The four ways the semantic pass can be asked for, and the one way it is
    /// not. Pure, so this needs no model on disk — which is the point: the
    /// decision to load one is made before anything touches the network.
    #[test]
    fn the_semantic_pass_is_off_unless_something_asks_for_it() {
        let mut settings = crate::config::CodeDuplicationConfig::default();

        assert!(
            !semantic_requested(&settings, &DupOverrides::default()),
            "a default run must not load a model"
        );
        assert!(
            semantic_requested(
                &settings,
                &DupOverrides {
                    semantic: true,
                    ..Default::default()
                }
            ),
            "--semantic asks for it"
        );
        assert!(
            semantic_requested(
                &settings,
                &DupOverrides {
                    threshold: Some(0.8),
                    ..Default::default()
                }
            ),
            "a threshold tunes the semantic pass, so it implies it"
        );

        settings.semantic = true;
        assert!(
            semantic_requested(&settings, &DupOverrides::default()),
            "config semantic = true is the standing opt-in"
        );
    }

    /// A default `mdkb code dup` never constructs the embedder.
    ///
    /// The model name is one `fastembed_model` rejects outright, so building it
    /// would fail without reaching the network. `handle_dup` swallows that
    /// failure by design — it degrades to the structural half rather than
    /// refusing to report — so the report alone cannot distinguish "never
    /// loaded" from "tried and gave up". What it does prove is the guarantee a
    /// caller has: the default path returns a complete structural report on a
    /// store whose configured model could never load. The guard itself is
    /// `semantic_requested`, tested exhaustively above, at its single call site.
    #[test]
    fn a_default_run_reports_even_when_the_configured_model_cannot_load() {
        let root = tempfile::tempdir().unwrap();
        index_with_one_symbol(root.path());

        let mut config = Config::default();
        config.code.duplication.model = "not-a-model-anyone-ships".to_string();
        assert!(
            !config.code.duplication.semantic,
            "the default this test rests on"
        );

        let report = handle_dup(root.path(), None, &config, &DupOverrides::default())
            .expect("a default run must not fail on a model it never loads");
        assert!(report.indexed, "{}", report.markdown);
    }

    #[test]
    fn command_line_options_win_over_the_configured_ones() {
        // Proven through the option struct the scan actually receives, rather
        // than by reading the report: the two numbers do not show up there.
        let settings = crate::config::CodeDuplicationConfig::default();
        let overrides = DupOverrides {
            semantic: false,
            threshold: Some(0.9),
            min_nodes: Some(3),
            file: Some("src/a.rs".into()),
            since: None,
        };

        let options = DupOptions {
            min_nodes: overrides.min_nodes.unwrap_or(settings.min_nodes),
            hamming_threshold: settings.hamming_threshold,
            similarity_threshold: overrides.threshold.unwrap_or(settings.similarity_threshold),
            file: overrides.file.clone(),
            ..DupOptions::default()
        };

        assert_eq!(options.min_nodes, 3);
        assert!((options.similarity_threshold - 0.9).abs() < f32::EPSILON);
        assert_eq!(options.file.as_deref(), Some("src/a.rs"));
        // And an option left off keeps the configured value.
        let none = DupOverrides::default();
        assert_eq!(
            none.min_nodes.unwrap_or(settings.min_nodes),
            settings.min_nodes
        );
    }

    #[test]
    fn an_indexed_path_is_read_relative_to_the_repository() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/a.rs"), "fn a() {}\n").unwrap();

        assert_eq!(
            read_indexed(root.path(), "src/a.rs").as_deref(),
            Some("fn a() {}\n")
        );
        assert_eq!(read_indexed(root.path(), "src/missing.rs"), None);
    }
}
