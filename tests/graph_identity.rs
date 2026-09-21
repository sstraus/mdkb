//! Identity extraction on index (plan: graph-identity-and-relation-modes, step 2).
//!
//! A document declares its own names in frontmatter — `id:` and `aliases:`.
//! Indexing has to record them, and a re-index has to *replace* them, or a name
//! the author deleted keeps resolving forever.

use std::path::PathBuf;

use mdkb::cli::handlers::{handle_collection_add, handle_init, handle_update};
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
        std::fs::write(self.root.join("docs").join(name), body).expect("write");
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
