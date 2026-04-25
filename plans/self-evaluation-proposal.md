# mdkb Self-Evaluation System Proposal

## Executive Summary

This proposal outlines a self-evaluation framework for mdkb that enables continuous quality assessment of search and indexing operations without requiring external services or user feedback infrastructure. The system focuses on **passive metrics** that can be collected automatically, with optional **explicit feedback** integration for MCP clients.

---

## 1. Metrics Classification

### 1.1 Passive Metrics (No User Feedback Required)

These metrics can be collected automatically during normal operation:

| Category | Metric | Description | Collection Method |
|----------|--------|-------------|-------------------|
| **Latency** | query_latency_ms | Time to execute search query | Timestamp before/after |
| **Latency** | embedding_latency_ms | Time to generate embeddings | Timestamp before/after |
| **Latency** | indexing_latency_ms | Time to index single document | Timestamp before/after |
| **Throughput** | docs_indexed_per_sec | Indexing throughput | Counter / time window |
| **Throughput** | queries_per_min | Query throughput | Counter / time window |
| **Index Health** | index_size_bytes | Total FTS5 + vec index size | sqlite file stats |
| **Index Health** | doc_count | Total indexed documents | COUNT(*) |
| **Index Health** | avg_doc_tokens | Average tokens per document | Stored during indexing |
| **Index Health** | stale_doc_count | Docs needing reindex (mtime changed) | mtime comparison |
| **Search Quality** | zero_result_rate | Queries returning no results | Counter / query count |
| **Search Quality** | avg_result_count | Average results per query | SUM / query count |
| **Search Quality** | avg_bm25_score | Average top-k BM25 score | Aggregate at query time |
| **Search Quality** | score_distribution | p25/p50/p75/p95 of top result scores | Histogram |
| **Embedding Quality** | vec_similarity_distribution | Distribution of cosine similarities in results | Histogram |
| **Embedding Quality** | vec_recall_estimate | Estimated recall vs exhaustive search (sampled) | Periodic sampling |

### 1.2 Session-Derived Metrics (Inferred from Usage Patterns)

These require session tracking but no explicit user action:

| Metric | Description | Signal Meaning |
|--------|-------------|----------------|
| **re_search_rate** | Same/similar query within N seconds | Poor initial results |
| **query_refinement_rate** | Modified query (added/removed terms) | Partial success, needs tuning |
| **multi_search_session** | Multiple searches before get/read | Difficulty finding target |
| **search_to_get_ratio** | Searches per document retrieval | Lower = more efficient |
| **abandoned_search_rate** | Search with no subsequent get | Complete failure |

### 1.3 Explicit Feedback Metrics (Requires Client Cooperation)

These require the MCP client or CLI user to provide feedback:

| Metric | Description | Collection Method |
|--------|-------------|-------------------|
| **result_used** | Which result positions were actually read | Client reports usage |
| **thumbs_up/down** | Explicit relevance feedback | Client UI integration |
| **time_to_first_useful** | Time from query to finding useful doc | Client timestamps |
| **result_position_used** | Position of clicked/used result | Client reports position |

---

## 2. Storage Schema

### 2.1 Metrics Tables

