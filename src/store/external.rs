//! Owner-pushed documents. Revisions and tombstones share the document transaction.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::domain::Document;
use crate::error::{Error, Result};
use crate::store::documents;

pub(crate) const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS external_records (
    source TEXT NOT NULL,
    external_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision >= 0),
    document_id INTEGER UNIQUE REFERENCES documents(id),
    PRIMARY KEY(source, external_id)
);
"#;

const PREFIX: &str = "external:";

/// Reserved collection names cannot be created through the filesystem CLI.
pub fn source(collection: &str) -> Option<&str> {
    collection.strip_prefix(PREFIX)
}

pub fn label(collection: &str) -> String {
    source(collection)
        .map(|source| format!(" [read-only, source: {source}]"))
        .unwrap_or_default()
}

pub(crate) fn refuse_collection(collection: &str) -> Result<()> {
    if source(collection).is_some() {
        return Err(Error::other(
            "External documents are read-only; use external ingest",
        ));
    }
    Ok(())
}

pub(crate) fn refuse_document(conn: &Connection, id: i64) -> Result<()> {
    let collection: Option<String> = conn
        .query_row(
            "SELECT collection FROM documents WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(collection) = collection {
        refuse_collection(&collection)?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub(crate) struct Owner {
    pub source: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Delete {
    pub source: String,
    pub id: String,
    pub revision: i64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Upsert {
    pub source: String,
    pub id: String,
    pub revision: i64,
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

#[derive(Debug)]
pub(crate) enum Request {
    Upsert(Upsert),
    Delete(Delete),
    List(Owner),
}

impl Request {
    /// Validate completely before opening a store or taking a write lock.
    pub(crate) fn parse(method: &str, params: Value) -> std::result::Result<Self, String> {
        let request = match method {
            "external.upsert" => {
                Self::Upsert(serde_json::from_value(params).map_err(|e| e.to_string())?)
            }
            "external.delete" => {
                Self::Delete(serde_json::from_value(params).map_err(|e| e.to_string())?)
            }
            "external.list" => {
                Self::List(serde_json::from_value(params).map_err(|e| e.to_string())?)
            }
            _ => return Err("Unknown external method".into()),
        };
        let (owner, record) = match &request {
            Self::Upsert(p) => (&p.source, Some((&p.id, p.revision))),
            Self::Delete(p) => (&p.source, Some((&p.id, p.revision))),
            Self::List(p) => (&p.source, None),
        };
        if owner.trim().is_empty() || owner.len() > 256 {
            return Err("source must contain 1-256 UTF-8 bytes".into());
        }
        if let Some((id, revision)) = record {
            if id.trim().is_empty() || id.len() > 1024 {
                return Err("id must contain 1-1024 UTF-8 bytes".into());
            }
            if revision < 0 {
                return Err("revision must be a non-negative signed 64-bit integer".into());
            }
        }
        Ok(request)
    }

    pub(crate) fn is_write(&self) -> bool {
        !matches!(self, Self::List(_))
    }

    pub(crate) fn execute(&self, conn: &Connection) -> Result<Value> {
        match self {
            Self::List(p) => {
                let mut stmt = conn.prepare(
                    "SELECT external_id, revision, document_id FROM external_records WHERE source = ?1 ORDER BY external_id",
                )?;
                let rows = stmt
                    .query_map([&p.source], read_record)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(json!({"documents": rows}))
            }
            Self::Upsert(p) => mutate(conn, &p.source, &p.id, p.revision, Some(p)),
            Self::Delete(p) => mutate(conn, &p.source, &p.id, p.revision, None),
        }
    }
}

#[derive(Debug, Serialize)]
struct Record {
    id: String,
    revision: i64,
    document_id: Option<i64>,
    deleted: bool,
}

fn read_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    let document_id: Option<i64> = row.get(2)?;
    Ok(Record {
        id: row.get(0)?,
        revision: row.get(1)?,
        deleted: document_id.is_none(),
        document_id,
    })
}

fn mutate(
    conn: &Connection,
    source: &str,
    id: &str,
    revision: i64,
    upsert: Option<&Upsert>,
) -> Result<Value> {
    // IMMEDIATE serializes read-check-write across connections, not just daemon tasks.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        let previous = conn.query_row(
            "SELECT external_id, revision, document_id FROM external_records WHERE source = ?1 AND external_id = ?2",
            params![source, id], read_record,
        ).optional()?;
        if let Some(previous) = &previous
            && previous.revision >= revision
        {
            let mut value = serde_json::to_value(previous)?;
            value["applied"] = json!(false);
            return Ok(value);
        }
        let document_id = if let Some(p) = upsert {
            let now = chrono::Utc::now().timestamp();
            let collection = format!("{PREFIX}{source}");
            conn.execute(
                "INSERT OR IGNORE INTO collections(name, path, pattern, source, created_at, updated_at) VALUES (?1, '', '', 'external', ?2, ?2)",
                params![collection, now],
            )?;
            let mut metadata = p.metadata.clone();
            metadata.insert(
                "external".into(),
                json!({"source":source, "id":id, "revision":revision, "read_only":true}),
            );
            let doc = Document {
                id: 0,
                collection,
                relative_path: id.into(),
                hash: String::new(),
                title: Some(p.title.clone()),
                metadata: Some(Value::Object(metadata)),
                file_modified_at: now,
                indexed_at: now,
                status: None,
            };
            Some(documents::index_external_in_tx(conn, &doc, &p.body)?)
        } else {
            // Unlink first: the FK prevents any accidental non-ingest document deletion.
            conn.execute("UPDATE external_records SET document_id = NULL WHERE source = ?1 AND external_id = ?2", params![source, id])?;
            if let Some(doc_id) = previous.and_then(|p| p.document_id) {
                documents::delete_external_in_tx(conn, doc_id)?;
            }
            None
        };
        conn.execute(
            "INSERT INTO external_records(source, external_id, revision, document_id) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(source, external_id) DO UPDATE SET revision=excluded.revision, document_id=excluded.document_id",
            params![source, id, revision, document_id],
        )?;
        let mut value = serde_json::to_value(Record {
            id: id.into(),
            revision,
            document_id,
            deleted: document_id.is_none(),
        })?;
        value["applied"] = json!(true);
        Ok(value)
    })();
    match result {
        Ok(value) => {
            if let Err(error) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error.into());
            }
            Ok(value)
        }
        Err(error) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> crate::store::Store {
        let store = crate::store::Store::open_in_memory().unwrap();
        crate::store::schema::init_schema(store.conn()).unwrap();
        store
    }

    fn call(conn: &Connection, method: &str, params: Value) -> Value {
        Request::parse(method, params)
            .unwrap()
            .execute(conn)
            .unwrap()
    }

    fn upsert(revision: i64, body: &str) -> Value {
        json!({"source":"owner", "id":"opaque/../id", "revision":revision,
            "title":"Nebula", "body":body, "metadata":{"external":{"read_only":false}}})
    }

    /// Catches out-of-order replay overwriting content or reviving deleted content.
    #[test]
    fn external_ingest_revision_replay_and_tombstone() {
        let store = store();
        let conn = store.conn();
        let first = call(conn, "external.upsert", upsert(5, "newest"));
        let mut other = upsert(99, "another owner");
        other["source"] = json!("another");
        let other = call(conn, "external.upsert", other);
        assert_ne!(other["document_id"], first["document_id"]);
        for (revision, body) in [(4, "older"), (5, "conflicting retry")] {
            let reply = call(conn, "external.upsert", upsert(revision, body));
            assert_eq!(reply["applied"], false);
            assert_eq!(reply["revision"], 5);
            assert_eq!(reply["document_id"], first["document_id"]);
        }
        let doc = documents::get_document(conn, first["document_id"].as_i64().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            documents::get_content(conn, &doc.hash).unwrap().as_deref(),
            Some("newest")
        );
        assert_eq!(doc.metadata.unwrap()["external"]["read_only"], true);
        let stale_delete = call(
            conn,
            "external.delete",
            json!({"source":"owner", "id":"opaque/../id", "revision":4}),
        );
        assert_eq!(stale_delete["deleted"], false);
        let deleted = call(
            conn,
            "external.delete",
            json!({"source":"owner", "id":"opaque/../id", "revision":6}),
        );
        assert_eq!(deleted["applied"], true);
        for revision in [5, 6] {
            let reply = call(conn, "external.upsert", upsert(revision, "resurrected"));
            assert_eq!(reply["applied"], false);
            assert_eq!(reply["deleted"], true);
        }
        assert_eq!(
            call(conn, "external.list", json!({"source":"owner"})),
            json!({"documents":[
                {"id":"opaque/../id", "revision":6, "document_id":null, "deleted":true}
            ]})
        );
        let recreated = call(conn, "external.upsert", upsert(7, "intentional recreation"));
        assert_eq!(recreated["applied"], true);
        assert_eq!(recreated["deleted"], false);
        let unknown = call(
            conn,
            "external.delete",
            json!({"source":"owner", "id":"never-present", "revision":0}),
        );
        assert_eq!(unknown["deleted"], true);
        assert_eq!(
            call(conn, "external.list", json!({"source":"another"}))["documents"][0]["revision"],
            99
        );
        assert_eq!(
            call(conn, "external.list", json!({"source":"unseen"})),
            json!({"documents":[]})
        );
    }

    /// Catches partial content writes that advance revisions after a database error.
    #[test]
    fn external_ingest_failed_write_rolls_back_revision_and_content() {
        let store = store();
        let conn = store.conn();
        call(conn, "external.upsert", upsert(1, "original"));
        conn.execute_batch("CREATE TRIGGER reject_external BEFORE UPDATE ON external_records BEGIN SELECT RAISE(ABORT, 'reject revision'); END;").unwrap();
        let result = Request::parse("external.upsert", upsert(2, "uncommitted"))
            .unwrap()
            .execute(conn);
        assert!(result.is_err());
        assert!(conn.is_autocommit());
        let current = call(conn, "external.list", json!({"source":"owner"}));
        assert_eq!(current["documents"][0]["revision"], 1);
        let doc = documents::get_document(
            conn,
            current["documents"][0]["document_id"].as_i64().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            documents::get_content(conn, &doc.hash).unwrap().as_deref(),
            Some("original")
        );
    }

    /// Catches alternate collection/document mutation APIs bypassing owner revisions.
    #[test]
    fn external_ingest_refuses_non_owner_mutations() {
        let store = store();
        let conn = store.conn();
        let reply = call(conn, "external.upsert", upsert(1, "immutable"));
        let id = reply["document_id"].as_i64().unwrap();
        let doc = documents::get_document(conn, id).unwrap().unwrap();
        assert!(documents::index_document(conn, &doc, "changed").is_err());
        assert!(documents::index_document_in_tx(conn, &doc, "changed").is_err());
        assert!(documents::delete_document(conn, id).is_err());
        assert!(crate::store::collections::remove_collection(conn, &doc.collection).is_err());
        assert!(
            crate::store::collections::rename_collection(conn, &doc.collection, "files").is_err()
        );
        let mut collection = crate::store::collections::get_collection(conn, &doc.collection)
            .unwrap()
            .unwrap();
        collection.source = "manual".into();
        assert!(crate::store::collections::add_collection(conn, &collection).is_err());
        assert!(
            crate::store::evolution::update_document_status(
                conn,
                id,
                crate::store::evolution::DocumentStatus::Retracted,
                None
            )
            .is_err()
        );
        assert_eq!(
            documents::get_content(conn, &doc.hash).unwrap().as_deref(),
            Some("immutable")
        );
    }

    /// Catches watcher deletion notifications treating owner IDs as vanished files.
    #[test]
    fn external_ingest_survives_watcher_file_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = crate::core::Context::init(temp.path()).unwrap();
        std::fs::write(
            temp.path().join(".mdkb/config.toml"),
            "[search]\nauto_embed_docs = false\n",
        )
        .unwrap();
        let mut record = upsert(1, "watcher retained");
        record["id"] = json!("vanished.md");
        let inserted = call(&ctx.conn, "external.upsert", record);
        let result = crate::core::indexing::handle_watcher_files_unverified(
            &ctx,
            temp.path(),
            &[temp
                .path()
                .join("vanished.md")
                .to_string_lossy()
                .into_owned()],
        )
        .unwrap();
        assert_eq!(result.removed, 0);
        let doc = documents::get_document(&ctx.conn, inserted["document_id"].as_i64().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(
            documents::get_content(&ctx.conn, &doc.hash)
                .unwrap()
                .as_deref(),
            Some("watcher retained")
        );
    }

    /// Catches independent daemon/CLI connections racing the revision check.
    #[test]
    fn external_ingest_concurrent_revisions_keep_newest() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        let store = crate::store::Store::open(&path).unwrap();
        crate::store::schema::init_schema(store.conn()).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = [(1, "older"), (2, "newest")]
            .into_iter()
            .map(|(revision, body)| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let writer = crate::store::Store::open(&path).unwrap();
                    barrier.wait();
                    call(writer.conn(), "external.upsert", upsert(revision, body))
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let current = call(store.conn(), "external.list", json!({"source":"owner"}));
        assert_eq!(current["documents"][0]["revision"], 2);
        let doc = documents::get_document(
            store.conn(),
            current["documents"][0]["document_id"].as_i64().unwrap(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            documents::get_content(store.conn(), &doc.hash)
                .unwrap()
                .as_deref(),
            Some("newest")
        );
    }

    /// Catches v34 stores opening without the owner revision table.
    #[test]
    fn external_ingest_migrates_v34_store() {
        let store = store();
        let conn = store.conn();
        conn.execute_batch("DROP TABLE external_records; UPDATE schema_version SET version = 34;")
            .unwrap();
        crate::store::schema::init_schema(conn).unwrap();
        let reply = call(conn, "external.upsert", upsert(0, "migrated"));
        assert_eq!(reply["applied"], true);
        assert_eq!(
            call(conn, "external.list", json!({"source":"owner"}))["documents"][0]["revision"],
            0
        );
    }

    /// Catches malformed revisions and payloads admitted as owner writes.
    #[test]
    fn external_ingest_rejects_invalid_parameters() {
        let mut invalid = Vec::new();
        for revision in [
            json!(-1),
            json!(1.5),
            json!("1"),
            json!(9_223_372_036_854_775_808_u64),
            Value::Null,
        ] {
            let mut params = upsert(0, "body");
            params["revision"] = revision;
            invalid.push(params);
        }
        for (key, value) in [
            ("source", json!("")),
            ("id", json!(" ")),
            ("metadata", json!([])),
            ("body", json!(false)),
        ] {
            let mut params = upsert(0, "body");
            params[key] = value;
            invalid.push(params);
        }
        for params in invalid {
            assert!(
                Request::parse("external.upsert", params.clone()).is_err(),
                "{params}"
            );
        }
        assert!(Request::parse("external.upsert", upsert(i64::MAX, "body")).is_ok());
    }
}
