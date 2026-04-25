//! Shared helpers for CLI integration tests.
//!
//! The setup tests mutate `$HOME` globally so cargo's parallel execution must
//! be serialized per-binary to avoid cross-test interference.

use std::sync::{Mutex, MutexGuard, OnceLock};

pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
