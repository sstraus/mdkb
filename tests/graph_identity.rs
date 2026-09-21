//! Identity extraction on index (plan: graph-identity-and-relation-modes, step 2).
//!
//! A document declares its own names in frontmatter — `id:` and `aliases:`.
//! Indexing has to record them, and a re-index has to *replace* them, or a name
//! the author deleted keeps resolving forever.

use std::path::PathBuf;

use mdkb::cli::handlers::{handle_collection_add, handle_init, handle_update};
use mdkb::core::indexing::handle_update_files;
use mdkb::core::Context;
use tempfile::TempDir;

struct Env {
    _dir: TempDir,
    root: PathBuf,
    ctx: Context,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        handle_init(&root).expect("init");
        let ctx = Context::open(&root).expect("open");
        std::fs::create_dir_all(root.join("docs")).expect("mkdir");
        handle_collection_add(&ctx, "docs", "docs", "**/*.md").expect("add collection");
        Self {
            _dir: dir,
            root,
            ctx,
        }
    }

    fn write(&self, name: &str, body: &str) {
        let path = self.root.join("docs").join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, body).expect("write");
    }

    /// Rewrite a file and push its mtime into the future.
    ///
    /// `index_single_file` re-indexes on `file_mtime > doc.indexed_at`, both in
    /// whole seconds. A test that writes twice inside one second is skipped and
    /// the assertion that follows measures nothing.
    fn rewrite(&self, name: &str, body: &str) {
        let path = self.root.join("docs").join(name);
        std::fs::write(&path, body).expect("write");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open");
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .expect("set mtime");
    }

    fn update(&self) {
        handle_update(&self.ctx, &self.root).expect("update");
    }

    /// Every outgoing edge of the named document as `(relation, target)`.
    fn edges(&self, relative_path: &str) -> Vec<(String, String)> {
        let mut stmt = self
            .ctx
            .conn
            .prepare(
                "SELECT e.relation, e.target_ref
                 FROM edges e JOIN documents d ON d.id = e.source_doc_id
                 WHERE d.relative_path = ?1
                 ORDER BY e.relation, e.target_ref",
            )
            .expect("prepare");
        stmt.query_map([relative_path], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect")
    }

    /// `(alias, source_key)` for every identity the named document declares.
    fn aliases(&self, relative_path: &str) -> Vec<(String, String)> {
        let mut stmt = self
            .ctx
            .conn
            .prepare(
                "SELECT a.alias, a.source_key
                 FROM document_aliases a
                 JOIN documents d ON d.id = a.doc_id
                 WHERE d.relative_path = ?1
                 ORDER BY a.source_key, a.alias",
            )
            .expect("prepare");
        stmt.query_map([relative_path], |r| Ok((r.get(0)?, r.get(1)?)))
            .expect("query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect")
    }
}

#[test]
fn indexing_records_the_identities_a_document_declares() {
    let env = Env::new();
    env.write(
        "alice.md",
        "---\nid: person:alice\naliases: [\"@alice\", alice@example.com]\ntype: person\n---\n\n# Alice\n",
    );
    env.update();

    assert_eq!(
        env.aliases("alice.md"),
        vec![
            ("@alice".to_string(), "aliases".to_string()),
            ("alice@example.com".to_string(), "aliases".to_string()),
            ("person:alice".to_string(), "id".to_string()),
        ],
        "`id` and every `aliases` entry are recorded; `type` is not an identity"
    );
}

#[test]
fn a_reindex_replaces_identities_instead_of_accumulating_them() {
    let env = Env::new();
    env.write(
        "alice.md",
        "---\nid: person:alice\naliases: [\"@alice\", alice@example.com]\n---\n\n# Alice\n",
    );
    env.update();
    assert_eq!(env.aliases("alice.md").len(), 3, "three names to start");

    // The author drops `aliases:`. A name that is no longer declared must stop
    // resolving -- an accumulate-only table would keep answering to it.
    env.rewrite("alice.md", "---\nid: person:alice\n---\n\n# Alice\n");
    env.update();

    assert_eq!(
        env.aliases("alice.md"),
        vec![("person:alice".to_string(), "id".to_string())],
        "only the still-declared `id` survives the re-index"
    );
}

#[test]
fn reindexing_an_unchanged_document_is_idempotent() {
    let env = Env::new();
    env.write("alice.md", "---\nid: person:alice\n---\n\n# Alice\n");
    env.update();
    env.update();

    assert_eq!(
        env.aliases("alice.md"),
        vec![("person:alice".to_string(), "id".to_string())],
        "a second pass must not duplicate the row"
    );
}

