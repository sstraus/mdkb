//! Tantivy-backed storage for the code intelligence index.
//!
//! Provides a single Tantivy index that stores symbols, relationships,
//! files, and metadata as distinct document types. An ngram tokenizer
//! enables partial symbol name matching ("Archive" finds "ArchiveAppService").

pub mod schema;

use std::fmt;
use std::path::{Path, PathBuf};

use tantivy::tokenizer::{NgramTokenizer, TextAnalyzer};
use tantivy::{Index, IndexReader, ReloadPolicy};

pub use schema::IndexSchema;

/// Tantivy-backed code intelligence index.
///
/// Wraps a Tantivy `Index` with a pre-built schema and registered
/// tokenizers. Create with [`CodeIndex::create`] or open an existing
/// one with [`CodeIndex::open`].
pub struct CodeIndex {
    index: Index,
    reader: IndexReader,
    schema: IndexSchema,
    path: PathBuf,
}

/// Default heap budget for the Tantivy index writer (50 MB).
const DEFAULT_HEAP_SIZE: usize = 50_000_000;

impl CodeIndex {
    /// Create a new index at `path`. Fails if the directory already
    /// contains a Tantivy index (use [`open`] instead).
    pub fn create(path: impl AsRef<Path>) -> tantivy::Result<Self> {
        let path = path.as_ref().to_path_buf();
        std::fs::create_dir_all(&path)
            .map_err(|e| tantivy::TantivyError::IoError(e.into()))?;

        let (tantivy_schema, index_schema) = IndexSchema::build();
        let dir = tantivy::directory::MmapDirectory::open(&path)?;
        let index = Index::create(dir, tantivy_schema, tantivy::IndexSettings::default())?;

        Self::register_tokenizers(&index);
        let reader = Self::build_reader(&index)?;

        Ok(Self {
            index,
            reader,
            schema: index_schema,
            path,
        })
    }

    /// Open an existing index at `path`.
    pub fn open(path: impl AsRef<Path>) -> tantivy::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let (_, index_schema) = IndexSchema::build();
        let index = Index::open_in_dir(&path)?;

        Self::register_tokenizers(&index);
        let reader = Self::build_reader(&index)?;
        reader.reload()?;

        Ok(Self {
            index,
            reader,
            schema: index_schema,
            path,
        })
    }

    /// Open an existing index, or create one if none exists at `path`.
    pub fn open_or_create(path: impl AsRef<Path>) -> tantivy::Result<Self> {
        let path = path.as_ref();
        if path.join("meta.json").exists() {
            Self::open(path)
        } else {
            Self::create(path)
        }
    }

    pub fn schema(&self) -> &IndexSchema {
        &self.schema
    }

    pub fn reader(&self) -> &IndexReader {
        &self.reader
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create an index writer with the default heap budget.
    pub fn writer(&self) -> tantivy::Result<tantivy::IndexWriter> {
        self.index.writer(DEFAULT_HEAP_SIZE)
    }

    /// Create an index writer with a custom heap budget (bytes).
    pub fn writer_with_heap(&self, heap_bytes: usize) -> tantivy::Result<tantivy::IndexWriter> {
        let clamped = heap_bytes.clamp(10_000_000, 1_000_000_000);
        self.index.writer(clamped)
    }

    /// Reload the reader to pick up newly committed segments.
    pub fn reload(&self) -> tantivy::Result<()> {
        self.reader.reload()
    }

    // -- private helpers --

    fn register_tokenizers(index: &Index) {
        // Ngram tokenizer (3-10 grams) for partial symbol name matching.
        let ngram = TextAnalyzer::builder(NgramTokenizer::new(3, 10, false).unwrap()).build();
        index.tokenizers().register("ngram", ngram);
    }

    fn build_reader(index: &Index) -> tantivy::Result<IndexReader> {
        index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
    }
}

impl fmt::Debug for CodeIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CodeIndex")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_index");

        // Create
        let idx = CodeIndex::create(&path).unwrap();
        assert!(path.join("meta.json").exists());
        assert_eq!(idx.path(), path);
        drop(idx);

        // Re-open
        let idx = CodeIndex::open(&path).unwrap();
        assert_eq!(idx.path(), path);
        drop(idx);
    }

    #[test]
    fn test_open_or_create_creates_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new_index");

        let idx = CodeIndex::open_or_create(&path).unwrap();
        assert!(path.join("meta.json").exists());
        drop(idx);
    }

    #[test]
    fn test_open_or_create_opens_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing_index");

        CodeIndex::create(&path).unwrap();
        // open_or_create should open, not fail
        let idx = CodeIndex::open_or_create(&path).unwrap();
        assert_eq!(idx.path(), path);
        drop(idx);
    }

    #[test]
    fn test_writer_creation() {
        let dir = tempfile::tempdir().unwrap();
        let idx = CodeIndex::create(dir.path().join("writer_test")).unwrap();

        let writer = idx.writer();
        assert!(writer.is_ok());
    }

    #[test]
    fn test_schema_accessible() {
        let dir = tempfile::tempdir().unwrap();
        let idx = CodeIndex::create(dir.path().join("schema_test")).unwrap();
        let schema = idx.schema();

        // Verify we can read fields through the accessor
        let tantivy_schema = idx.index().schema();
        let field_entry = tantivy_schema.get_field_entry(schema.doc_type);
        assert_eq!(field_entry.name(), "doc_type");
    }

    #[test]
    fn test_write_and_read_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let idx = CodeIndex::create(dir.path().join("roundtrip_test")).unwrap();

        // Write a document
        let mut writer = idx.writer().unwrap();
        let schema = idx.schema();
        let doc = tantivy::doc!(
            schema.doc_type => "symbol",
            schema.symbol_id => 1u64,
            schema.name => "test_fn",
            schema.kind => "Function"
        );
        writer.add_document(doc).unwrap();
        writer.commit().unwrap();

        // Reload and search
        idx.reload().unwrap();
        let searcher = idx.reader().searcher();
        assert_eq!(searcher.num_docs(), 1);
    }
}