```sql
-- Time-series metrics storage (append-only)
CREATE TABLE IF NOT EXISTS metrics_timeseries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,           -- Unix epoch milliseconds
    metric_name TEXT NOT NULL,     -- e.g., 'query_latency_ms'
    metric_type TEXT NOT NULL,     -- 'gauge', 'counter', 'histogram'
    value REAL NOT NULL,
    dimensions TEXT,               -- JSON: {"collection": "docs", "query_type": "search"}
    created_at INTEGER DEFAULT (strftime('%s', 'now') * 1000)
);

CREATE INDEX idx_metrics_ts ON metrics_timeseries(metric_name, ts);
CREATE INDEX idx_metrics_dims ON metrics_timeseries(metric_name, dimensions);

-- Query event log (for session analysis)
CREATE TABLE IF NOT EXISTS query_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    session_id TEXT NOT NULL,      -- UUID or client-provided
    event_type TEXT NOT NULL,      -- 'search', 'vsearch', 'query', 'get', 'mget'
    query_text TEXT,               -- For search events
    doc_id TEXT,                   -- For get events
    result_count INTEGER,
    latency_ms REAL,
    top_score REAL,                -- Top result score (BM25 or cosine)
    metadata TEXT                  -- JSON for extensibility
);

CREATE INDEX idx_query_session ON query_events(session_id, ts);
CREATE INDEX idx_query_type ON query_events(event_type, ts);

-- Aggregated rollups (hourly/daily summaries)
CREATE TABLE IF NOT EXISTS metrics_rollups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    period_start INTEGER NOT NULL,  -- Start of period (epoch ms)
    period_type TEXT NOT NULL,      -- 'hour', 'day', 'week'
    metric_name TEXT NOT NULL,
    count INTEGER NOT NULL,
    sum REAL NOT NULL,
    min REAL NOT NULL,
    max REAL NOT NULL,
    p50 REAL,                       -- Stored percentiles
    p95 REAL,
    p99 REAL,
    dimensions TEXT
);

CREATE UNIQUE INDEX idx_rollups_unique ON metrics_rollups(period_start, period_type, metric_name, dimensions);

-- Feedback events (from clients)
CREATE TABLE IF NOT EXISTS feedback_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts INTEGER NOT NULL,
    session_id TEXT,
    query_id INTEGER REFERENCES query_events(id),
    feedback_type TEXT NOT NULL,   -- 'used', 'thumbs_up', 'thumbs_down', 'skip'
    result_position INTEGER,
    doc_id TEXT,
    metadata TEXT
);
```

### 2.2 Configuration Schema

```toml
# .mdkb/config.toml additions

[metrics]
enabled = true
retention_days = 90              # Raw data retention
rollup_enabled = true            # Enable hourly/daily aggregation
session_timeout_secs = 300       # Session boundary detection

[metrics.passive]
latency = true                   # Track latency metrics
throughput = true                # Track throughput metrics
index_health = true              # Track index size/health
search_quality = true            # Track zero-result rates, score distributions

[metrics.session]
enabled = true                   # Enable session-based analysis
re_search_window_secs = 30       # Window for detecting re-searches

[metrics.embedding]
recall_sampling_rate = 0.01      # Sample 1% of queries for recall estimation
```

---

## 3. Metrics Collection Architecture

### 3.1 Collection Points

```
┌──────────────────────────────────────────────────────────────────┐
│                         mdkb Core                                 │
│                                                                   │
│  ┌──────────┐    ┌──────────┐    ┌──────────┐    ┌──────────┐   │
│  │   CLI    │    │   MCP    │    │  Store   │    │   LLM    │   │
│  │  Layer   │    │  Server  │    │  Layer   │    │  Layer   │   │
│  └────┬─────┘    └────┬─────┘    └────┬─────┘    └────┬─────┘   │
│       │               │               │               │          │
│       └───────────────┴───────────────┴───────────────┘          │
│                              │                                    │
│                    ┌─────────▼─────────┐                         │
│                    │  MetricsCollector │                         │
│                    │    (in-memory)    │                         │
│                    └─────────┬─────────┘                         │
│                              │                                    │
│                    ┌─────────▼─────────┐                         │
│                    │   MetricsStore    │                         │
│                    │    (SQLite)       │                         │
│                    └───────────────────┘                         │
└──────────────────────────────────────────────────────────────────┘
```

### 3.2 Collection Strategy

```rust
// Minimal-overhead collection approach
pub struct MetricsCollector {
    buffer: RwLock<Vec<MetricEvent>>,  // In-memory buffer
    flush_interval: Duration,           // Default: 10 seconds
    batch_size: usize,                  // Default: 100 events
}

impl MetricsCollector {
    /// Record a latency measurement
    pub fn record_latency(&self, name: &str, latency_ms: f64, dimensions: Option<&Dimensions>) {
        self.buffer.write().push(MetricEvent::Gauge {
            ts: now_ms(),
            name: name.to_string(),
            value: latency_ms,
            dimensions: dimensions.cloned(),
        });
    }

    /// Auto-flush when buffer reaches batch_size or interval elapsed
    async fn flush(&self, store: &MetricsStore) -> Result<()> {
        let events = std::mem::take(&mut *self.buffer.write());
        store.insert_batch(&events).await
    }
}
```

---

## 4. Quality Metrics Computation

### 4.1 Zero-Result Rate

