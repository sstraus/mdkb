//! File system watcher for auto-reindexing.
//!
//! Watches collection paths and triggers reindexing on file changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{Debouncer, new_debouncer};
use tokio::sync::mpsc;

use crate::error::{ErrorKind, Result};

/// Minimum gap between "dropped events" warnings, so a burst that overflows the
/// channel logs once rather than per dropped path.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(5);

/// File change event.
#[derive(Debug, Clone)]
pub struct FileChange {
    /// Path that changed.
    pub path: PathBuf,
    /// Type of change.
    pub kind: ChangeKind,
}

/// Kind of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// File created or modified.
    CreateOrModify,
    /// File removed.
    Remove,
}

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Debounce duration in milliseconds.
    pub debounce_ms: u64,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self { debounce_ms: 100 }
    }
}

/// File watcher that monitors paths and emits change events.
pub struct FileWatcher {
    /// The underlying debounced watcher.
    _debouncer: Debouncer<RecommendedWatcher>,
    /// Channel receiver for events.
    receiver: mpsc::Receiver<FileChange>,
    /// Set when the debouncer had to drop an event because the channel was full
    /// (a burst larger than the buffer while the consumer was mid-flush). The
    /// consumer reads this via [`FileWatcher::take_missed_events`] and forces a
    /// full rescan so silently-dropped changes are not lost.
    missed_events: Arc<AtomicBool>,
}

impl std::fmt::Debug for FileWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWatcher").finish_non_exhaustive()
    }
}

impl FileWatcher {
    /// Create a new file watcher.
    pub fn new(config: WatcherConfig) -> Result<Self> {
        let (tx, rx) = mpsc::channel(100);
        let missed_events = Arc::new(AtomicBool::new(false));

        let missed_for_closure = Arc::clone(&missed_events);
        let mut last_warn: Option<std::time::Instant> = None;
        let debouncer = new_debouncer(
            Duration::from_millis(config.debounce_ms),
            move |result: std::result::Result<Vec<notify_debouncer_mini::DebouncedEvent>, _>| {
                if let Ok(events) = result {
                    for event in events {
                        let change = FileChange {
                            path: event.path,
                            kind: ChangeKind::CreateOrModify,
                        };

                        // Non-blocking send from a non-tokio thread. On a full
                        // channel (burst > buffer while the consumer flushes) the
                        // event would be silently lost, leaving files stale; flag
                        // it so the consumer forces a full rescan, and warn (rate-
                        // limited) so the drop is visible.
                        if tx.try_send(change).is_err() {
                            missed_for_closure.store(true, Ordering::Release);
                            let now = std::time::Instant::now();
                            if last_warn.is_none_or(|t| now.duration_since(t) >= DROP_WARN_INTERVAL)
                            {
                                tracing::warn!(
                                    "File watcher channel full — dropped change event(s); \
                                     scheduling a full rescan to recover. (Rate-limited warning.)"
                                );
                                last_warn = Some(now);
                            }
                        }
                    }
                }
            },
        )
        .map_err(|e| ErrorKind::Watcher(format!("Failed to create watcher: {e}")))?;

        Ok(Self {
            _debouncer: debouncer,
            receiver: rx,
            missed_events,
        })
    }

    /// Returns and clears the "dropped events" flag. When true, the consumer
    /// must run a full rescan because at least one change was dropped by
    /// backpressure since the last check.
    pub fn take_missed_events(&self) -> bool {
        self.missed_events.swap(false, Ordering::AcqRel)
    }

    /// Watch a path for changes.
    pub fn watch(&mut self, path: &PathBuf) -> Result<()> {
        self._debouncer
            .watcher()
            .watch(path.as_ref(), RecursiveMode::Recursive)
            .map_err(|e| ErrorKind::Watcher(format!("Failed to watch {}: {e}", path.display())))?;
        Ok(())
    }

    /// Stop watching a path.
    pub fn unwatch(&mut self, path: &PathBuf) -> Result<()> {
        self._debouncer
            .watcher()
            .unwatch(path.as_ref())
            .map_err(|e| {
                ErrorKind::Watcher(format!("Failed to unwatch {}: {e}", path.display()))
            })?;
        Ok(())
    }

    /// Receive the next file change event.
    pub async fn recv(&mut self) -> Option<FileChange> {
        self.receiver.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn setup_temp_dir() -> TempDir {
        // On macOS, /tmp is a symlink to /private/tmp. FSEvents reports canonical paths,
        // so the watcher must use a canonical (non-symlinked) base dir.
        let base = std::env::temp_dir()
            .canonicalize()
            .unwrap_or_else(|_| std::env::temp_dir());
        tempfile::tempdir_in(base).expect("failed to create temp dir")
    }

    #[tokio::test]
    async fn missed_events_flag_defaults_false_and_clears_on_take() {
        // BUG-F1 flag contract: no drops on a fresh watcher; take_missed_events
        // reports-and-clears so the consumer only rescans once per drop episode.
        let config = WatcherConfig { debounce_ms: 50 };
        let watcher = FileWatcher::new(config).expect("watcher creation should succeed");
        assert!(!watcher.take_missed_events(), "no drops on a fresh watcher");

        // Simulate a recorded drop, then confirm take reports true once and clears.
        watcher.missed_events.store(true, Ordering::Release);
        assert!(watcher.take_missed_events(), "first take sees the drop");
        assert!(
            !watcher.take_missed_events(),
            "second take is false — the flag is cleared, so we rescan once not repeatedly"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_watcher_detects_file_creation() {
        let temp = setup_temp_dir();
        let config = WatcherConfig { debounce_ms: 50 };
        let mut watcher = FileWatcher::new(config).expect("watcher creation should succeed");

        watcher
            .watch(&temp.path().to_path_buf())
            .expect("watch should succeed");

        // FSEvents on macOS registers watches asynchronously
        tokio::time::sleep(Duration::from_millis(300)).await;

        let file_path = temp.path().join("test.md");
        fs::write(&file_path, "# Test").expect("write should succeed");

        let result = timeout(Duration::from_secs(5), watcher.recv()).await;
        assert!(result.is_ok(), "Should receive event within timeout");
        let event = result.unwrap();
        assert!(event.is_some(), "Should receive a file change event");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_watcher_detects_file_modification() {
        let temp = setup_temp_dir();
        let config = WatcherConfig { debounce_ms: 50 };
        let mut watcher = FileWatcher::new(config).expect("watcher creation should succeed");

        let file_path = temp.path().join("test.md");
        fs::write(&file_path, "# Original").expect("write should succeed");

        tokio::time::sleep(Duration::from_millis(200)).await;

        watcher
            .watch(&temp.path().to_path_buf())
            .expect("watch should succeed");

        // FSEvents on macOS registers watches asynchronously
        tokio::time::sleep(Duration::from_millis(300)).await;

        fs::write(&file_path, "# Modified").expect("write should succeed");

        let result = timeout(Duration::from_secs(5), watcher.recv()).await;
        assert!(result.is_ok(), "Should receive event within timeout");
    }

    #[test]
    fn test_watcher_config_default() {
        let config = WatcherConfig::default();
        assert_eq!(config.debounce_ms, 100);
    }
}
