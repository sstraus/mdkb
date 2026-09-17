//! Offline evaluation harness for memory retrieval quality.
//!
//! `recall` computes recall@k / MRR and `judge` scores whether the retrieved
//! context answers a question. Both run every query through the production
//! memory search on a real store, in one [`recall::Mode`] per run: `bm25`
//! needs no model and is deterministic; `embedding` and `hybrid` need the ONNX
//! model and are skipped, with the reason in the output, when it is not
//! cached. [`run_modes`] is the entry the CLI uses.

pub mod embedding_gap;
pub mod fixture;
pub mod judge;
pub mod recall;

use std::sync::Arc;

use crate::config::Config;
use crate::error::Result;
use crate::llm::EmbeddingService;
use fixture::Fixture;
use recall::{Mode, Retrieval};
use rusqlite::Connection;
use serde::Serialize;

/// The outcome of one mode: a report, or the reason it did not run.
#[derive(Debug, Clone, Serialize)]
pub struct ModeRun<T> {
    pub mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// Load the embedding model, or say why the model modes must be skipped.
///
/// Without `download`, a model that is not on disk is a skip, not a fetch: a
/// CI run with a cold cache must still pass, and a developer must not be
/// surprised by a 90 MB download from an eval command.
pub fn load_embedder(download: bool) -> std::result::Result<Arc<EmbeddingService>, String> {
    if !download && !crate::llm::embeddings::model_is_cached() {
        return Err(format!(
            "ONNX model not cached at {} (pass --download to fetch it)",
            crate::llm::embeddings::model_cache_path().display()
        ));
    }
    if download {
        crate::llm::get_or_download_service()
    } else {
        crate::llm::get_cached_service()
    }
    .map_err(|e| format!("embedding model unavailable: {e}"))
}

/// Seed a real store from `fixture` and run `eval` once per requested mode.
///
/// `embedder` is the result of [`load_embedder`]; the modes that need a model
/// are skipped with its `Err` reason. The store is embedded through the
/// production backfill only when a model mode will actually run.
pub fn run_modes<T>(
    fixture: &Fixture,
    modes: &[Mode],
    embedder: std::result::Result<Arc<EmbeddingService>, String>,
    eval: impl Fn(&Connection, &Retrieval<'_>) -> Result<T>,
) -> Result<Vec<ModeRun<T>>> {
    let store = fixture.open_store()?;
    let memory_cfg = Config::default().search.memory;
    let (embedder, skip_reason) = match embedder {
        Ok(svc) => (Some(svc), None),
        Err(reason) => (None, Some(reason)),
    };
    let embedder = if modes.iter().any(|m| m.needs_model()) {
        if embedder.is_some() {
            fixture.embed(&store.ctx.conn)?;
        }
        embedder
    } else {
        None
    };
    let mut runs = Vec::with_capacity(modes.len());
    for &mode in modes {
        if mode.needs_model() && embedder.is_none() {
            runs.push(ModeRun {
                mode,
                report: None,
                skipped: skip_reason.clone(),
            });
            continue;
        }
        let retrieval = Retrieval {
            mode,
            embedder: embedder.as_deref(),
            memory_cfg: &memory_cfg,
        };
        runs.push(ModeRun {
            mode,
            report: Some(eval(&store.ctx.conn, &retrieval)?),
            skipped: None,
        });
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_modes_are_skipped_with_the_reason_and_bm25_still_runs() {
        let fx = Fixture::bundled().unwrap();
        let runs = run_modes(
            &fx,
            &Mode::ALL,
            Err("test: no model".to_string()),
            |conn, retrieval| {
                recall::run_recall(
                    conn,
                    retrieval,
                    &fx.recall_cases(),
                    &fx.negative_queries(),
                    5,
                )
            },
        )
        .unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].mode, Mode::Bm25);
        assert!(runs[0].report.is_some() && runs[0].skipped.is_none());
        for run in &runs[1..] {
            assert!(run.report.is_none(), "{:?} must not run", run.mode);
            assert_eq!(run.skipped.as_deref(), Some("test: no model"));
        }
        // JSON is what CI and the docs read: a skipped mode carries its reason
        // and no report fields.
        let json = serde_json::to_value(&runs).unwrap();
        assert_eq!(json[1]["skipped"], "test: no model");
        assert!(json[1].get("report").is_none());
        assert_eq!(json[0]["report"]["mode"], "bm25");
    }

    #[test]
    fn a_bm25_only_selection_ignores_the_embedder_entirely() {
        // The closure must see exactly one run, in BM25 mode, with no embedder.
        let fx = Fixture::bundled().unwrap();
        let runs = run_modes(&fx, &[Mode::Bm25], Err("unused".to_string()), |_, r| {
            assert_eq!(r.mode, Mode::Bm25);
            assert!(r.embedder.is_none());
            Ok(())
        })
        .unwrap();
        assert_eq!(runs.len(), 1);
        assert!(runs[0].skipped.is_none());
    }
}

#[cfg(test)]
pub(crate) mod testkit {
    use crate::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};
    use crate::store::schema::init_schema;
    use chrono::Utc;
    use rusqlite::Connection;

    /// In-memory DB with the full schema applied — the shared fixture for the
    /// metric arithmetic tests (BM25 mode only; it has no vector tables).
    pub fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    /// Insert a minimal active `Topic` entry.
    pub fn add(conn: &Connection, id: &str, title: &str, content: &str, tags: &[&str]) {
        let now = Utc::now().timestamp();
        let entry = MemoryEntry {
            id: id.to_string(),
            title: title.to_string(),
            content: content.to_string(),
            entry_type: EntryType::Topic,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        };
        add_entry(conn, &entry).unwrap();
    }
}
