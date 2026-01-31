//! Usage tracking and statistics for MCP tool calls.
//!
//! Records metrics for each tool invocation to optimize token efficiency.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Statistics for a single tool type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolUsage {
    /// Number of times this tool was called.
    pub call_count: u64,
    /// Total tokens returned across all calls.
    pub total_tokens: u64,
    /// Total documents/results returned.
    pub total_results: u64,
    /// Average tokens per call.
    pub avg_tokens_per_call: f64,
    /// Average tokens per result.
    pub avg_tokens_per_result: f64,
}

impl ToolUsage {
    /// Record a tool call.
    pub fn record(&mut self, tokens: usize, results: usize) {
        self.call_count += 1;
        self.total_tokens += tokens as u64;
        self.total_results += results as u64;

        // Update averages
        self.avg_tokens_per_call = self.total_tokens as f64 / self.call_count as f64;
        if self.total_results > 0 {
            self.avg_tokens_per_result = self.total_tokens as f64 / self.total_results as f64;
        }
    }
}

/// Global usage metrics tracker.
#[derive(Debug, Default)]
pub struct UsageMetrics {
    /// Search tool usage (mdkb_search - hybrid search).
    pub search: Mutex<ToolUsage>,
    /// Document retrieval usage (mdkb_get).
    pub get: Mutex<ToolUsage>,
    /// Batch retrieval usage (mdkb_multi_get).
    pub multi_get: Mutex<ToolUsage>,
    /// Status check usage (mdkb_status).
    pub status: Mutex<ToolUsage>,
    /// Update usage (mdkb_update).
    pub update: Mutex<ToolUsage>,

    /// Total tokens delivered across all tools.
    total_tokens: AtomicU64,
    /// Total tool calls.
    total_calls: AtomicU64,
}

impl UsageMetrics {
    /// Create a new usage metrics tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a search tool call (hybrid search).
    pub fn record_search(&self, tokens: usize, results: usize) {
        if let Ok(mut usage) = self.search.lock() {
            usage.record(tokens, results);
        }
        self.total_tokens.fetch_add(tokens as u64, Ordering::Relaxed);
        self.total_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a get tool call.
    pub fn record_get(&self, tokens: usize) {
        if let Ok(mut usage) = self.get.lock() {
            usage.record(tokens, 1);
        }
        self.total_tokens.fetch_add(tokens as u64, Ordering::Relaxed);
        self.total_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a multi_get tool call.
    pub fn record_multi_get(&self, tokens: usize, results: usize) {
        if let Ok(mut usage) = self.multi_get.lock() {
            usage.record(tokens, results);
        }
        self.total_tokens.fetch_add(tokens as u64, Ordering::Relaxed);
        self.total_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a status tool call.
    pub fn record_status(&self, tokens: usize) {
        if let Ok(mut usage) = self.status.lock() {
            usage.record(tokens, 1);
        }
        self.total_tokens.fetch_add(tokens as u64, Ordering::Relaxed);
        self.total_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Record an update tool call.
    pub fn record_update(&self, tokens: usize) {
        if let Ok(mut usage) = self.update.lock() {
            usage.record(tokens, 1);
        }
        self.total_tokens.fetch_add(tokens as u64, Ordering::Relaxed);
        self.total_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Get total tokens delivered.
    pub fn total_tokens(&self) -> u64 {
        self.total_tokens.load(Ordering::Relaxed)
    }

    /// Get total tool calls.
    pub fn total_calls(&self) -> u64 {
        self.total_calls.load(Ordering::Relaxed)
    }

    /// Get average tokens per call across all tools.
    pub fn avg_tokens_per_call(&self) -> f64 {
        let calls = self.total_calls();
        if calls == 0 {
            0.0
        } else {
            self.total_tokens() as f64 / calls as f64
        }
    }

    /// Get a summary of all usage.
    pub fn summary(&self) -> UsageSummary {
        UsageSummary {
            total_calls: self.total_calls(),
            total_tokens: self.total_tokens(),
            avg_tokens_per_call: self.avg_tokens_per_call(),
            search: self.search.lock().map(|u| u.clone()).unwrap_or_default(),
            get: self.get.lock().map(|u| u.clone()).unwrap_or_default(),
            multi_get: self.multi_get.lock().map(|u| u.clone()).unwrap_or_default(),
            status: self.status.lock().map(|u| u.clone()).unwrap_or_default(),
            update: self.update.lock().map(|u| u.clone()).unwrap_or_default(),
        }
    }
}

/// Summary of usage metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub total_calls: u64,
    pub total_tokens: u64,
    pub avg_tokens_per_call: f64,
    pub search: ToolUsage,
    pub get: ToolUsage,
    pub multi_get: ToolUsage,
    pub status: ToolUsage,
    pub update: ToolUsage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_usage_record() {
        let mut usage = ToolUsage::default();
        usage.record(100, 5);

        assert_eq!(usage.call_count, 1);
        assert_eq!(usage.total_tokens, 100);
        assert_eq!(usage.total_results, 5);
        assert!((usage.avg_tokens_per_call - 100.0).abs() < 0.001);
        assert!((usage.avg_tokens_per_result - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_tool_usage_multiple_records() {
        let mut usage = ToolUsage::default();
        usage.record(100, 5);
        usage.record(200, 10);

        assert_eq!(usage.call_count, 2);
        assert_eq!(usage.total_tokens, 300);
        assert_eq!(usage.total_results, 15);
        assert!((usage.avg_tokens_per_call - 150.0).abs() < 0.001);
        assert!((usage.avg_tokens_per_result - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_usage_metrics_record() {
        let metrics = UsageMetrics::new();
        metrics.record_search(100, 5);
        metrics.record_get(500);

        assert_eq!(metrics.total_calls(), 2);
        assert_eq!(metrics.total_tokens(), 600);
    }

    #[test]
    fn test_usage_metrics_summary() {
        let metrics = UsageMetrics::new();
        metrics.record_search(100, 5);
        metrics.record_search(200, 10);
        metrics.record_get(500);

        let summary = metrics.summary();
        assert_eq!(summary.total_calls, 3);
        assert_eq!(summary.total_tokens, 800);
        assert_eq!(summary.search.call_count, 2);
        assert_eq!(summary.get.call_count, 1);
    }

    #[test]
    fn test_usage_metrics_thread_safe() {
        use std::sync::Arc;
        use std::thread;

        let metrics = Arc::new(UsageMetrics::new());
        let mut handles = vec![];

        for _ in 0..10 {
            let m = Arc::clone(&metrics);
            handles.push(thread::spawn(move || {
                for _ in 0..100 {
                    m.record_search(10, 1);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(metrics.total_calls(), 1000);
        assert_eq!(metrics.total_tokens(), 10000);
    }
}