#[test]
fn a_document_declaring_nothing_claims_no_identity() {
    let env = Env::new();
    env.write("note.md", "---\ntype: note\ntitle: Plain\n---\n\n# Plain\n");
    env.update();

    assert!(
        env.aliases("note.md").is_empty(),
        "no identity key, no row -- metadata must not become an identity"
    );
}

#[test]
fn two_documents_may_claim_the_same_identity() {
    // A repository defect to report, never an index that fails: refusing the
    // second insert would make one bad document break the whole update.
    let env = Env::new();
    env.write("a.md", "---\nid: person:alice\n---\n\n# A\n");
    env.write("b.md", "---\nid: person:alice\n---\n\n# B\n");
    env.update();

    assert_eq!(env.aliases("a.md").len(), 1);
    assert_eq!(env.aliases("b.md").len(), 1);
}

#[test]
fn identity_keys_are_configurable() {
    let env = Env::new();
    let config_path = env.root.join(".mdkb/config.toml");
    let config = std::fs::read_to_string(&config_path).expect("read config");
    // Appending at the end of the file would land the key in whichever table
    // happens to be last; it has to sit under `[graph]`.
    // The shipped config is every default commented out, so `[graph]` exists
    // only as `# [graph]`. Appending a live section is the way to set one key.
    assert!(
        config.contains("# [graph]"),
        "the shipped config comments every default out"
    );
    std::fs::write(
        &config_path,
        format!("{config}\n[graph]\nidentity_keys = [\"slug\"]\n"),
    )
    .expect("write config");

    let ctx = Context::open(&env.root).expect("reopen");
    env.write(
        "alice.md",
        "---\nid: person:alice\nslug: alice\n---\n\n# Alice\n",
    );
    handle_update(&ctx, &env.root).expect("update");

    let env = Env {
        _dir: env._dir,
        root: env.root,
        ctx,
    };
    assert_eq!(
        env.aliases("alice.md"),
        vec![("alice".to_string(), "slug".to_string())],
        "the configured keys are the identity keys -- `id` is only the default"
    );
}

/// Widening the allowlist must reach documents already indexed.
///
/// Before the edge pass this silently did nothing: extraction was welded to
/// document indexing, and `update` skips a file whose mtime has not moved. The
/// user read a config they had edited and a graph that ignored it.
#[test]
fn widening_the_allowlist_applies_without_force() {
    let env = Env::new();
    env.write(
        "a.md",
        "---\nowner: alice\norg: org:acme\n---\n\n# A\n",
    );
    env.update();
    assert_eq!(
        env.edges("a.md"),
        vec![("owner".to_string(), "alice".to_string())],
        "`org` is not in the default allowlist yet"
    );

    let config_path = env.root.join(".mdkb/config.toml");
    let config = std::fs::read_to_string(&config_path).expect("read config");
    std::fs::write(
        &config_path,
        format!(
            "{config}\n[graph]\nfrontmatter_relations = [\"owner\", \"org\"]\n"
        ),
    )
    .expect("write config");

    // No `--force`, and not one file has changed.
    let ctx = Context::open(&env.root).expect("reopen");
    handle_update(&ctx, &env.root).expect("update");
    let env = Env {
        _dir: env._dir,
        root: env.root,
        ctx,
    };

    assert_eq!(
        env.edges("a.md"),
        vec![
            ("org".to_string(), "org:acme".to_string()),
            ("owner".to_string(), "alice".to_string()),
        ],
        "the widened allowlist reaches a document nothing touched"
    );
}

#[test]
fn narrowing_the_allowlist_removes_the_edges_it_dropped() {
    let env = Env::new();
    let config_path = env.root.join(".mdkb/config.toml");
    let base = std::fs::read_to_string(&config_path).expect("read config");
    std::fs::write(
        &config_path,
        format!("{base}\n[graph]\nfrontmatter_relations = [\"owner\", \"org\"]\n"),
    )
    .expect("write config");
    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.write("a.md", "---\nowner: alice\norg: org:acme\n---\n\n# A\n");
    env.update();
    assert_eq!(env.edges("a.md").len(), 2);

    std::fs::write(
        &config_path,
        format!("{base}\n[graph]\nfrontmatter_relations = [\"owner\"]\n"),
    )
    .expect("write config");
    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    handle_update(&env.ctx, &env.root).expect("update");

    assert_eq!(
        env.edges("a.md"),
        vec![("owner".to_string(), "alice".to_string())],
        "a key removed from the allowlist stops producing edges"
    );
}

