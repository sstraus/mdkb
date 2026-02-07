//! Content hashing and file metadata for change detection.
//!
//! SHA-256 hashes are used to detect file content changes for
//! incremental indexing without re-reading the full file.

use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Compute a SHA-256 hash of file content, returned as a hex string.
pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Get a file's modification time as seconds since the Unix epoch.
///
/// Returns `None` if the file doesn't exist or metadata can't be read.
pub fn file_mtime(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Get the current UTC timestamp as seconds since the Unix epoch.
pub fn utc_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_deterministic() {
        let h1 = content_hash("hello world");
        let h2 = content_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_content_hash_differs_for_different_content() {
        let h1 = content_hash("hello");
        let h2 = content_hash("world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_content_hash_is_hex_sha256() {
        let h = content_hash("hello world");
        // SHA-256 produces 64 hex characters
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_content_hash_known_value() {
        // SHA-256 of empty string is well-known
        let h = content_hash("");
        assert_eq!(
            h,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_file_mtime_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, "content").unwrap();

        let mtime = file_mtime(&path);
        assert!(mtime.is_some());
        assert!(mtime.unwrap() > 0);
    }

    #[test]
    fn test_file_mtime_nonexistent() {
        let mtime = file_mtime(Path::new("/nonexistent/file.txt"));
        assert!(mtime.is_none());
    }

    #[test]
    fn test_utc_timestamp_is_reasonable() {
        let ts = utc_timestamp();
        // Should be after 2024-01-01 (1704067200)
        assert!(ts > 1_704_067_200);
    }
}
