//! Embedding-based semantic search over code symbols.
//!
//! Uses fastembed (ONNX Runtime) with AllMiniLML6V2 (384-dim) for local
//! embedding generation and brute-force cosine similarity for search.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail, ensure};

use crate::code::types::SymbolKind;
use crate::llm::EmbeddingService;

/// Embedding dimensionality for AllMiniLML6V2.
///
/// Public because [`VectorStore::write_all`] rejects any other length: a caller
/// building entries has to know the one width the store accepts.
pub const EMBEDDING_DIM: usize = 384;

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
    ///
    /// Uses write-to-temp + rename for atomic writes (crash-safe on Unix).
    pub fn write_all(&self, entries: &[(u32, Vec<f32>)]) -> anyhow::Result<()> {
        let count = entries.len() as u32;

        let mut buf = Vec::with_capacity(HEADER_SIZE + entries.len() * entry_size());
        push_header(&mut buf, count);

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

        atomic_write(&self.path, &buf)?;
        Ok(())
    }

    /// Load all entries from the store into memory.
    pub fn load(&self) -> anyhow::Result<Vec<(u32, Vec<f32>)>> {
        let data = std::fs::read(&self.path)
            .with_context(|| format!("Failed to read vector store: {}", self.path.display()))?;

        let count = entry_count_of(&data)?;
        let mut entries = Vec::with_capacity(count);
        let mut offset = HEADER_SIZE;

        for _ in 0..count {
            let id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;

            let mut embedding = Vec::with_capacity(EMBEDDING_DIM);
            for _ in 0..EMBEDDING_DIM {
                let val = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                ensure!(
                    val.is_finite(),
                    "Non-finite float in vector store at offset {offset}"
                );
                embedding.push(val);
                offset += 4;
            }

            entries.push((id, embedding));
        }

        Ok(entries)
    }

    /// Remove entries by symbol ID set.
    pub fn remove_by_ids(&self, ids: &HashSet<u32>) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.retain(|id| !ids.contains(&id))
    }

    /// Drop every entry whose id `keep` rejects, returning how many went.
    ///
    /// The file is rewritten only when something was dropped, so a store with
    /// no orphans in it costs one read and no write.
    ///
    /// The entries are never materialised. `load` turns each one into its own
    /// `Vec<f32>`, so sweeping a store of 1.5M vectors — the size a 10k-file
    /// repository reaches — used to allocate 1.5M vectors to decide which four
    /// bytes of each to look at. This reads the ids in place and, when there is
    /// something to drop, copies the survivors' bytes straight through: the
    /// stored form is the written form.
    ///
    /// One thing this deliberately does not do is validate the floats. `load`
    /// rejects a non-finite value and still does; the sweep is not the pass
    /// that should decide a store is corrupt, and refusing to drop orphans
    /// because of an unrelated bad entry left the store growing.
    ///
    /// `keep` is asked about each id exactly once. The answers are held, not
    /// asked again for the copy: the header states how many entries follow it,
    /// so a predicate that answered differently the second time would write a
    /// count that disagrees with the body it precedes.
    pub fn retain(&self, keep: impl Fn(u32) -> bool) -> anyhow::Result<usize> {
        let data = std::fs::read(&self.path)
            .with_context(|| format!("Failed to read vector store: {}", self.path.display()))?;
        let count = entry_count_of(&data)?;
        let entry_sz = entry_size();
        let id_at = |i: usize| {
            let offset = HEADER_SIZE + i * entry_sz;
            u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
        };

        let survives: Vec<bool> = (0..count).map(|i| keep(id_at(i))).collect();
        let kept = survives.iter().filter(|s| **s).count();
        let removed = count - kept;
        if removed == 0 {
            return Ok(0);
        }

        let mut buf = Vec::with_capacity(HEADER_SIZE + kept * entry_sz);
        push_header(&mut buf, kept as u32);
        for (i, _) in survives.iter().enumerate().filter(|(_, s)| **s) {
            let offset = HEADER_SIZE + i * entry_sz;
            buf.extend_from_slice(&data[offset..offset + entry_sz]);
        }

        atomic_write(&self.path, &buf)?;
        Ok(removed)
    }

    /// Clear the store (reset to empty header).
    pub fn clear(&self) -> anyhow::Result<()> {
        write_empty_file(&self.path)
    }

    /// Entry count from the header (reads only 16 bytes, not the full file).
    pub fn count(&self) -> anyhow::Result<usize> {
        let mut file = std::fs::File::open(&self.path)
            .with_context(|| format!("Failed to open vector store: {}", self.path.display()))?;
        let mut header = [0u8; HEADER_SIZE];
        match file.read_exact(&mut header) {
            Ok(()) => {
                let count = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
                Ok(count)
            }
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(0),
            Err(e) => Err(e).context("Failed to read vector store header"),
        }
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
    push_header(&mut buf, 0);
    atomic_write(path, &buf)?;
    Ok(())
}