```sql
-- Current zero-result rate (last 24 hours)
SELECT
    COUNT(*) FILTER (WHERE result_count = 0) * 100.0 / COUNT(*) as zero_result_pct
FROM query_events
WHERE event_type IN ('search', 'vsearch', 'query')
  AND ts > (strftime('%s', 'now') - 86400) * 1000;
```

**Interpretation:**
- < 5%: Excellent coverage
- 5-15%: Normal for specialized corpora
- > 15%: Index may be missing content or queries are out of domain

### 4.2 Re-Search Detection

```sql
-- Detect re-search patterns within sessions
WITH session_queries AS (
    SELECT
        session_id,
        ts,
        query_text,
        LAG(query_text) OVER (PARTITION BY session_id ORDER BY ts) as prev_query,
        LAG(ts) OVER (PARTITION BY session_id ORDER BY ts) as prev_ts
    FROM query_events
    WHERE event_type = 'search'
)
SELECT
    COUNT(*) FILTER (WHERE ts - prev_ts < 30000 AND similarity(query_text, prev_query) > 0.8)
        * 100.0 / COUNT(*) as re_search_pct
FROM session_queries
WHERE prev_query IS NOT NULL;
```

**Note:** `similarity()` would need to be implemented as a custom SQLite function using edit distance or token overlap.

### 4.3 Latency Percentiles

```sql
-- Compute latency percentiles from raw data
SELECT
    metric_name,
    COUNT(*) as sample_count,
    AVG(value) as avg_ms,
    -- Approximate percentiles using subqueries
    (SELECT value FROM metrics_timeseries m2
     WHERE m2.metric_name = m1.metric_name
       AND m2.ts > (strftime('%s', 'now') - 3600) * 1000
     ORDER BY value LIMIT 1 OFFSET (COUNT(*) * 0.50)) as p50,
    (SELECT value FROM metrics_timeseries m2
     WHERE m2.metric_name = m1.metric_name
       AND m2.ts > (strftime('%s', 'now') - 3600) * 1000
     ORDER BY value LIMIT 1 OFFSET (COUNT(*) * 0.95)) as p95,
    (SELECT value FROM metrics_timeseries m2
     WHERE m2.metric_name = m1.metric_name
       AND m2.ts > (strftime('%s', 'now') - 3600) * 1000
     ORDER BY value LIMIT 1 OFFSET (COUNT(*) * 0.99)) as p99
FROM metrics_timeseries m1
WHERE metric_name = 'query_latency_ms'
  AND ts > (strftime('%s', 'now') - 3600) * 1000
GROUP BY metric_name;
```

### 4.4 Vector Recall Estimation

For approximate nearest neighbor (ANN) search quality:

```rust
/// Sample-based recall estimation
/// Periodically run exhaustive search on sampled queries and compare to ANN results
pub fn estimate_recall(
    store: &Store,
    sample_rate: f64,  // e.g., 0.01 for 1%
    k: usize,          // e.g., 10 for recall@10
) -> Result<f64> {
    let sample_queries = store.sample_recent_vsearch_queries(sample_rate)?;

    let mut total_recall = 0.0;
    for query in &sample_queries {
        let ann_results = store.vsearch(&query.embedding, k, /*use_ann=*/true)?;
        let exact_results = store.vsearch(&query.embedding, k, /*use_ann=*/false)?;

        let ann_ids: HashSet<_> = ann_results.iter().map(|r| &r.doc_id).collect();
        let exact_ids: HashSet<_> = exact_results.iter().map(|r| &r.doc_id).collect();

        let intersection = ann_ids.intersection(&exact_ids).count();
        total_recall += intersection as f64 / k as f64;
    }

    Ok(total_recall / sample_queries.len() as f64)
}
```

---

## 5. A/B Testing Infrastructure

### 5.1 Experiment Configuration

```toml
# .mdkb/experiments.toml

[experiment.chunking_strategy]
enabled = true
start_date = 2026-01-15
variants = ["paragraph", "sliding_window_256", "sliding_window_512"]
traffic_split = [0.33, 0.33, 0.34]
primary_metric = "search_to_get_ratio"  # Lower is better
secondary_metrics = ["query_latency_ms", "avg_result_count"]

[experiment.quantization_level]
enabled = false
variants = ["q4_k_m", "q5_k_m", "q8_0"]
traffic_split = [0.33, 0.33, 0.34]
primary_metric = "vec_recall_estimate"
```

