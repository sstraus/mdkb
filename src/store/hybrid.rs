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

/// Reorder results to mitigate "lost in the middle" attention bias.
///
/// LLMs attend more to content at the beginning and end of their context.
/// This reorders a ranked list so the strongest results land at positions
/// 1 and N, with weaker results in the middle.
///
/// Input must be sorted by score descending (strongest first).
/// Output: positions 1,3,5... get odd-ranked items; positions N,N-1,N-2...
/// get even-ranked items.
pub fn lost_in_middle_reorder<T>(items: &mut Vec<T>) {
    if items.len() <= 2 {
        return;
    }

    let original: Vec<T> = items.drain(..).collect();
    let mut front = Vec::new();
    let mut back = Vec::new();

    for (i, item) in original.into_iter().enumerate() {
        if i % 2 == 0 {
            front.push(item);
        } else {
            back.push(item);
        }
    }

    // Back items go in reverse (weakest in middle, stronger toward end)
    back.reverse();

    items.extend(front);
    items.extend(back);
}

/// Normalize scores to [0, 1] range using max-normalization.
///
/// Divides all scores by the maximum observed score, preserving relative
/// differences between entries. This avoids the min-max problem where
/// single-source results get their differences artificially amplified.
pub fn normalize_scores(scores: &mut [(i64, f64)]) {
    if scores.is_empty() {
        return;
    }

    let max = scores.iter().map(|(_, s)| *s).fold(0.0_f64, f64::max);

    if max > 0.0 {
        for (_, score) in scores.iter_mut() {
            *score /= max;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_bm25_result(id: i64, score: f64) -> SearchResult {
        SearchResult {
            id,
            collection: "docs".to_string(),
            path: format!("doc{}.md", id),
            title: Some(format!("Document {}", id)),
            score,
            snippets: vec![],
            status: None,
            superseded_by: None,
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

        // Highest should be 1.0, others proportional (max-normalization)
        assert!((scores.iter().find(|(id, _)| *id == 1).unwrap().1 - 1.0).abs() < 0.001);
        assert!((scores.iter().find(|(id, _)| *id == 2).unwrap().1 - 0.5).abs() < 0.001);
        assert!((scores.iter().find(|(id, _)| *id == 3).unwrap().1 - 0.75).abs() < 0.001);
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

    #[test]
    fn test_rrf_weight_effects() {
        let bm25 = vec![
            make_bm25_result(1, -5.0), // BM25 prefers doc 1
            make_bm25_result(2, -6.0),
        ];
        let vector = vec![
            (2, 0.1), // Vector prefers doc 2
            (1, 0.2),
        ];

        // Heavy BM25 weight should favor doc 1
        let bm25_heavy = HybridConfig {
            rrf_k: 60.0,
            bm25_weight: 10.0,
            vector_weight: 0.1,
        };
        let fused_bm25 = rrf_fusion(&bm25, &vector, &bm25_heavy);
        assert_eq!(fused_bm25[0].0, 1);

        // Heavy vector weight should favor doc 2
        let vector_heavy = HybridConfig {
            rrf_k: 60.0,
            bm25_weight: 0.1,
            vector_weight: 10.0,
        };
        let fused_vector = rrf_fusion(&bm25, &vector, &vector_heavy);
        assert_eq!(fused_vector[0].0, 2);
    }

    #[test]
    fn test_rrf_k_effect() {
        let bm25 = vec![
            make_bm25_result(1, -5.0),
            make_bm25_result(2, -6.0),
            make_bm25_result(3, -7.0),
        ];
        let vector: Vec<(i64, f32)> = vec![];

        // Lower k = more emphasis on rank differences
        let low_k = HybridConfig {
            rrf_k: 1.0,
            bm25_weight: 1.0,
            vector_weight: 0.0,
        };
        let fused_low = rrf_fusion(&bm25, &vector, &low_k);

        // Higher k = flattens rank differences
        let high_k = HybridConfig {
            rrf_k: 100.0,
            bm25_weight: 1.0,
            vector_weight: 0.0,
        };
        let fused_high = rrf_fusion(&bm25, &vector, &high_k);

        // Score gap between rank 1 and 3 should be larger with low k
        let gap_low = fused_low[0].1 - fused_low[2].1;
        let gap_high = fused_high[0].1 - fused_high[2].1;
        assert!(gap_low > gap_high);
    }

    #[test]
    fn test_normalize_preserves_order() {
        let mut scores = vec![(5, 0.05), (3, 0.03), (1, 0.01), (4, 0.04), (2, 0.02)];
        let original_order: Vec<i64> = scores.iter().map(|(id, _)| *id).collect();

        normalize_scores(&mut scores);

        // Order should be preserved
        let normalized_order: Vec<i64> = scores.iter().map(|(id, _)| *id).collect();
        assert_eq!(original_order, normalized_order);

        // Scores should be in [0, 1]
        for (_, score) in &scores {
            assert!(*score >= 0.0 && *score <= 1.0);
        }

        // Highest original score should become 1.0
        assert!((scores[0].1 - 1.0).abs() < 0.001);
        // Lowest original score (0.01) should become 0.01/0.05 = 0.2
        assert!((scores[2].1 - 0.2).abs() < 0.001);
    }

    #[test]
    fn test_rrf_many_results() {
        // Test with many results to verify scalability
        let bm25: Vec<SearchResult> = (1..=50)
            .map(|i| make_bm25_result(i, -(i as f64)))
            .collect();
        let vector: Vec<(i64, f32)> = (51..=100).map(|i| (i, i as f32 * 0.01)).collect();

        let config = HybridConfig::default();
        let fused = rrf_fusion(&bm25, &vector, &config);

        // Should have all 100 documents
        assert_eq!(fused.len(), 100);

        // All scores should be positive
        for (_, score) in &fused {
            assert!(*score > 0.0);
        }

        // BM25 docs should generally rank higher due to higher weight
        let top_10: Vec<i64> = fused.iter().take(10).map(|(id, _)| *id).collect();
        let bm25_in_top10 = top_10.iter().filter(|id| **id <= 50).count();
        assert!(bm25_in_top10 >= 7); // Most of top 10 should be from BM25
    }

    #[test]
    fn test_rrf_duplicate_in_both_lists() {
        // Same document appearing in both lists should get boosted
        let bm25 = vec![make_bm25_result(1, -5.0)];
        let vector = vec![(1, 0.1)];

        let config = HybridConfig::default();
        let fused = rrf_fusion(&bm25, &vector, &config);

        assert_eq!(fused.len(), 1);
        // Should have contributions from both lists
        let expected_score =
            config.bm25_weight / (config.rrf_k + 1.0) + config.vector_weight / (config.rrf_k + 1.0);
        assert!((fused[0].1 - expected_score).abs() < 0.0001);
    }

    #[test]
    fn test_normalize_single_score() {
        let mut scores = vec![(1, 0.5)];
        normalize_scores(&mut scores);
        // Single score should become 1.0 (range is 0, so all equal case)
        assert!((scores[0].1 - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_rrf_score_calculation() {
        // Verify exact RRF score calculation
        let bm25 = vec![make_bm25_result(1, -5.0)];
        let vector: Vec<(i64, f32)> = vec![];

        let config = HybridConfig {
            rrf_k: 60.0,
            bm25_weight: 1.0,
            vector_weight: 0.7,
        };
        let fused = rrf_fusion(&bm25, &vector, &config);

        // RRF score = bm25_weight / (k + rank + 1) = 1.0 / (60 + 0 + 1) = 1/61
        let expected = 1.0 / 61.0;
        assert!((fused[0].1 - expected).abs() < 0.0001);
    }

    // ==================== Lost-in-the-Middle Tests ====================

    #[test]
    fn test_lost_in_middle_reorder_basic() {
        // Input ranked by score: [1st, 2nd, 3rd, 4th, 5th]
        let mut items = vec![1, 2, 3, 4, 5];
        lost_in_middle_reorder(&mut items);

        // Expected: front gets 1,3,5 (odd positions); back gets 4,2 (even, reversed)
        // Result: [1, 3, 5, 4, 2]
        // Strongest (1) at position 1, second strongest (2) at position N
        assert_eq!(items[0], 1, "strongest at position 1");
        assert_eq!(items[items.len() - 1], 2, "second strongest at position N");
        // Weakest in the middle
        assert_eq!(items, vec![1, 3, 5, 4, 2]);
    }

    #[test]
    fn test_lost_in_middle_reorder_small() {
        // 0 items: no-op
        let mut empty: Vec<i32> = vec![];
        lost_in_middle_reorder(&mut empty);
        assert!(empty.is_empty());

        // 1 item: no-op
        let mut single = vec![42];
        lost_in_middle_reorder(&mut single);
        assert_eq!(single, vec![42]);

        // 2 items: no-op
        let mut pair = vec![1, 2];
        lost_in_middle_reorder(&mut pair);
        assert_eq!(pair, vec![1, 2]);
    }

    #[test]
    fn test_lost_in_middle_reorder_three() {
        let mut items = vec![1, 2, 3];
        lost_in_middle_reorder(&mut items);
        // front: 1, 3; back: 2 (reversed = 2)
        // Result: [1, 3, 2]
        assert_eq!(items[0], 1, "strongest at position 1");
        assert_eq!(items[2], 2, "second strongest at position N");
    }

    #[test]
    fn test_lost_in_middle_preserves_length() {
        for len in 0..=20 {
            let mut items: Vec<i32> = (0..len).collect();
            lost_in_middle_reorder(&mut items);
            assert_eq!(items.len(), len as usize, "length must be preserved for len={len}");
        }
    }
}