/// How many entries `data` holds, once its header and length agree.
///
/// Shared by [`VectorStore::load`] and [`VectorStore::retain`]: a truncated or
/// overlong file must be refused the same way by both, or the sweep would index
/// past the end of a file the reader rejects.
fn entry_count_of(data: &[u8]) -> anyhow::Result<usize> {
    if data.len() < HEADER_SIZE {
        bail!("Vector store file too small");
    }
    validate_header(&data[..HEADER_SIZE])?;
    let count = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let expected_data_size = count
        .checked_mul(entry_size())
        .and_then(|s| s.checked_add(HEADER_SIZE))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Vector store count overflow: {count} entries * {} bytes",
                entry_size()
            )
        })?;
    ensure!(
        data.len() == expected_data_size,
        "Vector store size mismatch: expected {} bytes, got {}",
        expected_data_size,
        data.len()
    );
    Ok(count)
}

/// Lay the 16-byte header for a store of `count` entries onto `buf`.
///
/// Three writers produce a store — a full write, an empty file, and the sweep —
/// and all three must agree byte for byte with what [`validate_header`] accepts.
/// They agree by sharing this.
fn push_header(buf: &mut Vec<u8>, count: u32) {
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&(EMBEDDING_DIM as u32).to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
}

/// Write data to a file atomically via write-to-temp + rename.
///
/// On Unix, rename is atomic so a crash during write won't corrupt the target.
fn atomic_write(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let tmp_path = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("vectorstore")
    ));

    std::fs::write(&tmp_path, data)
        .with_context(|| format!("Failed to write temp file: {}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, path).with_context(|| {
        // Clean up temp file on rename failure
        let _ = std::fs::remove_file(&tmp_path);
        format!(
            "Failed to rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    Ok(())
}

/// Validate a 16-byte header.
fn validate_header(header: &[u8]) -> anyhow::Result<()> {
    ensure!(header.len() >= HEADER_SIZE, "Header too small");
    ensure!(
        &header[0..4] == MAGIC,
        "Invalid magic bytes in vector store"
    );
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
pub fn format_symbol_text(
    kind: SymbolKind,
    name: &str,
    signature: Option<&str>,
    doc_comment: Option<&str>,
) -> String {
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
        let mut end = MAX_EMBED_TEXT_LEN;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
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

/// Batch size for embedding generation to avoid loading all symbols at once.
const EMBED_BATCH_SIZE: usize = 5000;

type SymbolEmbedding = (u32, Vec<f32>);
type EmbeddingCache = Mutex<Option<Vec<SymbolEmbedding>>>;

/// Orchestrates embedding generation and brute-force search over code symbols.
///
/// Acquires the embedding service on-demand from the global cache. The global
/// cache handles idle release to free ONNX Runtime memory arenas (multiple GB)
/// when the model hasn't been used for a while.
pub struct SemanticSearch {
    store: VectorStore,
    /// Cached embeddings loaded from disk. Invalidated on write.
    cache: EmbeddingCache,
}

impl std::fmt::Debug for SemanticSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cached = self
            .cache
            .lock()
            .map(|g| g.as_ref().map(|c| c.len()))
            .unwrap_or(None);
        f.debug_struct("SemanticSearch")
            .field("store", &self.store)
            .field("cached_entries", &cached)
            .finish()
    }
}

impl SemanticSearch {
    /// Create a new semantic search instance.
    ///
    /// The embedding service is acquired on-demand, not at construction time.
    pub fn new(store_path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let store = VectorStore::open(store_path)?;
        Ok(Self {
            store,
            cache: Mutex::new(None),
        })
    }

    /// Get the embedding service, initializing it if needed.
    fn service() -> anyhow::Result<Arc<EmbeddingService>> {
        crate::llm::get_cached_service()
            .map_err(|e| anyhow::anyhow!("Failed to get embedding service: {e}"))
    }

    /// Generate embeddings for symbols and write them to the vector store.
    ///
    /// Processes in batches of [`EMBED_BATCH_SIZE`] to bound memory usage.
    /// Each entry is `(symbol_id, text_to_embed)`.
    pub fn generate_embeddings(&self, symbols: &[(u32, String)]) -> anyhow::Result<()> {
        // Invalidate cache since we're rewriting the store
        self.invalidate_cache();

        if symbols.is_empty() {
            return self.store.clear();
        }

        let service = Self::service()?;
        let mut all_entries: Vec<(u32, Vec<f32>)> = Vec::with_capacity(symbols.len());

        for chunk in symbols.chunks(EMBED_BATCH_SIZE) {
            let texts: Vec<&str> = chunk.iter().map(|(_, text)| text.as_str()).collect();
            let embeddings = service
                .embed_documents(&texts)
                .map_err(|e| anyhow::anyhow!("Failed to generate embeddings: {e}"))?;

            for ((id, _), emb) in chunk.iter().zip(embeddings) {
                all_entries.push((*id, emb));
            }

            tracing::info!(
                "Generated embeddings batch: {}/{} symbols",
                all_entries.len(),
                symbols.len()
            );
        }

        self.store.write_all(&all_entries)?;
        tracing::info!("Wrote {} embeddings to store", all_entries.len());
        Ok(())
    }

    /// Load existing embeddings, filtering out entries matching a predicate.
    ///
    /// Returns entries where `keep(symbol_id)` returns true.
    ///
    /// A read that failed is returned as a failure rather than flattened into
    /// an empty result. The caller adds what it just embedded to what this
    /// returns and writes the sum back as the whole store, so "unreadable" read
    /// as "holds nothing" deletes every vector the failed read was holding.
    pub fn store_load_filtered(
        &self,
        keep: impl Fn(u32) -> bool,
    ) -> anyhow::Result<Vec<(u32, Vec<f32>)>> {
        Ok(self
            .store
            .load()?
            .into_iter()
            .filter(|(id, _)| keep(*id))
            .collect())
    }

    /// Generate embeddings for new symbols and merge with existing kept entries.
    ///
    /// `existing` — embeddings to preserve (already filtered).
    /// `new_symbols` — `(symbol_id, text)` pairs to embed and add.
    pub fn generate_embeddings_incremental(
        &self,
        existing: &[(u32, Vec<f32>)],
        new_symbols: &[(u32, String)],
    ) -> anyhow::Result<()> {
        self.invalidate_cache();

        if new_symbols.is_empty() && existing.is_empty() {
            return self.store.clear();
        }

        let mut all_entries: Vec<(u32, Vec<f32>)> = existing.to_vec();

        if !new_symbols.is_empty() {
            let service = Self::service()?;
            for chunk in new_symbols.chunks(EMBED_BATCH_SIZE) {
                let texts: Vec<&str> = chunk.iter().map(|(_, text)| text.as_str()).collect();
                let embeddings = service
                    .embed_documents(&texts)
                    .map_err(|e| anyhow::anyhow!("Failed to generate embeddings: {e}"))?;

                for ((id, _), emb) in chunk.iter().zip(embeddings) {
                    all_entries.push((*id, emb));
                }
            }
        }

        self.store.write_all(&all_entries)?;
        tracing::info!(
            "Wrote {} embeddings to store ({} existing + {} new)",
            all_entries.len(),
            existing.len(),
            new_symbols.len()
        );
        Ok(())
    }

    /// Search for symbols similar to the given query text.
    ///
    /// Returns up to `limit` results with similarity >= `threshold`, sorted by score descending.
    /// Embeddings are cached in memory after first load.
    pub fn search(
        &self,
        query: &str,
        limit: usize,
        threshold: f32,
    ) -> anyhow::Result<Vec<SemanticMatch>> {
        // Embed the query
        let service = Self::service()?;
        let query_embedding = service
            .embed_query(query)
            .map_err(|e| anyhow::anyhow!("Failed to embed query: {e}"))?;

        // Load stored embeddings (cached after first read)
        self.ensure_cache()?;
        let cache_guard = self
            .cache
            .lock()
            .map_err(|e| anyhow::anyhow!("Cache lock poisoned: {e}"))?;
        let entries = cache_guard
            .as_ref()
            .expect("cache populated by ensure_cache");

        // Brute-force cosine similarity
        let mut scored: Vec<SemanticMatch> = entries
            .iter()
            .map(|(id, emb)| SemanticMatch {
                symbol_id: *id,
                score: cosine_similarity(&query_embedding, emb),
            })
            .filter(|m| m.score >= threshold)
            .collect();

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(limit);

        Ok(scored)
    }

    /// Invalidate the in-memory cache. Logs a warning if the mutex is poisoned
    /// (a thread panicked while holding it), but does not propagate — callers
    /// proceed to mutate the store, and the next `ensure_cache` will reload.
    fn invalidate_cache(&self) {
        match self.cache.lock() {
            Ok(mut cache) => *cache = None,
            Err(e) => tracing::warn!("Cache mutex poisoned during invalidation: {e}"),
        }
    }

    /// Ensure the embedding cache is populated from disk.
    fn ensure_cache(&self) -> anyhow::Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|e| anyhow::anyhow!("Cache lock poisoned: {e}"))?;
        if cache.is_none() {
            let entries = self.store.load()?;
            tracing::debug!("Cached {} embeddings from disk", entries.len());
            *cache = Some(entries);
        }
        Ok(())
    }

    /// Remove embeddings for the given symbol IDs.
    ///
    /// Invalidates the in-memory cache so subsequent searches reflect the removal.
    pub fn remove_embeddings(&self, ids: &HashSet<u32>) -> anyhow::Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        self.invalidate_cache();
        self.store.remove_by_ids(ids)
    }

    /// Drop every embedding whose symbol id is not in `live`, returning how many
    /// went. No embedding model is needed: this only reads and rewrites the
    /// store.
    pub fn retain_embeddings(&self, live: &HashSet<u32>) -> anyhow::Result<usize> {
        self.invalidate_cache();
        self.store.retain(|id| live.contains(&id))
    }

    /// Clear the vector store and invalidate the cache.
    pub fn clear(&self) -> anyhow::Result<()> {
        self.invalidate_cache();
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

    /// The sweep is the only thing that reads the whole store on a run that
    /// changed nothing, so what it keeps, drops, and leaves untouched is worth
    /// pinning independently of how it walks the file.
    #[test]
    fn retaining_keeps_the_survivors_byte_for_byte_and_leaves_a_full_store_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let store = VectorStore::open(&path).unwrap();

        let entries: Vec<(u32, Vec<f32>)> = (1..=5)
            .map(|i| (i, vec![i as f32 / 10.0; EMBEDDING_DIM]))
            .collect();
        store.write_all(&entries).unwrap();

        // Nothing to drop: the file must not be rewritten at all.
        let before = std::fs::read(&path).unwrap();
        assert_eq!(store.retain(|_| true).unwrap(), 0);
        assert_eq!(std::fs::read(&path).unwrap(), before, "no write was needed");

        // Dropping keeps the survivors' bytes exactly, at their new positions.
        // Comparing the bytes rather than the loaded floats is the point: the
        // sweep copies byte ranges, so a range off by one entry would still
        // load as five plausible vectors in the right order.
        assert_eq!(store.retain(|id| id % 2 == 1).unwrap(), 2);
        let after = std::fs::read(&path).unwrap();
        let entry_at = |file: &[u8], i: usize| {
            file[HEADER_SIZE + i * entry_size()..HEADER_SIZE + (i + 1) * entry_size()].to_vec()
        };
        for (new_i, old_i) in [0usize, 2, 4].into_iter().enumerate() {
            assert_eq!(
                entry_at(&after, new_i),
                entry_at(&before, old_i),
                "entry {old_i} must survive unchanged at its new place {new_i}"
            );
        }
        assert_eq!(store.count().unwrap(), 3, "the header count follows");
        assert_eq!(
            after.len(),
            HEADER_SIZE + 3 * entry_size(),
            "and nothing trails the entries it counts"
        );

        // Dropping everything leaves a valid, empty store, and sweeping that
        // empty store again is a no-op rather than an error.
        assert_eq!(store.retain(|_| false).unwrap(), 3);
        assert!(store.load().unwrap().is_empty());
        assert_eq!(store.count().unwrap(), 0);
        assert_eq!(store.retain(|_| true).unwrap(), 0);
    }

    /// A store that cannot be read holds an unknown number of vectors, not zero
    /// of them. The caller merges what this returns with what it just embedded
    /// and writes the sum back, so answering "nothing" for "unreadable" deletes
    /// every vector the failed read was holding.
    #[test]
    fn an_unreadable_store_is_not_reported_as_an_empty_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let search = SemanticSearch::new(&path).unwrap();
        search
            .store
            .write_all(&[(1, vec![0.5; EMBEDDING_DIM]), (2, vec![0.5; EMBEDDING_DIM])])
            .unwrap();

        // Cut the second entry away, leaving the header still claiming two.
        let full = std::fs::read(&path).unwrap();
        std::fs::write(&path, &full[..full.len() - entry_size()]).unwrap();

        assert!(
            search.store_load_filtered(|_| true).is_err(),
            "an unreadable store must be refused, not reported as holding nothing"
        );
    }

    /// The sweep addresses entries by offset, and the two offsets an off-by-one
    /// reaches first are the ones an interior drop never moves: the first entry
    /// and the last.
    #[test]
    fn retaining_drops_the_first_and_the_last_entry_as_readily_as_a_middle_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let store = VectorStore::open(&path).unwrap();
        let entries: Vec<(u32, Vec<f32>)> = (1..=4)
            .map(|i| (i, vec![i as f32 / 10.0; EMBEDDING_DIM]))
            .collect();
        let ids_left = || {
            store
                .load()
                .unwrap()
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        };

        store.write_all(&entries).unwrap();
        assert_eq!(store.retain(|id| id != 1).unwrap(), 1);
        assert_eq!(ids_left(), [2, 3, 4], "the first entry went");

        store.write_all(&entries).unwrap();
        assert_eq!(store.retain(|id| id != 4).unwrap(), 1);
        assert_eq!(ids_left(), [1, 2, 3], "the last entry went");
    }

    /// `retain` walks the file by offset instead of going through `load`, so the
    /// checks that make those offsets safe have to fire on its own path too: a
    /// header promising more entries than the file holds must be refused, not
    /// indexed past.
    #[test]
    fn retaining_refuses_a_file_whose_header_promises_more_than_it_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.bin");
        let store = VectorStore::open(&path).unwrap();
        store
            .write_all(&[
                (1, vec![0.5; EMBEDDING_DIM]),
                (2, vec![0.5; EMBEDDING_DIM]),
            ])
            .unwrap();

        // Cut the second entry away, leaving the header still claiming two.
        let full = std::fs::read(&path).unwrap();
        std::fs::write(&path, &full[..full.len() - entry_size()]).unwrap();

        let err = store.retain(|_| true).unwrap_err().to_string();
        assert!(err.contains("size mismatch"), "got: {err}");
        assert!(
            store.load().is_err(),
            "and the reader refuses the same file identically"
        );
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

    #[test]
    fn test_vector_store_overflow_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overflow.bin");

        // Craft a header with a huge count that would overflow when multiplied by entry_size
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&(EMBEDDING_DIM as u32).to_le_bytes());
        buf.extend_from_slice(&u32::MAX.to_le_bytes()); // count = 0xFFFFFFFF
        std::fs::write(&path, &buf).unwrap();

        let store = VectorStore::open(&path).unwrap();
        let result = store.load();
        assert!(result.is_err(), "Should fail on overflow count");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("overflow") || err.contains("mismatch"),
            "Expected overflow or mismatch error, got: {err}"
        );
    }

    #[test]
    fn test_vector_store_nan_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nan.bin");

        // Write a valid header + one entry with NaN
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&(EMBEDDING_DIM as u32).to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        buf.extend_from_slice(&1u32.to_le_bytes()); // symbol_id = 1
        // First float is NaN, rest are 0.0
        buf.extend_from_slice(&f32::NAN.to_le_bytes());
        for _ in 1..EMBEDDING_DIM {
            buf.extend_from_slice(&0.0f32.to_le_bytes());
        }
        std::fs::write(&path, &buf).unwrap();

        let store = VectorStore::open(&path).unwrap();
        let result = store.load();
        assert!(result.is_err(), "Should reject NaN values");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Non-finite"),
            "Expected non-finite error, got: {err}"
        );
    }

    #[test]
    fn test_vector_store_inf_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inf.bin");

        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&(EMBEDDING_DIM as u32).to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.extend_from_slice(&f32::INFINITY.to_le_bytes());
        for _ in 1..EMBEDDING_DIM {
            buf.extend_from_slice(&0.0f32.to_le_bytes());
        }
        std::fs::write(&path, &buf).unwrap();

        let store = VectorStore::open(&path).unwrap();
        let result = store.load();
        assert!(result.is_err(), "Should reject Inf values");
    }

    #[test]
    fn test_vector_store_atomic_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("atomic.bin")).unwrap();

        let entries = vec![(1, vec![0.5; EMBEDDING_DIM])];
        store.write_all(&entries).unwrap();

        // Verify no temp file left behind
        let tmp_path = dir.path().join(".atomic.bin.tmp");
        assert!(
            !tmp_path.exists(),
            "Temp file should be cleaned up after rename"
        );

        // Verify data is correct
        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, 1);
    }

    #[test]
    fn test_vector_store_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.bin");

        // Write a header claiming 1 entry but only provide partial data
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&(EMBEDDING_DIM as u32).to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // claims 1 entry
        buf.extend_from_slice(&1u32.to_le_bytes()); // partial: just symbol_id, no embedding
        std::fs::write(&path, &buf).unwrap();

        let store = VectorStore::open(&path).unwrap();
        let result = store.load();
        assert!(result.is_err(), "Should fail on size mismatch");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("mismatch"),
            "Expected size mismatch error, got: {err}"
        );
    }

    #[test]
    fn test_vector_store_remove_by_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("test.bin")).unwrap();

        let entries: Vec<(u32, Vec<f32>)> = vec![
            (1, vec![0.1; EMBEDDING_DIM]),
            (2, vec![0.2; EMBEDDING_DIM]),
            (3, vec![0.3; EMBEDDING_DIM]),
            (4, vec![0.4; EMBEDDING_DIM]),
        ];
        store.write_all(&entries).unwrap();
        assert_eq!(store.count().unwrap(), 4);

        let to_remove: HashSet<u32> = [2, 4].into_iter().collect();
        let removed = store.remove_by_ids(&to_remove).unwrap();
        assert_eq!(removed, 2);
        assert_eq!(store.count().unwrap(), 2);

        let remaining = store.load().unwrap();
        let ids: Vec<u32> = remaining.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 3]);
    }

    #[test]
    fn test_vector_store_remove_by_ids_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("test.bin")).unwrap();

        let entries = vec![(1, vec![0.1; EMBEDDING_DIM])];
        store.write_all(&entries).unwrap();

        let empty: HashSet<u32> = HashSet::new();
        let removed = store.remove_by_ids(&empty).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn test_vector_store_remove_by_ids_nonexistent() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("test.bin")).unwrap();

        let entries = vec![(1, vec![0.1; EMBEDDING_DIM])];
        store.write_all(&entries).unwrap();

        let to_remove: HashSet<u32> = [99].into_iter().collect();
        let removed = store.remove_by_ids(&to_remove).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn test_vector_store_remove_by_ids_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = VectorStore::open(dir.path().join("test.bin")).unwrap();

        let entries: Vec<(u32, Vec<f32>)> = vec![
            (1, vec![0.1; EMBEDDING_DIM]),
            (2, vec![0.2; EMBEDDING_DIM]),
            (3, vec![0.3; EMBEDDING_DIM]),
        ];
        store.write_all(&entries).unwrap();

        let to_remove: HashSet<u32> = [1, 2, 3].into_iter().collect();
        let removed = store.remove_by_ids(&to_remove).unwrap();
        assert_eq!(removed, 3);
        assert_eq!(store.count().unwrap(), 0);
        assert!(store.load().unwrap().is_empty());
    }

    // --- SemanticSearch-level tests (no model needed) ---

    #[test]
    fn test_semantic_search_remove_embeddings_clears_cache() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("vectors.bin");

        // Create a SemanticSearch and manually populate the store
        let search = SemanticSearch::new(&store_path).unwrap();
        let entries: Vec<(u32, Vec<f32>)> = vec![
            (1, vec![0.5; EMBEDDING_DIM]),
            (2, vec![0.6; EMBEDDING_DIM]),
            (3, vec![0.7; EMBEDDING_DIM]),
        ];
        search.store.write_all(&entries).unwrap();
        assert_eq!(search.count().unwrap(), 3);

        // Warm the cache by calling ensure_cache
        search.ensure_cache().unwrap();

        // Remove some entries
        let to_remove: HashSet<u32> = [1, 3].into_iter().collect();
        let removed = search.remove_embeddings(&to_remove).unwrap();
        assert_eq!(removed, 2);

        // Count should reflect removal (reads from store, not stale cache)
        assert_eq!(search.count().unwrap(), 1);
    }

    #[test]
    fn test_semantic_search_remove_embeddings_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let search = SemanticSearch::new(dir.path().join("vectors.bin")).unwrap();
        search
            .store
            .write_all(&[(1, vec![0.5; EMBEDDING_DIM])])
            .unwrap();

        let empty: HashSet<u32> = HashSet::new();
        let removed = search.remove_embeddings(&empty).unwrap();
        assert_eq!(removed, 0);
        assert_eq!(search.count().unwrap(), 1);
    }

    // --- Cosine similarity tests ---

    #[test]
    fn test_cosine_similarity_identical() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![1.0, 2.0, 3.0];
        let sim = cosine_similarity(&a, &b);
        assert!(
            (sim - 1.0).abs() < 1e-5,
            "Identical vectors should have similarity ~1.0, got {sim}"
        );
    }

    #[test]
    fn test_cosine_similarity_orthogonal() {
        let a = vec![1.0, 0.0, 0.0];
        let b = vec![0.0, 1.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(
            sim.abs() < 1e-5,
            "Orthogonal vectors should have similarity ~0.0, got {sim}"
        );
    }

    #[test]
    fn test_cosine_similarity_opposite() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![-1.0, -2.0, -3.0];
        let sim = cosine_similarity(&a, &b);
        assert!(
            (sim - (-1.0)).abs() < 1e-5,
            "Opposite vectors should have similarity ~-1.0, got {sim}"
        );
    }

    #[test]
    fn test_cosine_similarity_zero_vector() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![0.0, 0.0, 0.0];
        let sim = cosine_similarity(&a, &b);
        assert!(
            sim.abs() < 1e-5,
            "Zero vector should have similarity 0.0, got {sim}"
        );
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
        assert_eq!(
            text,
            "Method connect\nfn connect(&self, url: &str) -> Connection"
        );
    }

    #[test]
    fn test_format_symbol_text_truncation() {
        let long_doc = "x".repeat(600);
        let text = format_symbol_text(SymbolKind::Function, "f", None, Some(&long_doc));
        assert!(text.len() <= MAX_EMBED_TEXT_LEN);
    }

    #[test]
    fn test_format_symbol_text_truncation_multibyte() {
        // 'é' is 2 bytes in UTF-8. Fill with enough to exceed MAX_EMBED_TEXT_LEN.
        let long_doc = "é".repeat(300); // 600 bytes, 300 chars
        let text = format_symbol_text(SymbolKind::Function, "f", None, Some(&long_doc));
        assert!(text.len() <= MAX_EMBED_TEXT_LEN);
        assert!(
            text.is_char_boundary(text.len()),
            "Must truncate at a char boundary"
        );
    }

    // --- Integration tests (require model download) ---

    #[test]
    #[ignore = "requires ONNX model download"]
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
    #[ignore = "requires ONNX model download"]
    fn test_semantic_search_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let search = SemanticSearch::new(dir.path().join("vectors.bin")).unwrap();

        let symbols = vec![(
            1,
            "Function hello\nfn hello()\nPrints a greeting.".to_string(),
        )];

        search.generate_embeddings(&symbols).unwrap();

        // High threshold should filter out low-similarity results
        let results = search.search("quantum physics equations", 10, 0.9).unwrap();
        assert!(
            results.is_empty(),
            "Unrelated query with high threshold should return no results"
        );
    }
}
