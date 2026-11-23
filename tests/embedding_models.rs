//! Model identity is a storage invariant, including encoders with equal dimensions.
use mdkb::llm::embeddings::Model;
use mdkb::store::{schema, vectors};
use rusqlite::Connection;

fn store() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    vectors::init_sqlite_vec();
    let conn = Connection::open(dir.path().join("index.sqlite")).unwrap();
    schema::init_schema(&conn).unwrap();
    vectors::init_vector_schema(&conn).unwrap();
    conn.execute_batch("INSERT INTO collections(name,path,pattern,created_at,updated_at) VALUES('notes','.','*.md',0,0);
        INSERT INTO content(hash,body,created_at) VALUES('hash','document body',0);
        INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(1,'notes','note.md','hash',0,0);
        INSERT INTO memory_entries(id,title,content,entry_type,tags,status,created_at,updated_at) VALUES('memory','Memory title','Memory body','topic','[]','active',0,0);").unwrap();
    (dir, conn)
}

fn select(dir: &tempfile::TempDir, model: &str) {
    std::fs::write(
        dir.path().join("config.toml"),
        format!("[embedding]\nmodel = '{model}'\n"),
    )
    .unwrap();
}

fn seed(conn: &Connection) {
    vectors::store_embedding(conn, 1, &[0.25; 384], "AllMiniLML6V2").unwrap();
    vectors::store_memory_embedding(conn, 1, &[0.5; 384], "AllMiniLML6V2").unwrap();
}

