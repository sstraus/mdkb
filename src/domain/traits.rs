//! Storage traits for hexagonal architecture.
//!
//! These traits define the interface between domain and storage layers.
//! The storage layer implements these traits, allowing domain logic to
//! be tested with mock implementations.

use crate::domain::{Collection, Document, IndexStatus, SearchQuery, SearchResult};
use crate::error::Result;

/// Trait for document storage operations.
pub trait DocumentStore {
    /// Get a document by ID.
    fn get_document(&self, id: i64) -> Result<Option<Document>>;

    /// Get a document by collection and path.
    fn get_document_by_path(&self, collection: &str, path: &str) -> Result<Option<Document>>;

    /// Get document content by hash.
    fn get_content(&self, hash: &str) -> Result<Option<String>>;

    /// Store document content, returns the hash.
    fn store_content(&mut self, content: &str) -> Result<String>;

    /// Index a document (insert or update).
    fn index_document(&mut self, doc: &Document, content: &str) -> Result<i64>;

    /// Delete a document by ID.
    fn delete_document(&mut self, id: i64) -> Result<bool>;

    /// Delete documents by collection.
    fn delete_documents_by_collection(&mut self, collection: &str) -> Result<usize>;

    /// List all documents in a collection.
    fn list_documents(&self, collection: &str) -> Result<Vec<Document>>;
}

/// Trait for collection management.
pub trait CollectionStore {
    /// Get a collection by name.
    fn get_collection(&self, name: &str) -> Result<Option<Collection>>;

    /// List all collections.
    fn list_collections(&self) -> Result<Vec<Collection>>;

    /// Create a new collection.
    fn create_collection(&mut self, collection: &Collection) -> Result<()>;

    /// Update a collection.
    fn update_collection(&mut self, collection: &Collection) -> Result<()>;

    /// Delete a collection.
    fn delete_collection(&mut self, name: &str) -> Result<bool>;

    /// Rename a collection.
    fn rename_collection(&mut self, old_name: &str, new_name: &str) -> Result<()>;
}

/// Trait for search operations.
pub trait SearchEngine {
    /// Perform full-text search with BM25 ranking.
    fn search(&self, query: &SearchQuery) -> Result<Vec<SearchResult>>;

    /// Get index status.
    fn get_status(&self) -> Result<IndexStatus>;
}

/// Trait for tag operations.
pub trait TagStore {
    /// Add tags to a document.
    fn add_tags(&mut self, document_id: i64, tags: &[String]) -> Result<()>;

    /// Remove all tags from a document.
    fn remove_tags(&mut self, document_id: i64) -> Result<()>;

    /// Get tags for a document.
    fn get_document_tags(&self, document_id: i64) -> Result<Vec<String>>;

    /// Find documents with a specific tag.
    fn find_by_tag(&self, tag: &str) -> Result<Vec<i64>>;

    /// List all unique tags.
    fn list_all_tags(&self) -> Result<Vec<String>>;
}

/// Trait for wiki-link operations.
pub trait LinkStore {
    /// Add links from a document.
    fn add_links(
        &mut self,
        source_doc_id: i64,
        links: &[(String, Option<String>, usize)],
    ) -> Result<()>;

    /// Remove all links from a document.
    fn remove_links(&mut self, source_doc_id: i64) -> Result<()>;

    /// Get backlinks (documents linking to a target path).
    fn get_backlinks(&self, target_path: &str) -> Result<Vec<(i64, String)>>;

    /// Get outgoing links from a document.
    fn get_outgoing_links(&self, source_doc_id: i64) -> Result<Vec<String>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Mock implementation for testing
    struct MockDocumentStore {
        documents: HashMap<i64, Document>,
        content: HashMap<String, String>,
        next_id: i64,
    }

    impl MockDocumentStore {
        fn new() -> Self {
            Self {
                documents: HashMap::new(),
                content: HashMap::new(),
                next_id: 1,
            }
        }
    }

    impl DocumentStore for MockDocumentStore {
        fn get_document(&self, id: i64) -> Result<Option<Document>> {
            Ok(self.documents.get(&id).cloned())
        }

