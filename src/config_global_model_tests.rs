//! `daemon.toml` `[embedding] model` as the default for every repository (story 094-9a63).

use super::*;
use crate::llm::embeddings::Model;

#[test]
fn global_model_applies_to_a_repo_that_does_not_choose_one() {
    // catches: the machine-wide model ignored, or its floors left at MiniLM's
    let config = Config::from_toml_with("", Model::Granite107M).unwrap();
    assert_eq!(config.embedding.model, Model::Granite107M);
    assert_eq!(config.hooks.recall_auto_min_cosine, 0.782);
    assert_eq!(config.hooks.recall_docs_min_cosine, 0.77);
    assert_eq!(config.search.memory.min_recall_cosine, 0.70);
}

#[test]
fn repo_model_wins_over_the_global_model() {
    // catches: daemon.toml overriding a repository's explicit choice
    let config =
        Config::from_toml_with("[embedding]\nmodel = \"minilm\"\n", Model::Granite107M).unwrap();
    assert_eq!(config.embedding.model, Model::MiniLm);
    assert_eq!(
        config.hooks.recall_auto_min_cosine,
        RECALL_AUTO_MIN_COSINE_DEFAULT
    );
}

#[test]
fn repo_floor_wins_under_the_global_model() {
    // catches: Granite floors replacing a floor the repository set
    let config = Config::from_toml_with(
        "[hooks]\nrecall_auto_min_cosine = 0.8\n",
        Model::Granite107M,
    )
    .unwrap();
    assert_eq!(config.hooks.recall_auto_min_cosine, 0.8);
    assert_eq!(config.hooks.recall_docs_min_cosine, 0.77);
}

#[test]
fn global_model_is_read_from_daemon_toml_embedding() {
    // catches: wrong section or key read, or a bad value failing every repo open
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("daemon.toml");
    assert_eq!(global_default_model_from(&path), Model::MiniLm);
    std::fs::write(&path, "[embedding]\nmodel = \"granite107m\"\n").unwrap();
    assert_eq!(global_default_model_from(&path), Model::Granite107M);
    std::fs::write(&path, "[embedding]\nmodel = \"e5\"\n").unwrap();
    assert_eq!(global_default_model_from(&path), Model::MiniLm);
}
