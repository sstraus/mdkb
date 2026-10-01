//! `forbid_load` (story 202-4c67, round 2). Its own test binary: the flag is
//! process-wide and one-way.

use mdkb::llm::rerank::{MODEL_REPO, MODEL_REVISION, forbid_load, shared};

#[test]
fn a_one_shot_process_with_cached_weights_never_leaves_idle() {
    // Catches: the one-shot check placed after the cache check or after the
    // thread spawn, so the in-process hook fallback still starts the 1 GB load
    // (the call would answer `loading` or, once the garbage weights fail,
    // `load_failed`, never `one_shot`).
    let home = tempfile::tempdir().unwrap();
    let snapshot = home
        .path()
        .join(format!("models--{}", MODEL_REPO.replace('/', "--")))
        .join("snapshots")
        .join(MODEL_REVISION);
    std::fs::create_dir_all(snapshot.join("onnx")).unwrap();
    for file in [
        "onnx/model_int8.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ] {
        std::fs::write(snapshot.join(file), b"not a model").unwrap();
    }
    // SAFETY: the only test in this binary, so nothing reads the environment
    // concurrently.
    unsafe { std::env::set_var("HF_HOME", home.path()) };

    forbid_load();
    let reranker = shared();
    let docs = vec!["d".to_string()];
    for _ in 0..2 {
        let error = reranker.score("q", &docs).unwrap_err();
        assert_eq!(error.outcome(), "one_shot");
    }
    // A load thread started anywhere above would have failed on the garbage
    // weights by now and turned the answer into `load_failed`.
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert_eq!(
        reranker.score("q", &docs).unwrap_err().outcome(),
        "one_shot"
    );
}
