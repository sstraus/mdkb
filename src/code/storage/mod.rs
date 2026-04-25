//! SQLite storage backend for the code intelligence index.

pub mod schema;

mod sqlite;
pub use sqlite::CodeDb;