#[test]
fn embedding_models_legacy_minilm_identity_does_not_discard_vectors() {
    // Catches needless migration of the pre-model-selection MiniLM row format.
    let (_dir, conn) = store();
    seed(&conn);
    conn.execute("DELETE FROM embedding_identity", []).unwrap();
    vectors::init_vector_schema(&conn).unwrap();
    let status = vectors::identity_status(&conn).unwrap();
    assert_eq!(status.current_model.as_deref(), Some("AllMiniLML6V2"));
    assert!(!status.stale);
    assert_eq!(
        vectors::get_embedding(&conn, 1).unwrap().unwrap(),
        vec![0.25; 384]
    );
    assert_eq!(
        vectors::memory_vector_search(&conn, &[0.5; 384], 5, None)
            .unwrap()
            .len(),
        1
    );
    let persisted: String = conn
        .query_row("SELECT current_model FROM embedding_identity", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(persisted, "AllMiniLML6V2");
}

#[test]
fn embedding_models_equal_dimensions_never_allow_cross_model_reads_or_writes() {
    // Catches using dimension equality as identity, including a stale live connection.
    let (dir, conn) = store();
    seed(&conn);
    select(&dir, "granite107m");
    let status = vectors::identity_status(&conn).unwrap();
    assert!(status.stale);
    assert_eq!(status.current_model.as_deref(), Some("AllMiniLML6V2"));
    for model in [Model::MiniLm, Model::Granite107M] {
        assert!(
            vectors::memory_vector_search_for_model(&conn, &[0.5; 384], 5, None, model).is_err()
        );
        assert!(vectors::store_memory_embedding(&conn, 1, &[0.5; 384], model.identity()).is_err());
        assert!(vectors::store_embedding(&conn, 1, &[0.25; 384], model.identity()).is_err());
    }
    assert_eq!(vectors::count_embeddings(&conn).unwrap(), 1);
}

#[test]
fn embedding_models_pending_rebuild_survives_reopen_and_blocks_automatic_backfill() {
    // Catches a partial or interrupted rebuild becoming queryable or resuming from a hook.
    let (dir, conn) = store();
    seed(&conn);
    select(&dir, "granite107m");
    vectors::begin_reembed(&conn).unwrap();
    drop(conn);
    let conn = Connection::open(dir.path().join("index.sqlite")).unwrap();
    vectors::init_vector_schema(&conn).unwrap();
    let status = vectors::identity_status(&conn).unwrap();
    assert_eq!(
        status.pending_model.as_deref(),
        Some(Model::Granite107M.identity())
    );
    assert!(mdkb::store::memory::backfill_memory_embeddings(&conn).is_err());
    vectors::store_embedding(&conn, 1, &[0.25; 384], Model::Granite107M.identity()).unwrap();
    vectors::store_memory_embedding(&conn, 1, &[0.5; 384], Model::Granite107M.identity()).unwrap();
    assert!(
        vectors::memory_vector_search_for_model(&conn, &[0.5; 384], 5, None, Model::Granite107M)
            .is_err()
    );
    vectors::finish_reembed(&conn).unwrap();
    assert!(!vectors::identity_status(&conn).unwrap().stale);
    assert_eq!(
        vectors::memory_vector_search_for_model(&conn, &[0.5; 384], 5, None, Model::Granite107M)
            .unwrap()
            .len(),
        1
    );
    assert!(vectors::memory_vector_search(&conn, &[0.5; 384], 5, None).is_err());
}

#[test]
fn embedding_models_finish_cannot_certify_missing_document_or_memory_vectors() {
    // Catches a partial but unambiguous generation being certified as current.
    let mut failures = Vec::new();
    for missing in ["document", "memory", "both"] {
        let (_dir, conn) = store();
        vectors::begin_reembed(&conn).unwrap();
        let model = Model::MiniLm.identity();
        if missing == "memory" {
            vectors::store_embedding(&conn, 1, &[0.25; 384], model).unwrap();
        }
        if missing == "document" {
            vectors::store_memory_embedding(&conn, 1, &[0.5; 384], model).unwrap();
        }
        if vectors::finish_reembed(&conn).is_ok() {
            failures.push(format!("{missing}: incomplete generation reported success"));
            continue;
        }
        let status = vectors::identity_status(&conn).unwrap();
        if !status.stale || status.pending_model.as_deref() != Some(model) {
            failures.push(format!("{missing}: failed finish lost pending protection"));
        }
        if vectors::vector_search(&conn, &[0.25; 384], 1).is_ok() {
            failures.push(format!("{missing}: partial generation became queryable"));
        }
        if missing != "memory" {
            vectors::store_embedding(&conn, 1, &[0.25; 384], model).unwrap();
        }
        if missing != "document" {
            vectors::store_memory_embedding(&conn, 1, &[0.5; 384], model).unwrap();
        }
        if let Err(error) = vectors::finish_reembed(&conn) {
            failures.push(format!("{missing}: complete generation refused: {error}"));
        }
        if vectors::identity_status(&conn).unwrap().stale {
            failures.push(format!("{missing}: completed generation stayed stale"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn embedding_models_finish_refuses_a_changed_selection_and_preserves_pending() {
    // Catches finish reporting success for a generation whose pending model no
    // longer matches the selected encoder, even when every vector is present.
    let (dir, conn) = store();
    select(&dir, "granite107m");
    vectors::begin_reembed(&conn).unwrap();
    vectors::store_embedding(&conn, 1, &[0.25; 384], Model::Granite107M.identity()).unwrap();
    vectors::store_memory_embedding(&conn, 1, &[0.5; 384], Model::Granite107M.identity()).unwrap();

    select(&dir, "minilm");
    let error = vectors::finish_reembed(&conn).unwrap_err();
    assert!(error.to_string().contains("re-embedding is incomplete"));
    let status = vectors::identity_status(&conn).unwrap();
    assert!(status.stale);
    assert_eq!(
        status.pending_model.as_deref(),
        Some(Model::Granite107M.identity())
    );

    select(&dir, "granite107m");
    vectors::finish_reembed(&conn).unwrap();
    let status = vectors::identity_status(&conn).unwrap();
    assert!(!status.stale);
    assert!(status.pending_model.is_none());
    assert_eq!(
        vectors::vector_search_for_model(&conn, &[0.25; 384], 1, None, Model::Granite107M)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn embedding_models_ambiguous_legacy_labels_and_orphan_chunks_fail_closed() {
    // Catches accepting one row label without checking every corpus and unlabelled chunk.
    for label in ["unknown", Model::Granite107M.identity()] {
        let (_dir, conn) = store();
        seed(&conn);
        conn.execute("DELETE FROM embedding_identity", []).unwrap();
        conn.execute("UPDATE memory_embeddings SET model=?1", [label])
            .unwrap();
        assert!(vectors::identity_status(&conn).unwrap().ambiguous);
        assert!(vectors::vector_search(&conn, &[0.25; 384], 5).is_err());
        assert!(vectors::store_embedding(&conn, 1, &[0.25; 384], "AllMiniLML6V2").is_err());
    }
    let (_dir, conn) = store();
    let chunk = mdkb::store::chunks::Chunk {
        index: 0,
        content: "body".into(),
        heading_path: None,
    };
    vectors::store_chunk_embeddings(&conn, 1, &[chunk], &[vec![0.25; 384]], "AllMiniLML6V2")
        .unwrap();
    conn.execute("DELETE FROM embeddings", []).unwrap();
    assert!(vectors::identity_status(&conn).unwrap().ambiguous);
    assert!(vectors::chunk_vector_search(&conn, &[0.25; 384], 5, None).is_err());
}

#[test]
fn embedding_models_one_orphan_chunk_among_labelled_documents_fails_closed() {
    // Catches an orphan probe that maps vector rows to chunks through the wrong key
    // (the sqlite-vec shadow `chunk_id` storage slot, or the document id): chunk
    // and document ids differ here, and only document 2 loses its label.
    let (_dir, conn) = store();
    conn.execute_batch("INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(2,'notes','other.md','hash',0,0);").unwrap();
    let chunks = |n: usize| -> Vec<mdkb::store::chunks::Chunk> {
        (0..n)
            .map(|index| mdkb::store::chunks::Chunk {
                index,
                content: format!("body {index}"),
                heading_path: None,
            })
            .collect()
    };
    vectors::store_chunk_embeddings(
        &conn,
        1,
        &chunks(3),
        &vec![vec![0.25; 384]; 3],
        "AllMiniLML6V2",
    )
    .unwrap();
    vectors::store_chunk_embeddings(&conn, 2, &chunks(1), &[vec![0.5; 384]], "AllMiniLML6V2")
        .unwrap();
    vectors::store_memory_embedding(&conn, 1, &[0.5; 384], "AllMiniLML6V2").unwrap();
    assert!(!vectors::identity_status(&conn).unwrap().ambiguous);
    assert_eq!(
        vectors::chunk_vector_search(&conn, &[0.25; 384], 5, None)
            .unwrap()
            .len(),
        2
    );

    // Drop the document-level vector too, so only the chunk side can see the orphan.
    conn.execute_batch(
        "DELETE FROM embeddings WHERE document_id = 2; DELETE FROM vec_documents WHERE document_id = 2;",
    )
    .unwrap();
    assert!(vectors::identity_status(&conn).unwrap().ambiguous);
    assert!(vectors::chunk_vector_search(&conn, &[0.25; 384], 5, None).is_err());
}

#[test]
fn embedding_models_read_snapshot_probe_never_outlives_its_snapshot() {
    // Catches a snapshot-scoped identity probe that is reused after the snapshot
    // ends, or after this connection writes inside it: either way an orphaned
    // vector would be served as healthy.
    let (dir, conn) = store();
    seed(&conn);
    let other = Connection::open(dir.path().join("index.sqlite")).unwrap();
    let hits = vectors::with_read_snapshot(&conn, || {
        assert!(!vectors::identity_status(&conn)?.ambiguous);
        vectors::memory_vector_search(&conn, &[0.5; 384], 5, None)
    })
    .unwrap();
    assert_eq!(hits.len(), 1);
    other.execute("DELETE FROM memory_embeddings", []).unwrap();
    assert!(vectors::memory_vector_search(&conn, &[0.5; 384], 5, None).is_err());

    let (_dir, conn) = store();
    seed(&conn);
    vectors::with_read_snapshot(&conn, || {
        assert!(!vectors::identity_status(&conn)?.ambiguous);
        conn.execute("DELETE FROM embeddings WHERE document_id = 1", [])?;
        assert!(vectors::identity_status(&conn)?.ambiguous);
        Ok(())
    })
    .unwrap();
}

/// Session transcript documents with ids `3..3 + count`.
fn add_session_documents(conn: &Connection, count: i64) {
    conn.execute("INSERT INTO collections(name,path,pattern,created_at,updated_at) VALUES('claude_sessions','.','*.jsonl',0,0)", []).unwrap();
    for id in 3..3 + count {
        conn.execute("INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(?1,'claude_sessions',?1 || '.jsonl','hash',0,0)", [id]).unwrap();
    }
}

fn unit(axis: usize) -> Vec<f32> {
    let mut v = vec![0.0; 384];
    v[axis] = 1.0;
    v
}

#[test]
fn embedding_models_regeneration_scope_excludes_unembedded_session_transcripts() {
    // Catches a completeness check that demands vectors for session transcripts
    // the embedding scope excludes (forcing every regeneration to embed them), or
    // one that ignores transcripts when the configuration includes them.
    for (embed_sessions, completes) in [(false, true), (true, false)] {
        let (dir, conn) = store();
        std::fs::write(
            dir.path().join("config.toml"),
            format!("[search]\nauto_embed_sessions = {embed_sessions}\n"),
        )
        .unwrap();
        add_session_documents(&conn, 1);
        vectors::begin_reembed(&conn).unwrap();
        seed(&conn);
        assert_eq!(
            vectors::finish_reembed(&conn).is_ok(),
            completes,
            "auto_embed_sessions = {embed_sessions}"
        );
        assert_eq!(vectors::identity_status(&conn).unwrap().stale, !completes);
    }
}

#[test]
fn embedding_models_unscoped_chunk_search_reaches_documents_behind_session_chunks() {
    // Catches a vector leg that fills its KNN pool with session transcript chunks
    // and returns them, or nothing, while the hybrid caller drops transcripts:
    // the farther in-scope document then never gets a cosine.
    let (_dir, conn) = store();
    // More transcripts than the first document-level pool and more of their
    // chunks than the first chunk pool, so only a widened KNN reaches document 1.
    add_session_documents(&conn, 6);
    let chunks = |n: usize| -> Vec<mdkb::store::chunks::Chunk> {
        (0..n)
            .map(|index| mdkb::store::chunks::Chunk {
                index,
                content: format!("body {index}"),
                heading_path: None,
            })
            .collect()
    };
    for id in 3..9 {
        vectors::store_chunk_embeddings(&conn, id, &chunks(2), &vec![unit(0); 2], "AllMiniLML6V2")
            .unwrap();
    }
    let mut far = unit(0);
    far[1] = 1.0;
    vectors::store_chunk_embeddings(&conn, 1, &chunks(1), &[far], "AllMiniLML6V2").unwrap();
    vectors::store_memory_embedding(&conn, 1, &unit(2), "AllMiniLML6V2").unwrap();

    let unscoped = vectors::chunk_vector_search(&conn, &unit(0), 1, None).unwrap();
    assert_eq!(
        unscoped.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![1]
    );
    let sessions =
        vectors::chunk_vector_search(&conn, &unit(0), 1, Some("claude_sessions")).unwrap();
    assert!(sessions.iter().all(|(id, _)| (3..9).contains(id)));
    assert_eq!(sessions.len(), 1);
}

#[test]
fn embedding_models_unscoped_document_search_reaches_documents_behind_session_vectors() {
    // Catches `mdkb vsearch` taking a fixed document-level KNN pool and dropping
    // session transcripts after it: with more transcripts nearer the query than
    // the pool holds, the in-scope document is never returned.
    let (_dir, conn) = store();
    add_session_documents(&conn, 6);
    for id in 3..9 {
        vectors::store_embedding(&conn, id, &unit(0), "AllMiniLML6V2").unwrap();
    }
    let mut far = unit(0);
    far[1] = 1.0;
    vectors::store_embedding(&conn, 1, &far, "AllMiniLML6V2").unwrap();
    vectors::store_memory_embedding(&conn, 1, &unit(2), "AllMiniLML6V2").unwrap();

    let unscoped = vectors::vector_search(&conn, &unit(0), 1).unwrap();
    assert_eq!(
        unscoped.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        vec![1]
    );
}

#[test]
fn embedding_models_config_preserves_owner_thresholds_and_refuses_unknown_encoder() {
    // Catches silent model fallback or MiniLM automatic floors leaking into Granite defaults.
    let dir = tempfile::tempdir().unwrap();
    select(&dir, "granite107m");
    let path = dir.path().join("config.toml");
    let c = mdkb::config::Config::load(&path).unwrap();
    assert_eq!(c.hooks.recall_auto_min_cosine, 0.782);
    assert_eq!(c.hooks.recall_docs_min_cosine, 0.77);
    // Catches the MiniLM-only sigil calibration leaking into Granite defaults.
    assert_eq!(c.search.memory.min_recall_cosine, 0.70);
    std::fs::write(&path, "[embedding]\nmodel='granite107m'\n[hooks]\nrecall_auto_min_cosine=0.9\nrecall_docs_min_cosine=0.85\n").unwrap();
    let c = mdkb::config::Config::load(&path).unwrap();
    assert_eq!(c.hooks.recall_auto_min_cosine, 0.9);
    assert_eq!(c.hooks.recall_docs_min_cosine, 0.85);
    select(&dir, "typo");
    assert!(mdkb::config::Config::load(&path).is_err());
    select(&dir, "minilm");
    let c = mdkb::config::Config::load(&path).unwrap();
    assert_eq!(c.hooks.recall_auto_min_cosine, 0.5);
    assert_eq!(c.search.memory.min_recall_cosine, 0.40);
}

#[test]
fn embedding_models_granite_preserves_explicit_memory_floors_including_disabled_gate() {
    // Catches replacing an owner's explicit threshold, including the 0.0 opt-out.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    for floor in [0.0_f32, 0.40, 0.90] {
        std::fs::write(
            &path,
            format!(
                "[embedding]\nmodel='granite107m'\n[search.memory]\nmin_recall_cosine={floor:?}\n"
            ),
        )
        .unwrap();
        let config = mdkb::config::Config::load(&path).unwrap();
        assert_eq!(config.search.memory.min_recall_cosine, floor);
    }
    // An unrelated memory override must not suppress the model-specific default.
    std::fs::write(
        &path,
        "[embedding]\nmodel='granite107m'\n[search.memory]\naccess_recency_weight=0.6\n",
    )
    .unwrap();
    let config = mdkb::config::Config::load(&path).unwrap();
    assert_eq!(config.search.memory.min_recall_cosine, 0.70);
    assert_eq!(config.search.memory.access_recency_weight, 0.6);
}

#[tokio::test]
#[ignore = "requires cached Granite weights"]
async fn embedding_models_granite_sigil_rejects_labelled_negatives_without_disabling_recall() {
    // Catches an uncalibrated default admitting unrelated context, and a blanket
    // rejection or automatic-floor substitution that loses calibrated positives.
    use std::sync::{Arc, atomic::AtomicBool};
    use tokio::sync::Mutex;

    let fixture = mdkb::eval::fixture::Fixture::bundled().unwrap();
    let store = fixture.open_store().unwrap();
    std::fs::write(
        &store.ctx.config_path,
        "[embedding]\nmodel='granite107m'\n[hooks]\nrecall_docs_limit=0\n",
    )
    .unwrap();
    let config = mdkb::config::Config::load(&store.ctx.config_path).unwrap();
    mdkb::llm::service_for(Model::Granite107M, false).unwrap();
    fixture.embed(&store.ctx.conn).unwrap();
    let root = store.ctx.root().to_path_buf();
    // Keep the fixture directory alive until the later-declared handle drops.
    let ctx = store.ctx;
    let handle = mdkb::daemon::registry::RepoHandle::from_shared(
        root,
        Arc::new(Mutex::new(Some(ctx))),
        Arc::new(Mutex::new(None)),
        config,
        vec![],
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let mut false_admissions = Vec::new();
    for query in &fixture.negatives {
        let result =
            mdkb::mcp::dispatch::hook_user_prompt_submit_impl(&handle, &format!("* {query}")).await;
        if result != serde_json::json!({}) {
            false_admissions.push((query, result));
        }
    }
    assert!(
        false_admissions.is_empty(),
        "unrelated contexts: {false_admissions:?}"
    );
    let mut recovered = 0;
    for case in &fixture.recall {
        let result = mdkb::mcp::dispatch::hook_user_prompt_submit_impl(
            &handle,
            &format!("* {}", case.query),
        )
        .await;
        let text = result["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap_or_default();
        recovered += usize::from(
            case.expected_ids
                .iter()
                .any(|id| text.contains(&format!("[{id}]"))),
        );
    }
    assert!(recovered >= 14, "lost calibrated answers: {recovered}/36");
}

#[test]
fn embedding_models_code_archive_recognizes_legacy_and_rejects_equal_width_model_change() {
    // Catches treating every 384-dimensional archive as the selected encoder.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vectors.bin");
    let mut legacy = b"MDVS".to_vec();
    for field in [1_u32, 384, 1, 7] {
        legacy.extend_from_slice(&field.to_le_bytes());
    }
    for _ in 0..384 {
        legacy.extend_from_slice(&0.25_f32.to_le_bytes());
    }
    std::fs::write(&path, &legacy).unwrap();
    let mini = mdkb::code::semantic::VectorStore::open(&path).unwrap();
    assert_eq!(mini.load().unwrap(), vec![(7, vec![0.25; 384])]);
    assert_eq!(std::fs::read(&path).unwrap(), legacy);
    select(&dir, "granite107m");
    let granite =
        mdkb::code::semantic::VectorStore::open_for_model(&path, Model::Granite107M).unwrap();
    assert!(mini.load().is_err());
    assert!(mini.write_all(&[(9, vec![0.5; 384])]).is_err());
    assert!(granite.load().is_err());
    assert!(granite.write_all(&[(9, vec![0.5; 384])]).is_err());
    granite.replace_model(&[(9, vec![0.5; 384])]).unwrap();
    assert_eq!(granite.load().unwrap(), vec![(9, vec![0.5; 384])]);
    assert!(mini.load().is_err());
}

#[test]
fn embedding_models_open_never_discards_unknown_dimension_vectors() {
    // Catches an ordinary schema open destructively migrating an unrecognized legacy encoder.
    let (_dir, conn) = store();
    conn.execute_batch("DROP TABLE vec_documents; CREATE VIRTUAL TABLE vec_documents USING vec0(document_id INTEGER PRIMARY KEY, embedding FLOAT[768]);").unwrap();
    let bytes = vec![0_u8; 768 * 4];
    conn.execute("INSERT INTO embeddings(document_id,embedding,model,created_at) VALUES(1,?1,'unknown-768',0)", [&bytes]).unwrap();
    vectors::init_vector_schema(&conn).unwrap();
    assert_eq!(vectors::count_embeddings(&conn).unwrap(), 1);
    assert!(vectors::identity_status(&conn).unwrap().ambiguous);
    assert!(vectors::vector_search(&conn, &[0.25; 384], 5).is_err());
    vectors::begin_reembed(&conn).unwrap();
    vectors::store_embedding(&conn, 1, &[0.25; 384], Model::MiniLm.identity()).unwrap();
}

#[test]
fn embedding_models_hybrid_memory_keeps_lexical_matches_during_model_change() {
    // Catches a warm new encoder making default memory search fail instead of using FTS.
    let (dir, conn) = store();
    seed(&conn);
    select(&dir, "granite107m");
    conn.execute("UPDATE memory_entries SET title='SQLite checkpoint recovery', content='Recover SQLite checkpoint journals' WHERE id='memory'", []).unwrap();
    let query = mdkb::llm::embeddings::Vector {
        model: Model::Granite107M,
        values: vec![0.5; 384],
    };
    let results = mdkb::store::memory::search_entries_recall_labelled(
        &conn,
        "SQLite checkpoint recovery",
        Some(&query),
        5,
        None,
        &mdkb::config::SearchMemoryConfig::default(),
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].entry.id, "memory");
    assert!(
        vectors::memory_vector_search_for_model(&conn, &query, 5, None, Model::Granite107M)
            .is_err()
    );
}

#[test]
fn embedding_models_hybrid_documents_keep_lexical_matches_during_model_change() {
    // Catches default document query refusing the whole request when only its vectors are stale.
    let dir = tempfile::tempdir().unwrap();
    let ctx = mdkb::core::Context::init(dir.path()).unwrap();
    ctx.conn.execute_batch("INSERT INTO collections(name,path,pattern,created_at,updated_at) VALUES('notes','.','*.md',0,0);
        INSERT INTO content(hash,body,created_at) VALUES('hash','sqlite recovery instructions',0);
        INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(1,'notes','recovery.md','hash',0,0);").unwrap();
    vectors::store_embedding(&ctx.conn, 1, &[0.25; 384], Model::MiniLm.identity()).unwrap();
    std::fs::write(&ctx.config_path, "[embedding]\nmodel='granite107m'\n").unwrap();
    let query = mdkb::llm::embeddings::Vector {
        model: Model::Granite107M,
        values: vec![0.25; 384],
    };
    let results = mdkb::core::search::hybrid_search_fts_labelled(
        &ctx,
        "sqlite",
        Some(&query),
        5,
        None,
        false,
    )
    .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, 1);
    assert!(
        vectors::vector_search_for_model(&ctx.conn, &query, 5, None, Model::Granite107M).is_err()
    );
}

#[test]
fn embedding_models_routed_command_uses_existing_mutation_lock() {
    // Catches daemon Embed acquiring its already-held advisory lock a second time.
    let dir = tempfile::tempdir().unwrap();
    let ctx = mdkb::core::Context::init(dir.path()).unwrap();
    let lock = mdkb::store::mutation_lock::acquire(&ctx.db_path, "routed command test").unwrap();
    // Fail before any model initialization, but after the executor would reacquire the lock.
    std::fs::write(&ctx.config_path, "[embedding]\nmodel='unknown-encoder'\n").unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = mdkb::core::cli_mutation::execute_context_mutation(
            &ctx,
            mdkb::core::cli_mutation::CliMutation::Embed {
                collection: None,
                reembed: true,
            },
        );
        tx.send(result.map_err(|e| e.to_string())).unwrap();
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(3));
    drop(lock);
    worker.join().unwrap();
    let error = result
        .expect("routed embedding must not wait on its own mutation lock")
        .unwrap_err();
    assert!(error.contains("embedding"), "{error}");
}

#[test]
fn embedding_models_new_granite_memory_write_succeeds_after_commit() {
    // Catches the near-duplicate warning searching a Granite store with the MiniLM
    // identity: the row and vector commit, then the caller receives an error.
    let dir = tempfile::tempdir().unwrap();
    let ctx = mdkb::core::Context::init(dir.path()).unwrap();
    std::fs::write(&ctx.config_path, "[embedding]\nmodel='granite107m'\n").unwrap();
    let vector = mdkb::llm::embeddings::Vector {
        model: Model::Granite107M,
        values: vec![0.5; 384],
    };
    let result = mdkb::core::memory::write_memory(
        &ctx.conn,
        mdkb::core::memory::WriteMemoryInput {
            id: "granite-entry",
            title: "Granite entry",
            content: "Stored with the multilingual encoder",
            entry_type: "topic",
            source_type: None,
            tags: &[],
            triggers: None,
            ttl: None,
            due_in: None,
            embedding: Some(&vector),
            embed_when_missing: false,
            source_path: None,
            relates: &[],
            session: None,
            agent: None,
            on_conflict: None,
            dry_run: false,
        },
    );
    assert!(
        result.is_ok(),
        "committed Granite write reported failure: {result:?}"
    );
    let label: String = ctx
        .conn
        .query_row("SELECT model FROM memory_embeddings", [], |r| r.get(0))
        .unwrap();
    assert_eq!(label, Model::Granite107M.identity());
}

#[test]
fn embedding_models_unrelated_config_error_keeps_store_open_and_lexical() {
    // Catches identity resolution using the strict config loader: a type error in an
    // unrelated key, which the store tolerates with defaults, must not make every open
    // and every lexical search fail.
    let dir = tempfile::tempdir().unwrap();
    {
        let ctx = mdkb::core::Context::init(dir.path()).unwrap();
        ctx.conn.execute_batch("INSERT INTO collections(name,path,pattern,created_at,updated_at) VALUES('notes','.','*.md',0,0);
            INSERT INTO content(hash,body,created_at) VALUES('hash','sqlite recovery instructions',0);
            INSERT INTO documents(id,collection,relative_path,hash,file_modified_at,indexed_at) VALUES(1,'notes','recovery.md','hash',0,0);").unwrap();
        std::fs::write(&ctx.config_path, "[search]\nauto_embed_memory = 'yes'\n").unwrap();
    }
    let ctx = mdkb::core::Context::open(dir.path())
        .expect("a tolerated config error must not make the store unopenable");
    let results =
        mdkb::core::search::hybrid_search_fts_labelled(&ctx, "sqlite", None, 5, None, false)
            .expect("lexical search must survive a tolerated config error");
    assert_eq!(results.len(), 1);
}

fn write_input<'a>(
    id: &'a str,
    embedding: &'a mdkb::llm::embeddings::Vector,
) -> mdkb::core::memory::WriteMemoryInput<'a> {
    mdkb::core::memory::WriteMemoryInput {
        id,
        title: id,
        content: "Shared near-duplicate content",
        entry_type: "topic",
        source_type: None,
        tags: &[],
        triggers: None,
        ttl: None,
        due_in: None,
        embedding: Some(embedding),
        embed_when_missing: false,
        source_path: None,
        relates: &[],
        session: None,
        agent: None,
        on_conflict: None,
        dry_run: false,
    }
}

#[test]
fn embedding_models_stale_identity_memory_write_persists_without_warning_error() {
    // Catches a stale-store write returning Err after the row committed: the similar-entry
    // warning is best effort, so the write must succeed and keep the row for lexical recall.
    let dir = tempfile::tempdir().unwrap();
    let ctx = mdkb::core::Context::init(dir.path()).unwrap();
    let minilm = mdkb::llm::embeddings::Vector::minilm(&[0.5; 384]);
    mdkb::core::memory::write_memory(&ctx.conn, write_input("old", &minilm)).unwrap();
    std::fs::write(&ctx.config_path, "[embedding]\nmodel='granite107m'\n").unwrap();
    let granite = mdkb::llm::embeddings::Vector {
        model: Model::Granite107M,
        values: vec![0.5; 384],
    };
    let output = mdkb::core::memory::write_memory(&ctx.conn, write_input("fresh", &granite))
        .expect("stale identity must not fail a committed write");
    assert!(!output.contains("Similar entry exists"), "{output}");
    let rows: i64 = ctx
        .conn
        .query_row(
            "SELECT COUNT(*) FROM memory_entries WHERE id='fresh'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
}

#[test]
fn embedding_models_minilm_memory_write_keeps_similar_entry_warning() {
    // Catches the model guard suppressing the near-duplicate warning on a healthy MiniLM store.
    let dir = tempfile::tempdir().unwrap();
    let ctx = mdkb::core::Context::init(dir.path()).unwrap();
    // Cosine 0.90 is similar, but not a duplicate: identical vectors would
    // correctly refuse the second entry before the warning path is reached.
    let mut first = vec![0.0; 384];
    first[0] = 1.0;
    let mut second = vec![0.0; 384];
    second[0] = 0.9;
    second[1] = 0.4358899;
    let first = mdkb::llm::embeddings::Vector::minilm(&first);
    let second = mdkb::llm::embeddings::Vector::minilm(&second);
    mdkb::core::memory::write_memory(&ctx.conn, write_input("first", &first)).unwrap();
    let output =
        mdkb::core::memory::write_memory(&ctx.conn, write_input("second", &second)).unwrap();
    assert!(output.contains("Similar entry exists: first"), "{output}");
}

#[test]
fn embedding_models_orphan_code_archive_refuses_before_resetting_sqlite_vectors() {
    // Catches a known code-index precondition deleting a healthy, stale or pending
    // SQLite generation before the command reports that code cannot be regenerated.
    for scenario in ["current", "stale", "pending"] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mdkb::core::Context::init(dir.path()).unwrap();
        let vector = mdkb::llm::embeddings::Vector::minilm(&[0.5; 384]);
        mdkb::core::memory::write_memory(&ctx.conn, write_input("retained", &vector)).unwrap();
        if scenario == "stale" {
            std::fs::write(&ctx.config_path, "[embedding]\nmodel='granite107m'\n").unwrap();
        } else if scenario == "pending" {
            vectors::begin_reembed(&ctx.conn).unwrap();
            let rowid = mdkb::store::memory::get_rowid(&ctx.conn, "retained")
                .unwrap()
                .unwrap();
            vectors::store_memory_embedding(&ctx.conn, rowid, &vector, Model::MiniLm.identity())
                .unwrap();
        }
        let before = vectors::identity_status(&ctx.conn).unwrap();
        let archive = ctx.db_path.parent().unwrap().join("vectors.bin");
        std::fs::write(&archive, b"orphan code archive").unwrap();
        let error = mdkb::core::ops::handle_embed_explicit(&ctx, None, true).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("code vectors have no source code index"),
            "{scenario}: {error}"
        );
        let count: i64 = ctx
            .conn
            .query_row("SELECT COUNT(*) FROM memory_embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            count, 1,
            "{scenario}: source vectors must survive a failed precondition"
        );
        let after = vectors::identity_status(&ctx.conn).unwrap();
        assert_eq!(after.current_model, before.current_model, "{scenario}");
        assert_eq!(after.pending_model, before.pending_model, "{scenario}");
        assert_eq!(
            std::fs::read(&archive).unwrap(),
            b"orphan code archive",
            "{scenario}"
        );
    }
}

#[test]
fn embedding_models_reembed_refuses_invalid_config_before_changing_generation() {
    // Catches whole-file fallback silently replacing a Granite generation with
    // MiniLM during destructive re-embedding. Serving must remain available.
    for invalid in [
        "[embedding]\nmodel=granite107m\n",
        "[embedding]\nmodel='granite107m'\n[search]\nauto_embed_memory='yes'\n",
        "[embedding]\nmodel='unknown'\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let ctx = mdkb::core::Context::init(dir.path()).unwrap();
        std::fs::write(&ctx.config_path, "[embedding]\nmodel='granite107m'\n").unwrap();
        vectors::begin_reembed(&ctx.conn).unwrap();
        let vector = mdkb::llm::embeddings::Vector {
            model: Model::Granite107M,
            values: vec![0.5; 384],
        };
        mdkb::core::memory::write_memory(&ctx.conn, write_input("retained", &vector)).unwrap();
        vectors::finish_reembed(&ctx.conn).unwrap();
        std::fs::write(&ctx.config_path, invalid).unwrap();
        let error = mdkb::core::ops::handle_embed_explicit(&ctx, None, true).unwrap_err();
        assert!(
            error.to_string().contains("config") || error.to_string().contains("TOML"),
            "{error}"
        );
        let (current, pending): (String, Option<String>) = ctx
            .conn
            .query_row(
                "SELECT current_model, pending_model FROM embedding_identity",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(current, Model::Granite107M.identity());
        assert_eq!(pending, None);
        let count: i64 = ctx
            .conn
            .query_row("SELECT COUNT(*) FROM memory_embeddings", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            count, 1,
            "invalid configuration must not delete existing vectors"
        );
    }
}
