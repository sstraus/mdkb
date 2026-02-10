//! Embedding-based semantic search over code symbols.
//!
//! Uses fastembed (ONNX Runtime) with AllMiniLML6V2 (384-dim) for local
//! embedding generation and brute-force cosine similarity for search.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, bail, ensure};
use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};

use crate::code::types::SymbolKind;

/// Embedding dimensionality for AllMiniLML6V2.
const EMBEDDING_DIM: usize = 384;

/// Magic bytes identifying our vector store file format.
const MAGIC: &[u8; 4] = b"MDVS";

/// Current file format version.
const FORMAT_VERSION: u32 = 1;

/// Header size in bytes: magic(4) + version(4) + dim(4) + count(4) = 16.
const HEADER_SIZE: usize = 16;

/// Maximum characters of text to embed per symbol.
const MAX_EMBED_TEXT_LEN: usize = 500;

// ---------------------------------------------------------------------------
// VectorStore — binary file storage for (symbol_id, embedding) entries
// ---------------------------------------------------------------------------

/// Binary file storage for `(symbol_id: u32, embedding: [f32; DIM])` entries.
///
/// File format:
/// ```text
/// Header (16 bytes): magic b"MDVS", version: u32, dim: u32, count: u32
/// Entries: [symbol_id: u32, embedding: [f32; dim]] * count
/// ```
#[derive(Debug)]
pub struct VectorStore {
    path: PathBuf,
}

impl VectorStore {
    /// Open or create a vector store at the given path.
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_path_buf();

        if path.exists() {
            // Validate existing header
            let mut file = std::fs::File::open(&path)
                .with_context(|| format!("Failed to open vector store: {}", path.display()))?;
            let mut header = [0u8; HEADER_SIZE];
            match file.read_exact(&mut header) {
                Ok(()) => validate_header(&header)?,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // File too small — treat as empty/corrupt, recreate
                    tracing::warn!("Vector store too small, recreating: {}", path.display());
                    write_empty_file(&path)?;
                }
                Err(e) => return Err(e).context("Failed to read vector store header"),
            }
        } else {
            write_empty_file(&path)?;
        }

        Ok(Self { path })
    }

    /// Write all entries to the store (overwrites existing data).
    pub fn write_all(&self, entries: &[(u32, Vec<f32>)]) -> anyhow::Result<()> {
        let count = entries.len() as u32;
        let dim = EMBEDDING_DIM as u32;

        let mut buf = Vec::with_capacity(HEADER_SIZE + entries.len() * entry_size());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&dim.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());

        for (id, embedding) in entries {
            ensure!(
                embedding.len() == EMBEDDING_DIM,
                "Embedding dimension mismatch: expected {EMBEDDING_DIM}, got {}",
                embedding.len()
            );
            buf.extend_from_slice(&id.to_le_bytes());
            for &val in embedding {
                buf.extend_from_slice(&val.to_le_bytes());
            }
        }

        std::fs::write(&self.path, &buf)
            .with_context(|| format!("Failed to write vector store: {}", self.path.display()))?;
        Ok(())
    }

    /// Load all entries from the store into memory.
    pub fn load(&self) -> anyhow::Result<Vec<(u32, Vec<f32>)>> {
        let data = std::fs::read(&self.path)
            .with_context(|| format!("Failed to read vector store: {}", self.path.display()))?;

        if data.len() < HEADER_SIZE {
            bail!("Vector store file too small");
        }

        validate_header(&data[..HEADER_SIZE])?;
        let count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
        let entry_sz = entry_size();

        ensure!(
            data.len() == HEADER_SIZE + count * entry_sz,
            "Vector store size mismatch: expected {} bytes, got {}",
            HEADER_SIZE + count * entry_sz,
            data.len()
        );

        let mut entries = Vec::with_capacity(count);
        let mut offset = HEADER_SIZE;

        for _ in 0..count {
            let id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;

            let mut embedding = Vec::with_capacity(EMBEDDING_DIM);
            for _ in 0..EMBEDDING_DIM {
                let val = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                embedding.push(val);
                offset += 4;
            }

            entries.push((id, embedding));
        }

        Ok(entries)
    }

    /// Clear the store (reset to empty header).
    pub fn clear(&self) -> anyhow::Result<()> {
        write_empty_file(&self.path)
    }

    /// Entry count from the header (without loading all data).
    pub fn count(&self) -> anyhow::Result<usize> {
        let data = std::fs::read(&self.path)
            .with_context(|| format!("Failed to read vector store: {}", self.path.display()))?;
        if data.len() < HEADER_SIZE {
            return Ok(0);
        }
        let count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
        Ok(count)
    }
}

