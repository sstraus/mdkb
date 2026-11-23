//! One bounded ONNX Runtime thread pool for every model in the process.
//!
//! fastembed builds each session with `with_intra_threads(available_parallelism())`
//! and exposes no option for it at the pinned version. Measured 2026-11-12 on 14
//! cores: the recall reranker held 9.2-11.1 cores per run and `mdkb update` 7.4.
//! ONNX Runtime has a process-global pool for exactly this: once the environment
//! carries one, `ort` disables the per-session pools of every session committed
//! after it, so fastembed's per-core setting no longer creates threads.

use std::sync::Once;

/// Intra-op threads every ONNX session in the process shares.
pub const DEFAULT_ORT_THREADS: usize = 2;

/// Overrides [`DEFAULT_ORT_THREADS`]. `0` keeps ONNX Runtime's own choice: one
/// pool per session, one thread per core.
pub const ORT_THREADS_ENV: &str = "MDKB_ORT_THREADS";

/// Intra-op threads for a full regeneration child: half the cores, at least
/// [`DEFAULT_ORT_THREADS`]. Measured 2026-11-21, Granite re-embed of 3,949
/// vectors on 14 cores: 2 threads 278 s / 550 CPU-s, 4 148 s / 576, 8 98 s / 730,
/// unbounded (`0`) 131 s / 1,438. An unbounded pool is slower and burns 2.6x
/// the CPU of 8 threads.
pub fn regeneration_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(DEFAULT_ORT_THREADS, |n| n.get() / 2)
        .max(DEFAULT_ORT_THREADS)
}

/// The bound for an [`ORT_THREADS_ENV`] value; `None` means no bound.
fn threads_from(value: Option<&str>) -> Option<usize> {
    let Some(raw) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Some(DEFAULT_ORT_THREADS);
    };
    match raw.parse::<usize>() {
        Ok(0) => None,
        Ok(n) => Some(n),
        Err(_) => {
            tracing::warn!(
                "{ORT_THREADS_ENV}={raw:?} is not a thread count; using {DEFAULT_ORT_THREADS}"
            );
            Some(DEFAULT_ORT_THREADS)
        }
    }
}

/// Give the process one ONNX Runtime pool of [`DEFAULT_ORT_THREADS`] intra-op
/// threads (or [`ORT_THREADS_ENV`]). Call it before any model session is
/// built: a session committed earlier keeps the pool it was built with.
///
/// Runs once per process. A failure leaves ONNX Runtime's default in place and
/// is logged: an unbounded model is slower for its neighbours, not wrong.
pub fn bound_onnx_threads() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Some(threads) = threads_from(std::env::var(ORT_THREADS_ENV).ok().as_deref()) else {
            return;
        };
        let committed = ort::environment::GlobalThreadPoolOptions::default()
            .with_intra_threads(threads)
            .and_then(|pool| pool.with_inter_threads(1))
            .and_then(|pool| ort::init().with_global_thread_pool(pool).commit());
        if let Err(e) = committed {
            tracing::warn!("could not bound ONNX Runtime to {threads} threads: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threads_from_reads_the_override_and_falls_back_to_the_default() {
        // Catches: an unset or mistyped value that removes the bound instead
        // of keeping the default, or a 0 that is not the documented opt-out.
        assert_eq!(threads_from(None), Some(DEFAULT_ORT_THREADS));
        assert_eq!(threads_from(Some("")), Some(DEFAULT_ORT_THREADS));
        assert_eq!(threads_from(Some("four")), Some(DEFAULT_ORT_THREADS));
        assert_eq!(threads_from(Some(" 4 ")), Some(4));
        assert_eq!(threads_from(Some("0")), None);
    }
}