        fn get_document_by_path(&self, collection: &str, path: &str) -> Result<Option<Document>> {
            Ok(self
                .documents
                .values()
                .find(|d| d.collection == collection && d.relative_path == path)
                .cloned())
        }

        fn get_content(&self, hash: &str) -> Result<Option<String>> {
            Ok(self.content.get(hash).cloned())
        }

        fn store_content(&mut self, content: &str) -> Result<String> {
            use sha2::{Digest, Sha256};
            let hash = format!("{:x}", Sha256::digest(content.as_bytes()));
            self.content.insert(hash.clone(), content.to_string());
            Ok(hash)
        }

        fn index_document(&mut self, doc: &Document, content: &str) -> Result<i64> {
            let hash = self.store_content(content)?;
            let id = self.next_id;
            self.next_id += 1;

            let mut doc = doc.clone();
            doc.id = id;
            doc.hash = hash;
            self.documents.insert(id, doc);
            Ok(id)
        }

        fn delete_document(&mut self, id: i64) -> Result<bool> {
            Ok(self.documents.remove(&id).is_some())
        }

        fn delete_documents_by_collection(&mut self, collection: &str) -> Result<usize> {
            let ids: Vec<_> = self
                .documents
                .iter()
                .filter(|(_, d)| d.collection == collection)
                .map(|(id, _)| *id)
                .collect();
            let count = ids.len();
            for id in ids {
                self.documents.remove(&id);
            }
            Ok(count)
        }

        fn list_documents(&self, collection: &str) -> Result<Vec<Document>> {
            Ok(self
                .documents
                .values()
                .filter(|d| d.collection == collection)
                .cloned()
                .collect())
        }
    }

    #[test]
    fn test_mock_store_index_and_get() {
        let mut store = MockDocumentStore::new();

        let doc = Document {
            id: 0,
            collection: "test".to_string(),
            relative_path: "readme.md".to_string(),
            hash: String::new(),
            title: Some("Test".to_string()),
            metadata: None,
            file_modified_at: 1234567890,
            indexed_at: 1234567890,
            status: None,
        };

        let id = store.index_document(&doc, "# Test\n\nContent").unwrap();
        assert!(id > 0);

        let retrieved = store.get_document(id).unwrap().unwrap();
        assert_eq!(retrieved.relative_path, "readme.md");
        assert!(!retrieved.hash.is_empty());

        let content = store.get_content(&retrieved.hash).unwrap().unwrap();
        assert_eq!(content, "# Test\n\nContent");
    }

    #[test]
    fn test_mock_store_delete() {
        let mut store = MockDocumentStore::new();

        let doc = Document {
            id: 0,
            collection: "test".to_string(),
            relative_path: "readme.md".to_string(),
            hash: String::new(),
            title: None,
            metadata: None,
            file_modified_at: 0,
            indexed_at: 0,
            status: None,
        };

        let id = store.index_document(&doc, "content").unwrap();
        assert!(store.get_document(id).unwrap().is_some());

        let deleted = store.delete_document(id).unwrap();
        assert!(deleted);
        assert!(store.get_document(id).unwrap().is_none());
    }

    #[test]
    fn test_mock_store_list_by_collection() {
        let mut store = MockDocumentStore::new();

        for i in 0..3 {
            let doc = Document {
                id: 0,
                collection: "docs".to_string(),
                relative_path: format!("doc{i}.md"),
                hash: String::new(),
                title: None,
                metadata: None,
                file_modified_at: 0,
                indexed_at: 0,
                status: None,
            };
            store.index_document(&doc, &format!("content {i}")).unwrap();
        }

        let doc = Document {
            id: 0,
            collection: "other".to_string(),
            relative_path: "other.md".to_string(),
            hash: String::new(),
            title: None,
            metadata: None,
            file_modified_at: 0,
            indexed_at: 0,
            status: None,
        };
        store.index_document(&doc, "other content").unwrap();

        let docs = store.list_documents("docs").unwrap();
        assert_eq!(docs.len(), 3);

        let other = store.list_documents("other").unwrap();
        assert_eq!(other.len(), 1);
    }
}