### 5.2 Variant Assignment

```rust
pub struct ExperimentManager {
    experiments: HashMap<String, Experiment>,
}

impl ExperimentManager {
    /// Deterministic assignment based on session_id hash
    pub fn get_variant(&self, experiment_name: &str, session_id: &str) -> Option<String> {
        let experiment = self.experiments.get(experiment_name)?;
        if !experiment.enabled {
            return None;
        }

        // Deterministic hash for consistent assignment
        let hash = seahash::hash(format!("{}:{}", experiment_name, session_id).as_bytes());
        let bucket = (hash % 100) as f64 / 100.0;

        let mut cumulative = 0.0;
        for (variant, weight) in experiment.variants.iter().zip(&experiment.traffic_split) {
            cumulative += weight;
            if bucket < cumulative {
                return Some(variant.clone());
            }
        }
        None
    }
}
```

### 5.3 Experiment Analysis

```sql
-- Compare metrics across variants
SELECT
    qe.metadata->>'$.experiment.chunking_strategy' as variant,
    COUNT(*) as query_count,
    AVG(latency_ms) as avg_latency,
    AVG(result_count) as avg_results,
    COUNT(*) FILTER (WHERE result_count = 0) * 100.0 / COUNT(*) as zero_result_pct
FROM query_events qe
WHERE qe.ts > (strftime('%s', 'now') - 604800) * 1000  -- Last 7 days
  AND qe.metadata->>'$.experiment.chunking_strategy' IS NOT NULL
GROUP BY variant;
```

---

## 6. Optimization Triggers

### 6.1 Automatic Actions Based on Metrics

| Trigger Condition | Automatic Action |
|-------------------|------------------|
| `zero_result_rate > 20%` | Log warning, suggest index update |
| `query_latency_p99 > 500ms` | Run FTS5 OPTIMIZE |
| `stale_doc_count > 100` | Trigger incremental reindex |
| `vec_recall_estimate < 0.90` | Suggest HNSW parameter tuning |
| `index_size_bytes > 1GB` | Suggest rollup/archival |

### 6.2 Alert Configuration

```toml
# .mdkb/config.toml

[alerts]
enabled = true

[[alerts.rules]]
name = "high_zero_result_rate"
condition = "zero_result_rate > 0.20"
window = "1h"
action = "log_warning"
message = "High zero-result rate detected. Consider updating index or reviewing query patterns."

[[alerts.rules]]
name = "slow_queries"
condition = "query_latency_p99 > 500"
window = "15m"
action = "auto_optimize"
```

---

## 7. CLI Integration

### 7.1 New Commands

```bash
# View current metrics
mdkb metrics show                      # Summary dashboard
mdkb metrics show --metric query_latency_ms --period 1h
mdkb metrics show --format json        # For scripting

# Export metrics
mdkb metrics export --from 2026-01-01 --to 2026-01-31 > metrics.csv

# Analyze experiments
mdkb metrics experiment chunking_strategy --analyze

# Trigger manual rollup
mdkb metrics rollup --period day
```

### 7.2 Status Command Enhancement

```bash
$ mdkb status --metrics

Index Status:
  Documents: 1,234
  Collections: 3
  Index size: 45.2 MB
  Last updated: 2 minutes ago

Performance (last hour):
  Query latency: p50=12ms, p95=45ms, p99=120ms
  Queries: 156
  Zero-result rate: 3.2%

Health Indicators:
  [OK] Latency within bounds
  [OK] Zero-result rate acceptable
  [WARN] 23 stale documents need reindex
```

---

## 8. MCP Integration

### 8.1 New MCP Tools

```rust
/// Exposed as mdkb_record_feedback MCP tool
pub fn record_feedback(
    session_id: String,
    query_id: i64,
    feedback_type: FeedbackType,  // Used, ThumbsUp, ThumbsDown, Skip
    result_position: Option<i32>,
    doc_id: Option<String>,
) -> Result<()>;

/// Exposed as mdkb_metrics MCP tool
pub fn get_metrics(
    metric_names: Vec<String>,
    period: String,  // "1h", "24h", "7d"
) -> Result<MetricsSummary>;
```

### 8.2 Session Tracking

