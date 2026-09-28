//! Read-only CLI inventory for assisted prior migration.

use rusqlite::Connection;
use serde::Serialize;

use crate::cli::OutputFormat;
use crate::error::Result;
use crate::store::memory::EntryType;
use crate::store::priors::TriggerMatcher;

#[derive(Debug, Serialize)]
pub struct PriorProposal {
    id: String,
    title: String,
    content: String,
    status: String,
    proposed_type: EntryType,
    proposed_triggers: Vec<TriggerMatcher>,
}

/// Include archived priors so a review does not silently lose retained source
/// entries. A cluster-promoted prior already has its own lifecycle and is not
/// part of the manual migration census.
pub fn prior_migration_proposals(conn: &Connection) -> Result<Vec<PriorProposal>> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.title, e.content, COALESCE(e.status, 'unknown')
         FROM memory_entries e
         WHERE e.entry_type = 'prior' AND e.source_type = 'user_statement'
           AND NOT EXISTS (
               SELECT 1 FROM prior_clusters c WHERE c.promoted_memory_id = e.id
           )
         ORDER BY e.id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(PriorProposal {
            id: row.get(0)?,
            title: row.get(1)?,
            content: row.get(2)?,
            status: row.get(3)?,
            // The reviewed 2026-09-27 migration already converted approved
            // targets. An unknown selector is not a safe durable trigger.
            proposed_type: EntryType::Prior,
            proposed_triggers: Vec::new(),
        })
    })?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

pub fn format_proposals(rows: &[PriorProposal], format: OutputFormat) -> Result<String> {
    match format {
        OutputFormat::Json => Ok(serde_json::to_string_pretty(rows)?),
        OutputFormat::Csv => {
            let mut out = String::from("id,title,status,proposed_type,proposed_triggers\n");
            for row in rows {
                out.push_str(&format!(
                    "{},{},{},{},[]\n",
                    csv_field(&row.id),
                    csv_field(&row.title),
                    csv_field(&row.status),
                    row.proposed_type
                ));
            }
            Ok(out.trim_end().to_owned())
        }
        OutputFormat::Markdown => {
            let mut out = String::from(
                "| ID | Title | Status | Proposed type | Proposed triggers |\n|---|---|---|---|---|",
            );
            for row in rows {
                out.push_str(&format!(
                    "\n| {} | {} | {} | {} | [] |",
                    markdown_field(&row.id),
                    markdown_field(&row.title),
                    markdown_field(&row.status),
                    row.proposed_type
                ));
            }
            Ok(out)
        }
        OutputFormat::Text => {
            if rows.is_empty() {
                return Ok("No cluster-less user_statement priors found.".to_owned());
            }
            let mut out = String::new();
            for row in rows {
                out.push_str(&format!(
                    "{} ({}) -> {} []: {:?}\n",
                    row.id, row.status, row.proposed_type, row.title
                ));
            }
            Ok(out.trim_end().to_owned())
        }
    }
}

fn csv_field(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn markdown_field(value: &str) -> String {
    value.replace('|', "\\|").replace(['\n', '\r'], " ")
}
