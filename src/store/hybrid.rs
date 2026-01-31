//! Hybrid search combining BM25 and vector search with RRF fusion.
//!
//! Uses Reciprocal Rank Fusion to combine keyword and semantic search results.

use std::collections::HashMap;

use crate::domain::SearchResult;

/// Configuration for hybrid search.
#[derive(Debug, Clone)]
pub struct HybridConfig {
    /// RRF constant (higher = more weight to lower ranks).
    pub rrf_k: f64,
    /// Weight for BM25 results.
    pub bm25_weight: f64,
    /// Weight for vector results.
    pub vector_weight: f64,
}

impl Default for HybridConfig {
    fn default() -> Self {
        Self {
            rrf_k: 60.0,
            bm25_weight: 1.0,
            vector_weight: 0.7,
        }
    }
}

/// Fuse BM25 and vector search results using Reciprocal Rank Fusion.
///
/// RRF score = sum of (weight * 1 / (k + rank)) for each ranking
pub fn rrf_fusion(
    bm25_results: &[SearchResult],
    vector_results: &[(i64, f32)],
    config: &HybridConfig,
) -> Vec<(i64, f64)> {
    let mut scores: HashMap<i64, f64> = HashMap::new();

    // Score from BM25 results
    for (rank, result) in bm25_results.iter().enumerate() {
        let score = config.bm25_weight / (config.rrf_k + rank as f64 + 1.0);
        *scores.entry(result.id).or_default() += score;
    }

    // Score from vector results
    for (rank, (doc_id, _distance)) in vector_results.iter().enumerate() {
        let score = config.vector_weight / (config.rrf_k + rank as f64 + 1.0);
        *scores.entry(*doc_id).or_default() += score;
    }

    // Sort by combined score descending
    let mut results: Vec<_> = scores.into_iter().collect();
    results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    results
}

/// Normalize scores to [0, 1] range.
pub fn normalize_scores(scores: &mut [(i64, f64)]) {
    if scores.is_empty() {
        return;
    }

    let max = scores.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);
    let min = scores.iter().map(|(_, s)| *s).fold(f64::MAX, f64::min);
    let range = max - min;

    if range > 0.0 {
        for (_, score) in scores.iter_mut() {
            *score = (*score - min) / range;
        }
    } else {
        // All scores are equal, normalize to 1.0
        for (_, score) in scores.iter_mut() {
            *score = 1.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bm25_result(id: i64, score: f64) -> SearchResult {
        SearchResult {
            id,
            path: format!("doc{}.md", id),
            title: Some(format!("Document {}", id)),
            score,
            snippets: vec![],
        }
    }

    #[test]
    fn test_rrf_fusion_basic() {
        let bm25 = vec![
            make_bm25_result(1, -5.0),
            make_bm25_result(2, -6.0),
            make_bm25_result(3, -7.0),
        ];

        let vector = vec![
            (2, 0.1), // doc 2 is closest in vector
            (1, 0.2),
            (4, 0.3), // doc 4 only in vector results
        ];

        let config = HybridConfig::default();
        let fused = rrf_fusion(&bm25, &vector, &config);

        // Docs appearing in both lists should be ranked higher
        assert!(!fused.is_empty());
        // Both doc 1 and doc 2 appear in both lists
        let top_ids: Vec<i64> = fused.iter().take(2).map(|(id, _)| *id).collect();
        assert!(top_ids.contains(&1));
        assert!(top_ids.contains(&2));
        // Doc 4 (only in vector) should be ranked lower than docs in both
        let doc4_pos = fused.iter().position(|(id, _)| *id == 4);
        assert!(doc4_pos.is_some());
        assert!(doc4_pos.unwrap() >= 2);
    }

    #[test]
    fn test_rrf_fusion_empty_inputs() {
        let bm25: Vec<SearchResult> = vec![];
        let vector: Vec<(i64, f32)> = vec![];
        let config = HybridConfig::default();

        let fused = rrf_fusion(&bm25, &vector, &config);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_rrf_fusion_bm25_only() {
        let bm25 = vec![make_bm25_result(1, -5.0), make_bm25_result(2, -6.0)];

        let vector: Vec<(i64, f32)> = vec![];
        let config = HybridConfig::default();

        let fused = rrf_fusion(&bm25, &vector, &config);
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, 1); // maintains BM25 order
    }

    #[test]
    fn test_rrf_fusion_vector_only() {
        let bm25: Vec<SearchResult> = vec![];
        let vector = vec![(1, 0.1), (2, 0.2)];

        let config = HybridConfig::default();
        let fused = rrf_fusion(&bm25, &vector, &config);

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, 1); // maintains vector order
    }

    #[test]
    fn test_normalize_scores() {
        let mut scores = vec![(1, 0.02), (2, 0.01), (3, 0.015)];

        normalize_scores(&mut scores);

        // Highest should be 1.0, lowest 0.0
        assert!((scores.iter().find(|(id, _)| *id == 1).unwrap().1 - 1.0).abs() < 0.001);
        assert!((scores.iter().find(|(id, _)| *id == 2).unwrap().1 - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_normalize_scores_empty() {
        let mut scores: Vec<(i64, f64)> = vec![];
        normalize_scores(&mut scores);
        assert!(scores.is_empty());
    }

    #[test]
    fn test_normalize_scores_equal() {
        let mut scores = vec![(1, 0.5), (2, 0.5)];

        normalize_scores(&mut scores);

        // Equal scores should all become 1.0
        assert!((scores[0].1 - 1.0).abs() < 0.001);
        assert!((scores[1].1 - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_config_default() {
        let config = HybridConfig::default();
        assert!((config.rrf_k - 60.0).abs() < 0.001);
        assert!((config.bm25_weight - 1.0).abs() < 0.001);
        assert!((config.vector_weight - 0.7).abs() < 0.001);
    }
}