MCP clients should include a session identifier in requests for accurate session analysis:

```json
{
    "method": "mdkb_search",
    "params": {
        "query": "rust error handling",
        "limit": 10,
        "session_id": "client-uuid-here"  // Optional but recommended
    }
}
```

---

## 9. Research Questions Answered

### Q1: What metrics can be collected passively?

**Fully passive (no client cooperation):**
- All latency metrics (query, indexing, embedding)
- All throughput metrics
- Index health metrics (size, doc count, staleness)
- Zero-result rate
- Result count distribution
- Score distributions (BM25, cosine similarity)

**Requires session tracking:**
- Re-search patterns
- Query refinement detection
- Search-to-get ratio
- Abandoned search rate

### Q2: What metrics require explicit feedback?

- Result position clicked/used
- Thumbs up/down ratings
- Time spent on retrieved document
- "Found what I needed" confirmation

### Q3: How to store and query historical metrics?

**Storage strategy:**
1. Raw events in `metrics_timeseries` and `query_events` (90-day retention)
2. Hourly rollups in `metrics_rollups` (1-year retention)
3. Daily rollups for long-term trends (indefinite)

**Query optimization:**
- Composite indexes on (metric_name, ts)
- Partitioning by month if data grows large
- Pre-computed percentiles in rollups

### Q4: How to trigger optimization based on metrics?

**Automatic triggers:**
- Threshold-based alerts with configurable actions
- Periodic health checks (e.g., every hour)
- Event-driven (e.g., on high error rate)

**Manual triggers:**
- CLI commands for on-demand analysis
- MCP tool for programmatic access

---

## 10. Implementation Phases

### Phase 1: Foundation (MVP)
- [ ] Metrics storage schema
- [ ] Basic latency collection (query, indexing)
- [ ] Zero-result rate tracking
- [ ] `mdkb metrics show` command

### Phase 2: Session Analysis
- [ ] Query event logging
- [ ] Session boundary detection
- [ ] Re-search pattern detection
- [ ] Search-to-get ratio

### Phase 3: Advanced Metrics
- [ ] Score distribution histograms
- [ ] Vector recall estimation (sampled)
- [ ] Automatic rollups
- [ ] Alert system

### Phase 4: A/B Testing
- [ ] Experiment configuration
- [ ] Variant assignment
- [ ] Experiment analysis queries
- [ ] MCP feedback integration

---

## 11. References

### Search Quality Metrics
- [BEIR Benchmark](https://github.com/beir-cellar/beir) - Heterogeneous IR evaluation
- [MTEB Leaderboard](https://huggingface.co/spaces/mteb/leaderboard) - Embedding model benchmarks
- [Elastic Search Relevance](https://www.elastic.co/what-is/search-relevance)
- [MongoDB Search Relevance Metrics](https://www.mongodb.com/resources/basics/search-relevance)

### Latency Monitoring
- [P50 vs P95 vs P99 Latency](https://oneuptime.com/blog/post/2025-09-15-p50-vs-p95-vs-p99-latency-percentiles/view)
- [Mastering Latency Metrics](https://medium.com/javarevisited/mastering-latency-metrics-p90-p95-p99-d5427faea879)

### Vespa Evaluation
- [Vespa Evaluation API](https://vespa-engine.github.io/pyvespa/api/vespa/evaluation.html)
- [IR Evaluation with Uncertainty](https://blog.vespa.ai/passage-uncertainty-evaluation/)

### Algolia Analytics
- [Algolia Search Analytics Metrics](https://www.algolia.com/doc/guides/search-analytics/concepts/metrics)
- [A/B Testing Metrics for Search](https://www.algolia.com/blog/engineering/a-b-testing-metrics-evaluating-the-best-metrics-for-your-search)

### GGUF/Embedding Optimization
- [GGUF Quantization Guide](https://apatero.com/blog/gguf-quantized-models-complete-guide-2025)
- [Embedding Quantization Quality](https://zilliz.com/ai-faq/how-do-i-quantize-embedding-models-without-significant-quality-loss)

### SQLite Time Series
- [Time Series on SQLite](https://dev.to/zanzythebar/building-high-performance-time-series-on-sqlite-with-go-uuidv7-sqlc-and-libsql-3ejb)
- [SQLite FTS5 Performance](https://sqlite.org/fts5.html)