#[test]
fn a_wikilink_edge_survives_the_frontmatter_pass() {
    // The pass owns frontmatter edges because those are the ones the config
    // decides. Wikilinks come from the body and must not be collateral.
    let env = Env::new();
    env.write("a.md", "---\nowner: alice\n---\n\nSee [[b]].\n");
    env.update();

    let mut found = env.edges("a.md");
    found.sort();
    assert_eq!(
        found,
        vec![
            ("links_to".to_string(), "b".to_string()),
            ("owner".to_string(), "alice".to_string()),
        ],
        "both kinds coexist after a full update"
    );

    // A second update re-runs the pass over an unchanged corpus.
    env.update();
    let mut again = env.edges("a.md");
    again.sort();
    assert_eq!(again, found, "the pass is idempotent and spares the wikilink");
}

#[test]
fn a_second_update_does_not_change_the_edge_count() {
    let env = Env::new();
    env.write("a.md", "---\nowner: alice\nrelated: [b.md, c.md]\n---\n\n[[b]]\n");
    env.write("b.md", "---\nowner: bob\n---\n\n# B\n");
    env.update();
    let first: i64 = env
        .ctx
        .conn
        .query_row("SELECT count(*) FROM edges", [], |r| r.get(0))
        .unwrap();

    env.update();
    let second: i64 = env
        .ctx
        .conn
        .query_row("SELECT count(*) FROM edges", [], |r| r.get(0))
        .unwrap();

    assert_eq!(first, second, "mdkb update twice must be idempotent");
    assert!(first > 0, "the fixture must actually produce edges");
}

/// The single-file path is what the daemon watcher uses on every save.
///
/// It runs in its own transaction and never sees the whole-corpus pass, so it
/// has to build the document's own frontmatter edges itself. Without this a
/// newly saved file had wikilinks and no relations until someone happened to
/// run a full `update`.
#[test]
fn indexing_one_named_file_builds_its_frontmatter_edges() {
    let env = Env::new();
    env.write("a.md", "---\nowner: alice\n---\n\nSee [[b]].\n");

    handle_update_files(&env.ctx, &env.root, &["docs/a.md".to_string()]).expect("update --files");

    let mut found = env.edges("a.md");
    found.sort();
    assert_eq!(
        found,
        vec![
            ("links_to".to_string(), "b".to_string()),
            ("owner".to_string(), "alice".to_string()),
        ],
        "a file indexed by name gets both edge kinds"
    );

    // And re-saving it does not duplicate either kind.
    env.rewrite("a.md", "---\nowner: alice\n---\n\nSee [[b]].\n");
    handle_update_files(&env.ctx, &env.root, &["docs/a.md".to_string()]).expect("update --files");
    let mut again = env.edges("a.md");
    again.sort();
    assert_eq!(again, found, "a second save is idempotent on both kinds");
}

/// Write a live `[graph]` section on top of the shipped commented config.
fn set_graph_config(root: &std::path::Path, body: &str) {
    let path = root.join(".mdkb/config.toml");
    let base = std::fs::read_to_string(&path).expect("read config");
    let base = match base.find("\n[graph]\n") {
        Some(at) => base[..at].to_string(),
        None => base,
    };
    std::fs::write(&path, format!("{base}\n[graph]\n{body}\n")).expect("write config");
}

/// Under `auto` a relation key the user never declared is still extracted —
/// and the config file is not edited to record that.
#[test]
fn auto_extracts_a_detected_key_without_touching_the_config() {
    let env = Env::new();
    env.write("orgs/acme.md", "---\nid: org:acme\n---\n\n# Acme\n");
    env.write("m1.md", "---\nowner: alice\norg: [org:acme]\n---\n\n# M1\n");
    env.write("m2.md", "---\nowner: bob\norg: [org:acme]\n---\n\n# M2\n");
    set_graph_config(&env.root, "relations = \"auto\"\nfrontmatter_relations = [\"owner\"]");

    let config_path = env.root.join(".mdkb/config.toml");
    let before = std::fs::read_to_string(&config_path).expect("read config");

    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.update();

    let mut found = env.edges("m1.md");
    found.sort();
    assert_eq!(
        found,
        vec![
            ("org".to_string(), "org:acme".to_string()),
            ("owner".to_string(), "alice".to_string()),
        ],
        "`org` was detected, not declared, and still produced an edge"
    );

    assert_eq!(
        std::fs::read_to_string(&config_path).expect("read config"),
        before,
        "auto must never rewrite the user's config to record its own guess"
    );
}

/// Under `manual` the same corpus yields only what the user declared.
#[test]
fn manual_extracts_only_the_declared_keys() {
    let env = Env::new();
    env.write("orgs/acme.md", "---\nid: org:acme\n---\n\n# Acme\n");
    env.write("m1.md", "---\nowner: alice\norg: [org:acme]\n---\n\n# M1\n");
    set_graph_config(&env.root, "relations = \"manual\"\nfrontmatter_relations = [\"owner\"]");

    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.update();

    assert_eq!(
        env.edges("m1.md"),
        vec![("owner".to_string(), "alice".to_string())],
        "manual ignores the detector"
    );
}