/// Size of one entry in bytes: u32 symbol_id + EMBEDDING_DIM * f32.
fn entry_size() -> usize {
    4 + EMBEDDING_DIM * 4
}

/// Write an empty vector store file (header only).
fn write_empty_file(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = Vec::with_capacity(HEADER_SIZE);
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&(EMBEDDING_DIM as u32).to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes()); // count = 0
    std::fs::write(path, &buf)
        .with_context(|| format!("Failed to write empty vector store: {}", path.display()))?;
    Ok(())
}

/// Validate a 16-byte header.
fn validate_header(header: &[u8]) -> anyhow::Result<()> {
    ensure!(header.len() >= HEADER_SIZE, "Header too small");
    ensure!(&header[0..4] == MAGIC, "Invalid magic bytes in vector store");
    let version = u32::from_le_bytes(header[4..8].try_into().unwrap());
    ensure!(
        version == FORMAT_VERSION,
        "Unsupported vector store version: {version} (expected {FORMAT_VERSION})"
    );
    let dim = u32::from_le_bytes(header[8..12].try_into().unwrap());
    ensure!(
        dim == EMBEDDING_DIM as u32,
        "Dimension mismatch: file has {dim}, expected {EMBEDDING_DIM}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Cosine similarity
// ---------------------------------------------------------------------------

/// Compute cosine similarity between two vectors.
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }
    let denom = norm_a.sqrt() * norm_b.sqrt();
    if denom < f32::EPSILON {
        0.0
    } else {
        dot / denom
    }
}

// ---------------------------------------------------------------------------
// Format symbol text for embedding
// ---------------------------------------------------------------------------

/// Format a symbol's metadata into a text string suitable for embedding.
///
/// Format: `"{kind} {name}\n{signature}\n{doc_comment}"` (truncated to 500 chars).
pub fn format_symbol_text(kind: SymbolKind, name: &str, signature: Option<&str>, doc_comment: Option<&str>) -> String {
    let mut text = format!("{kind} {name}");
    if let Some(sig) = signature {
        text.push('\n');
        text.push_str(sig);
    }
    if let Some(doc) = doc_comment {
        text.push('\n');
        text.push_str(doc);
    }
    if text.len() > MAX_EMBED_TEXT_LEN {
        text.truncate(MAX_EMBED_TEXT_LEN);
    }
    text
}

// ---------------------------------------------------------------------------
// SemanticSearch — orchestrates embedding generation and search
// ---------------------------------------------------------------------------

/// A result from semantic search: symbol ID and similarity score.
#[derive(Debug, Clone)]
pub struct SemanticMatch {
    pub symbol_id: u32,
    pub score: f32,
}

/// Orchestrates embedding generation and brute-force search over code symbols.
pub struct SemanticSearch {
    model: Mutex<Option<TextEmbedding>>,
    store: VectorStore,
}

impl std::fmt::Debug for SemanticSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticSearch")
            .field("store", &self.store)
            .field("model_loaded", &self.model.lock().map(|g| g.is_some()).unwrap_or(false))
            .finish()
    }
}

impl SemanticSearch {
    /// Create a new semantic search instance with a vector store at the given path.
    ///
    /// The embedding model is initialized lazily on first use.
    pub fn new(store_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let store = VectorStore::open(store_path)?;
        Ok(Self {
            model: Mutex::new(None),
            store,
        })
    }

    /// Initialize the embedding model on a blocking thread.
    ///
    /// Call this from async context before using `generate_embeddings` or `search`.
    /// Uses `spawn_blocking` so model download/init won't block the tokio executor.
    /// No-op if the model is already loaded.
    pub async fn init_model_async(&self) -> anyhow::Result<()> {
        // Fast path: already loaded
        {
            let guard = self.model.lock().map_err(|e| anyhow::anyhow!("Model lock poisoned: {e}"))?;
            if guard.is_some() {
                return Ok(());
            }
        }

        // Slow path: init on blocking thread
        tracing::info!("Initializing fastembed model (AllMiniLML6V2)...");
        let model = tokio::task::spawn_blocking(|| {
            TextEmbedding::try_new(
                InitOptions::new(EmbeddingModel::AllMiniLML6V2).with_show_download_progress(true),
            )
            .context("Failed to initialize fastembed model")
        })
        .await
        .context("Model init task panicked")??;

        tracing::info!("fastembed model ready");
        let mut guard = self.model.lock().map_err(|e| anyhow::anyhow!("Model lock poisoned: {e}"))?;
        *guard = Some(model);
        Ok(())
    }

