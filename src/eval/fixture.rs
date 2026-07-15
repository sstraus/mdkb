//! Evaluation fixtures: a self-contained corpus of memories plus recall and
//! judge cases, loaded from JSON. The harness seeds a fresh in-memory DB from
//! the fixture (reproducible — independent of the live repo), so a recall
//! baseline is stable across machines and runs.

use crate::error::Result;
use chrono::Utc;
use rusqlite::Connection;
use serde::Deserialize;
use std::path::Path;

use super::judge::JudgeCase;
use super::recall::EvalCase;
use crate::store::memory::{EntryStatus, EntryType, MemoryEntry, SourceType, add_entry};

/// The synthetic corpus shipped in the binary, used when no `--fixture` is given.
const DEFAULT_FIXTURE: &str = include_str!("../../assets/eval/memory-recall.json");

/// A memory to seed into the evaluation DB.
#[derive(Debug, Deserialize)]
pub struct FixtureMemory {
    pub id: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// A recall case: query + the id(s) a correct top-k must surface.
#[derive(Debug, Deserialize)]
pub struct FixtureRecall {
    pub query: String,
    pub expected_ids: Vec<String>,
}

/// A judge case: question + the answer the retrieved context must support.
#[derive(Debug, Deserialize)]
pub struct FixtureJudge {
    pub question: String,
    pub expected_answer: String,
}

/// A complete evaluation fixture.
#[derive(Debug, Deserialize)]
pub struct Fixture {
    #[serde(default)]
    pub memories: Vec<FixtureMemory>,
    #[serde(default)]
    pub recall: Vec<FixtureRecall>,
    #[serde(default)]
    pub judge: Vec<FixtureJudge>,
}

impl Fixture {
    /// Parse a fixture from a JSON file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&raw)?)
    }

    /// The synthetic fixture bundled in the binary (machine-independent).
    pub fn bundled() -> Result<Self> {
        Ok(serde_json::from_str(DEFAULT_FIXTURE)?)
    }

    /// Build a fresh in-memory DB seeded with this fixture's memories.
    pub fn seed_db(&self) -> Result<Connection> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        crate::store::schema::init_schema(&conn)?;
        let now = Utc::now().timestamp();
        for m in &self.memories {
            add_entry(
                &conn,
                &MemoryEntry {
                    id: m.id.clone(),
                    title: m.title.clone(),
                    content: m.content.clone(),
                    entry_type: EntryType::Topic,
                    tags: m.tags.clone(),
                    status: EntryStatus::Active,
                    created_at: now,
                    updated_at: now,
                    superseded_by: None,
                    access_count: 0,
                    last_accessed: None,
                    source_path: None,
                    confirmations: 0,
                    last_confirmed_at: None,
                    source_type: SourceType::UserStatement,
                    expires_at: None,
                    due_at: None,
                },
            )?;
        }
        Ok(conn)
    }

    pub fn recall_cases(&self) -> Vec<EvalCase> {
        self.recall
            .iter()
            .map(|r| EvalCase {
                query: r.query.clone(),
                expected_ids: r.expected_ids.clone(),
                embedding: None,
            })
            .collect()
    }

    pub fn judge_cases(&self) -> Vec<JudgeCase> {
        self.judge
            .iter()
            .map(|j| JudgeCase {
                question: j.question.clone(),
                expected_answer: j.expected_answer.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eval::recall::run_recall;

    fn committed_fixture() -> Fixture {
        Fixture::bundled().expect("bundled eval fixture parses")
    }

    #[test]
    fn committed_fixture_is_well_formed() {
        let fx = committed_fixture();
        assert!(!fx.memories.is_empty(), "fixture must seed memories");
        assert!(!fx.recall.is_empty(), "fixture must define recall cases");
        // Every expected id must reference a memory that actually exists.
        let ids: std::collections::HashSet<&str> =
            fx.memories.iter().map(|m| m.id.as_str()).collect();
        for c in &fx.recall {
            for e in &c.expected_ids {
                assert!(ids.contains(e.as_str()), "recall expects unknown id {e}");
            }
        }
    }

    #[test]
    fn committed_fixture_yields_a_strong_recall_baseline() {
        let fx = committed_fixture();
        let conn = fx.seed_db().unwrap();
        let report = run_recall(&conn, &fx.recall_cases(), 5).unwrap();
        assert_eq!(report.n, fx.recall.len());
        // BM25 over this hand-designed corpus should recover most expected ids.
        // A drop below this floor means the fixture or retrieval regressed.
        assert!(
            report.recall_at_k >= 0.8,
            "recall@5 baseline dropped to {}",
            report.recall_at_k
        );
    }
}
