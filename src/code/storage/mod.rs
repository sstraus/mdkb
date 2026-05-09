//! SQLite storage backend for the code intelligence index.

pub mod repair;
pub mod schema;

mod sqlite;
pub use sqlite::CodeDb;
