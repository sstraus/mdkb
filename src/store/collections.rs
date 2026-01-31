//! Collection storage operations.

use crate::domain::Collection;
use crate::error::{ErrorKind, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Add a new collection.
pub fn add_collection(conn: &Connection, collection: &Collection) -> Result<()> {
    conn.execute(
        "INSERT INTO collections (name, path, pattern, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            collection.name,
            collection.path,
            collection.pattern,
            collection.created_at,
            collection.updated_at,
        ],
    )?;
    Ok(())
}

/// Get a collection by name.
pub fn get_collection(conn: &Connection, name: &str) -> Result<Option<Collection>> {
    let result = conn
        .query_row(
            "SELECT name, path, pattern, created_at, updated_at FROM collections WHERE name = ?1",
            params![name],
            |row| {
                Ok(Collection {
                    name: row.get(0)?,
                    path: row.get(1)?,
                    pattern: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
                })
            },
        )
        .optional()?;
    Ok(result)
}

/// List all collections.
pub fn list_collections(conn: &Connection) -> Result<Vec<Collection>> {
    let mut stmt =
        conn.prepare("SELECT name, path, pattern, created_at, updated_at FROM collections")?;
    let rows = stmt.query_map([], |row| {
        Ok(Collection {
            name: row.get(0)?,
            path: row.get(1)?,
            pattern: row.get(2)?,
            created_at: row.get(3)?,
            updated_at: row.get(4)?,
        })
    })?;

    let mut collections = Vec::new();
    for row in rows {
        collections.push(row?);
    }
    Ok(collections)
}

/// Remove a collection by name.
pub fn remove_collection(conn: &Connection, name: &str) -> Result<bool> {
    let rows_affected = conn.execute("DELETE FROM collections WHERE name = ?1", params![name])?;
    Ok(rows_affected > 0)
}

/// Rename a collection.
pub fn rename_collection(conn: &Connection, old_name: &str, new_name: &str) -> Result<()> {
    let rows_affected = conn.execute(
        "UPDATE collections SET name = ?1 WHERE name = ?2",
        params![new_name, old_name],
    )?;

    if rows_affected == 0 {
        return Err(ErrorKind::CollectionNotFound {
            name: old_name.to_string(),
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::schema::init_schema;
    use chrono::Utc;

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn make_collection(name: &str, path: &str) -> Collection {
        let now = Utc::now().timestamp();
        Collection {
            name: name.to_string(),
            path: path.to_string(),
            pattern: "**/*.md".to_string(),
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn test_add_and_get_collection() {
        let conn = setup_db();
        let coll = make_collection("docs", "./docs");

        add_collection(&conn, &coll).expect("add should succeed");

        let retrieved = get_collection(&conn, "docs")
            .expect("get should succeed")
            .expect("collection should exist");

        assert_eq!(retrieved.name, "docs");
        assert_eq!(retrieved.path, "./docs");
        assert_eq!(retrieved.pattern, "**/*.md");
    }

    #[test]
    fn test_add_duplicate_collection_fails() {
        let conn = setup_db();
        let coll = make_collection("docs", "./docs");

        add_collection(&conn, &coll).expect("first add should succeed");
        let result = add_collection(&conn, &coll);

        assert!(result.is_err());
    }

    #[test]
    fn test_get_nonexistent_collection() {
        let conn = setup_db();

        let result = get_collection(&conn, "nonexistent").expect("get should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn test_list_collections() {
        let conn = setup_db();

        add_collection(&conn, &make_collection("docs", "./docs")).unwrap();
        add_collection(&conn, &make_collection("notes", "./notes")).unwrap();
        add_collection(&conn, &make_collection("wiki", "./wiki")).unwrap();

        let collections = list_collections(&conn).expect("list should succeed");
        assert_eq!(collections.len(), 3);

        let names: Vec<_> = collections.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"docs"));
        assert!(names.contains(&"notes"));
        assert!(names.contains(&"wiki"));
    }

    #[test]
    fn test_list_empty_collections() {
        let conn = setup_db();
        let collections = list_collections(&conn).expect("list should succeed");
        assert!(collections.is_empty());
    }

    #[test]
    fn test_remove_collection() {
        let conn = setup_db();
        add_collection(&conn, &make_collection("docs", "./docs")).unwrap();

        let removed = remove_collection(&conn, "docs").expect("remove should succeed");
        assert!(removed);

        let result = get_collection(&conn, "docs").expect("get should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn test_remove_nonexistent_collection() {
        let conn = setup_db();

        let removed = remove_collection(&conn, "nonexistent").expect("remove should succeed");
        assert!(!removed);
    }

    #[test]
    fn test_rename_collection() {
        let conn = setup_db();
        add_collection(&conn, &make_collection("old_name", "./path")).unwrap();

        rename_collection(&conn, "old_name", "new_name").expect("rename should succeed");

        assert!(get_collection(&conn, "old_name").unwrap().is_none());
        let renamed = get_collection(&conn, "new_name")
            .unwrap()
            .expect("should exist");
        assert_eq!(renamed.path, "./path");
    }

    #[test]
    fn test_rename_nonexistent_collection() {
        let conn = setup_db();

        let result = rename_collection(&conn, "nonexistent", "new_name");
        assert!(result.is_err());
    }

    #[test]
    fn test_rename_to_existing_name_fails() {
        let conn = setup_db();
        add_collection(&conn, &make_collection("first", "./first")).unwrap();
        add_collection(&conn, &make_collection("second", "./second")).unwrap();

        let result = rename_collection(&conn, "first", "second");
        assert!(result.is_err());
    }
}