    /// Ensure the embedding model is loaded, initializing it if needed.
    ///
    /// Prefer `init_model_async` from async context. This sync version blocks
    /// the current thread during model download and should only be used from
    /// non-async callers (e.g. the indexing pipeline).
    fn ensure_model(&self) -> anyhow::Result<()> {
        let mut guard = self.model.lock().map_err(|e| anyhow::anyhow!("Model lock poisoned: {e}"))?;
        if guard.is_none() {
            tracing::info!("Initializing fastembed model (AllMiniLML6V2)...");
            let model = TextEmbedding::try_new(
                InitOptions::new(EmbeddingModel::AllMiniLML6V2).with_show_download_progress(true),
            )
            .context("Failed to initialize fastembed model")?;
            tracing::info!("fastembed model ready");
            *guard = Some(model);
        }
        Ok(())
    }

    /// Generate embeddings for a batch of symbols and write them to the vector store.
    ///
    /// Each entry is `(symbol_id, text_to_embed)`.
    pub fn generate_embeddings(&self, symbols: &[(u32, String)]) -> anyhow::Result<()> {
        if symbols.is_empty() {
            return self.store.clear();
        }

        self.ensure_model()?;

        let texts: Vec<&str> = symbols.iter().map(|(_, text)| text.as_str()).collect();
        let embeddings = {
            let mut guard = self.model.lock().map_err(|e| anyhow::anyhow!("Model lock poisoned: {e}"))?;
            let model = guard.as_mut().expect("model initialized by ensure_model");
            model.embed(texts, None).context("Failed to generate embeddings")?
        };

        let entries: Vec<(u32, Vec<f32>)> = symbols
            .iter()
            .zip(embeddings)
            .map(|((id, _), emb)| (*id, emb))
            .collect();

        self.store.write_all(&entries)?;
        tracing::info!("Generated {} embeddings", entries.len());
        Ok(())
    }