/// `semi` reports but does not extract — same extraction as `manual`.
#[test]
fn semi_extracts_only_the_declared_keys() {
    let env = Env::new();
    env.write("orgs/acme.md", "---\nid: org:acme\n---\n\n# Acme\n");
    env.write("m1.md", "---\nowner: alice\norg: [org:acme]\n---\n\n# M1\n");
    set_graph_config(&env.root, "relations = \"semi\"\nfrontmatter_relations = [\"owner\"]");

    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.update();

    assert_eq!(
        env.edges("m1.md"),
        vec![("owner".to_string(), "alice".to_string())]
    );
}

/// The watcher route must agree with the full update, or a saved file would
/// keep a different graph from the one `mdkb update` builds.
#[test]
fn the_single_file_route_honours_auto_too() {
    let env = Env::new();
    env.write("orgs/acme.md", "---\nid: org:acme\n---\n\n# Acme\n");
    env.write("m1.md", "---\nowner: alice\norg: [org:acme]\n---\n\n# M1\n");
    set_graph_config(&env.root, "relations = \"auto\"\nfrontmatter_relations = [\"owner\"]");
    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    // Index the org first so its identity exists to resolve against.
    handle_update_files(&env.ctx, &env.root, &["docs/orgs/acme.md".to_string()]).expect("files");
    handle_update_files(&env.ctx, &env.root, &["docs/m1.md".to_string()]).expect("files");

    let mut found = env.edges("m1.md");
    found.sort();
    assert_eq!(
        found,
        vec![
            ("org".to_string(), "org:acme".to_string()),
            ("owner".to_string(), "alice".to_string()),
        ],
        "a saved file gets the same graph a full update would build"
    );
}

/// The detection SessionStart reads has to be written by `update`, and it has
/// to know which keys are already extracted — otherwise the hook advertises
/// keys that are producing edges already.
#[test]
fn update_records_what_the_detector_measured() {
    let env = Env::new();
    env.write("orgs/acme.md", "---\nid: org:acme\n---\n\n# Acme\n");
    env.write("m1.md", "---\nowner: alice\norg: [org:acme]\ntype: meeting\n---\n\n# M1\n");
    set_graph_config(&env.root, "relations = \"semi\"\nfrontmatter_relations = [\"owner\"]");
    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.update();

    let undetected = mdkb::store::graph::undetected_relation_keys(&env.ctx.conn).unwrap();
    let keys: Vec<&str> = undetected.iter().map(|r| r.key.as_str()).collect();
    assert_eq!(
        keys,
        vec!["org"],
        "`org` resolves and is not extracted; `owner` is extracted; `type` resolves to nothing"
    );
}

/// Under `auto` every detected key IS extracted, so there is nothing to report
/// and the hook must stay quiet.
#[test]
fn auto_leaves_nothing_undetected_to_report() {
    let env = Env::new();
    env.write("orgs/acme.md", "---\nid: org:acme\n---\n\n# Acme\n");
    env.write("m1.md", "---\nowner: alice\norg: [org:acme]\n---\n\n# M1\n");
    set_graph_config(&env.root, "relations = \"auto\"\nfrontmatter_relations = [\"owner\"]");
    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.update();

    assert!(
        mdkb::store::graph::undetected_relation_keys(&env.ctx.conn)
            .unwrap()
            .is_empty(),
        "auto extracts what it detects, so nothing is outstanding"
    );
}

/// `manual` means stop measuring. A stale row from an earlier `semi` run must
/// not survive the switch and keep being reported.
#[test]
fn switching_to_manual_clears_what_was_measured() {
    let env = Env::new();
    env.write("orgs/acme.md", "---\nid: org:acme\n---\n\n# Acme\n");
    env.write("m1.md", "---\nowner: alice\norg: [org:acme]\n---\n\n# M1\n");
    set_graph_config(&env.root, "relations = \"semi\"\nfrontmatter_relations = [\"owner\"]");
    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.update();
    assert!(!mdkb::store::graph::undetected_relation_keys(&env.ctx.conn).unwrap().is_empty());

    set_graph_config(&env.root, "relations = \"manual\"\nfrontmatter_relations = [\"owner\"]");
    let env = Env {
        ctx: Context::open(&env.root).expect("reopen"),
        _dir: env._dir,
        root: env.root,
    };
    env.update();

    assert!(
        mdkb::store::graph::undetected_relation_keys(&env.ctx.conn)
            .unwrap()
            .is_empty(),
        "manual must leave nothing behind for the hook to read"
    );
}