    /// Search for symbols similar to the given query text.
    ///
    /// Returns up to `limit` results with similarity >= `threshold`, sorted by score descending.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
    ) -> anyhow::Result<Vec<SemanticMatch>> {
        self.ensure_model()?;

        // Embed the query
        let query_embedding = {
            let mut guard = self.model.lock().map_err(|e| anyhow::anyhow!("Model lock poisoned: {e}"))?;
            let model = guard.as_mut().expect("model initialized by ensure_model");
            let mut embeddings = model
                .embed(vec![query], None)
                .context("Failed to embed query")?;
            embeddings.remove(0)
        };

        // Load stored embeddings
        let entries = self.store.load()?;

        // Brute-force cosine similarity
        let mut scored: Vec<SemanticMatch> = entries
            .iter()
            .map(|(id, emb)| SemanticMatch {
                symbol_id: *id,
                score: cosine_similarity(&query_embedding, emb),
            })
            .filter(|m| m.score >= threshold)
            .collect();

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);

        Ok(scored)
    }

    /// Clear the vector store.
    pub fn clear(&self) -> anyhow::Result<()> {
        self.store.clear()
    }

    /// Number of stored embeddings.
    pub fn count(&self) -> anyhow::Result<usize> {
        self.store.count()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- VectorStore unit tests (no model download) ---

    #[test]
    fn test_vector_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("test.bin")).unwrap();

        let entries: Vec<(u32, Vec<f32>)> = vec![
            (1, vec![0.1; EMBEDDING_DIM]),
            (2, vec![0.2; EMBEDDING_DIM]),
            (3, vec![0.3; EMBEDDING_DIM]),
        ];

        store.write_all(&entries).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 3);
        assert_eq!(loaded[0].0, 1);
        assert_eq!(loaded[1].0, 2);
        assert_eq!(loaded[2].0, 3);

        // Check embedding values
        assert!((loaded[0].1[0] - 0.1).abs() < f32::EPSILON);
        assert!((loaded[1].1[0] - 0.2).abs() < f32::EPSILON);
        assert!((loaded[2].1[0] - 0.3).abs() < f32::EPSILON);
        assert_eq!(loaded[0].1.len(), EMBEDDING_DIM);
    }

    #[test]
    fn test_vector_store_clear() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("test.bin")).unwrap();

        let entries = vec![(1, vec![0.5; EMBEDDING_DIM])];
        store.write_all(&entries).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        store.clear().unwrap();
        assert_eq!(store.count().unwrap(), 0);

        let loaded = store.load().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_vector_store_corrupt_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.bin");

        // Write garbage
        std::fs::write(&path, b"XXXX1234567890AB").unwrap();

        let result = VectorStore::open(&path);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Invalid magic bytes"),
            "Expected magic bytes error, got: {err}"
        );
    }

    #[test]
    fn test_vector_store_empty_on_create() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("new.bin")).unwrap();

        assert_eq!(store.count().unwrap(), 0);
        let loaded = store.load().unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn test_vector_store_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("persist.bin");

        // Write with one instance
        let store = VectorStore::open(&path).unwrap();
        store.write_all(&[(42, vec![1.0; EMBEDDING_DIM])]).unwrap();

        // Reopen and verify
        let store2 = VectorStore::open(&path).unwrap();
        let loaded = store2.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, 42);
    }

    // --- Cosine similarity tests ---

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - 1.0).abs() < 1e-5, "Identical vectors should have similarity ~1.0, got {sim}");
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-5, "Orthogonal vectors should have similarity ~0.0, got {sim}");
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!((sim - (-1.0)).abs() < 1e-5, "Opposite vectors should have similarity ~-1.0, got {sim}");
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![0.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(sim.abs() < 1e-5, "Zero vector should have similarity 0.0, got {sim}");
    }

    // --- format_symbol_text tests ---

    #[test]
    fn test_format_symbol_text_full() {
        let text = format_symbol_text(
            SymbolKind::Function,
            "process_data",
            Some("fn process_data(input: &[u8]) -> Result<()>"),
            Some("Processes raw data and returns results."),
        );
        assert!(text.starts_with("Function process_data"));
        assert!(text.contains("fn process_data(input: &[u8]) -> Result<()>"));
        assert!(text.contains("Processes raw data"));
    }

    #[test]
    fn test_format_symbol_text_name_only() {
        let text = format_symbol_text(SymbolKind::Struct, "MyConfig", None, None);
        assert_eq!(text, "Struct MyConfig");
    }

    #[test]
    fn test_format_symbol_text_with_signature_only() {
        let text = format_symbol_text(
            SymbolKind::Method,
            "connect",
            Some("fn connect(&self, url: &str) -> Connection"),
            None,
        );
        assert_eq!(text, "Method connect\nfn connect(&self, url: &str) -> Connection");
    }

    #[test]
    fn test_format_symbol_text_truncation() {
        let long_doc = "x".repeat(600);
        let text = format_symbol_text(SymbolKind::Function, "f", None, Some(&long_doc));
        assert!(text.len() <= MAX_EMBED_TEXT_LEN);
    }

    // --- Integration tests (require model download) ---

    #[test]
    #[ignore]
    fn test_semantic_search_basic() {
        let dir = tempfile::tempdir().unwrap();
        let search = SemanticSearch::new(dir.path().join("vectors.bin")).unwrap();

        let symbols = vec![
            (1, "Function authenticate_user\nfn authenticate_user(username: &str, password: &str) -> bool\nVerifies user credentials against the database.".to_string()),
            (2, "Function calculate_tax\nfn calculate_tax(amount: f64, rate: f64) -> f64\nComputes tax amount for a given price and rate.".to_string()),
            (3, "Struct DatabasePool\nstruct DatabasePool\nManages database connection pooling.".to_string()),
            (4, "Function send_email\nfn send_email(to: &str, subject: &str, body: &str) -> Result<()>\nSends an email notification.".to_string()),
        ];

        search.generate_embeddings(&symbols).unwrap();
        assert_eq!(search.count().unwrap(), 4);

        let results = search.search("authentication login", 10, 0.0).unwrap();
        assert!(!results.is_empty(), "Should find at least one result");
        // authenticate_user should be the top result
        assert_eq!(
            results[0].symbol_id, 1,
            "authenticate_user should be most similar to 'authentication login'"
        );
    }

    #[test]
    #[ignore]
    fn test_semantic_search_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let search = SemanticSearch::new(dir.path().join("vectors.bin")).unwrap();

        let symbols = vec![
            (1, "Function hello\nfn hello()\nPrints a greeting.".to_string()),
        ];

        search.generate_embeddings(&symbols).unwrap();

        // High threshold should filter out low-similarity results
        let results = search.search("quantum physics equations", 10, 0.9).unwrap();
        assert!(
            results.is_empty(),
            "Unrelated query with high threshold should return no results"
        );
    }
}
