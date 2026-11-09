//! Grouping candidates that share a shape, and dropping the pairs that share
//! it for a reason other than duplication.
//!
//! Union-find rather than pairwise output: three mutually similar functions are
//! one finding a reader acts on once, not three pairs they read as three
//! separate problems and fix three separate ways.

use std::collections::HashSet;
use std::hash::BuildHasher;

use super::body::hamming;

/// Why a pair that looked alike is not reported.
///
/// Carried rather than discarded silently: a suppression is a decision the tool
/// made on the reader's behalf, and `mdkb dup --explain` can say which one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suppression {
    /// One calls the other, on an edge the resolver actually placed. A wrapper
    /// resembling what it wraps is the design, not a defect.
    Calls,
    /// Both are members of the same class or trait. Two methods of one type
    /// sharing a skeleton is what a type is for.
    SharedOwner,
}

/// Disjoint sets over candidate indices, with union by rank and path halving.
#[derive(Debug)]
pub struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u32>,
}

impl UnionFind {
    pub fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    /// Representative of `i`'s set, halving the path on the way up.
    pub fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    pub fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        let (small, large) = if self.rank[a] < self.rank[b] {
            (a, b)
        } else {
            (b, a)
        };
        self.parent[small] = large;
        if self.rank[small] == self.rank[large] {
            self.rank[large] += 1;
        }
    }

    /// Sets of two or more, each sorted, ordered by their smallest member.
    ///
    /// Singletons are dropped: a symbol resembling nothing is not a finding.
    pub fn groups(&mut self) -> Vec<Vec<usize>> {
        let mut by_root: std::collections::HashMap<usize, Vec<usize>> =
            std::collections::HashMap::new();
        for i in 0..self.parent.len() {
            by_root.entry(self.find(i)).or_default().push(i);
        }
        let mut groups: Vec<Vec<usize>> = by_root.into_values().filter(|g| g.len() > 1).collect();
        for group in &mut groups {
            group.sort_unstable();
        }
        groups.sort_unstable_by_key(|g| g[0]);
        groups
    }
}

/// What a pair is judged on, before any embedding exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    /// Structural simhash of the body.
    pub simhash: u64,
    /// The symbol's row id, for looking the pair up among the call edges.
    pub symbol_id: i64,
    /// The class or trait this is a member of.
    pub owner_name: Option<String>,
}

/// Group fingerprints within `threshold` bits of each other.
///
/// O(n²) in the candidate count, and deliberately: this is the *cheap* half —
/// one xor and one popcount per pair — but cheap per pair is not cheap in
/// total. 200 000 candidates is 2·10^10 popcounts. The line-span filter in
/// [`candidates`](super::candidates::candidates) and the node-count filter are
/// what keep n small enough for this to be the affordable pass; the embedding
/// pass then runs only over what survives it.
///
/// `suppressed_pairs` holds symbol-id pairs, lower id first.
pub fn cluster<S: BuildHasher>(
    fingerprints: &[Fingerprint],
    threshold: u32,
    suppressed_pairs: &HashSet<(i64, i64), S>,
) -> Vec<Vec<usize>> {
    let admissible = |a: usize, b: usize| {
        hamming(fingerprints[a].simhash, fingerprints[b].simhash) <= threshold
            && suppression_for(&fingerprints[a], &fingerprints[b], suppressed_pairs).is_none()
    };

    // Union-find first, but only as a cheap pre-filter: it says which
    // candidates *could* share a group, and most components are two or three
    // symbols that need no further work. The bound is applied inside each one.
    let mut uf = UnionFind::new(fingerprints.len());
    for i in 0..fingerprints.len() {
        for j in (i + 1)..fingerprints.len() {
            if admissible(i, j) {
                uf.union(i, j);
            }
        }
    }

    let mut groups: Vec<Vec<usize>> = uf
        .groups()
        .into_iter()
        .flat_map(|component| dense_groups(&component, |i| fingerprints[i].simhash, admissible))
        .collect();
    groups.sort_unstable_by_key(|g| g[0]);
    groups
}

/// Split a component into groups where *every* pair is admissible.
///
/// A connected component is not a finding. Similarity is not transitive, so
/// closing it transitively lets a chain of admissible pairs carry two members
/// into one group that resemble nothing of each other — on the real store that
/// put 3209 symbols in a single cluster whose widest pair was 47 bits apart
/// against a threshold of 12, while the report described it with that 47.
///
/// Complete linkage restores the bound by construction: a member joins only if
/// it is admissible with every member already in the group, so the group's
/// diameter cannot exceed the threshold. Greedy rather than optimal — the
/// minimum such partition is a clique partition, which is NP-hard and not even
/// unique — so this is a deterministic heuristic and says so.
///
/// `key` orders the candidates. It must be content-derived: seeding on the row
/// id or on the position in the candidate list would repartition the repository
/// when a file is edited above a symbol, and the ignore-list is keyed on
/// membership.
///
/// Ties on the key are ties at distance zero, so which of two identical shapes
/// seeds first cannot change who is admissible with whom.
///
/// O(k²) admissibility tests and O(k) memory: each seed and each member added
/// costs one sweep, and there are at most k of each.
pub fn dense_groups<K: Ord>(
    component: &[usize],
    key: impl Fn(usize) -> K,
    admissible: impl Fn(usize, usize) -> bool,
) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = component.to_vec();
    order.sort_unstable_by(|&a, &b| key(a).cmp(&key(b)).then(a.cmp(&b)));

    let mut taken = vec![false; order.len()];
    // `allowed[n]` — is `order[n]` still admissible with every member taken so
    // far. Rebuilt per seed, narrowed by each member added.
    let mut allowed = vec![false; order.len()];
    let mut groups = Vec::new();

    for seed in 0..order.len() {
        if taken[seed] {
            continue;
        }
        taken[seed] = true;
        let mut group = vec![order[seed]];
        for (n, slot) in allowed.iter_mut().enumerate().skip(seed + 1) {
            *slot = !taken[n] && admissible(order[seed], order[n]);
        }

        for n in (seed + 1)..order.len() {
            if !allowed[n] {
                continue;
            }
            taken[n] = true;
            group.push(order[n]);
            for (m, slot) in allowed.iter_mut().enumerate().skip(n + 1) {
                *slot = *slot && admissible(order[n], order[m]);
            }
        }

        if group.len() > 1 {
            group.sort_unstable();
            groups.push(group);
        }
    }
    groups
}

/// Why this pair is not duplication, or `None` if nothing rules it out.
///
/// Only signals the index can be *sure* of. There is deliberately no rule here
/// for "similar names" or "same file": both are guesses, and a guess that
/// suppresses removes the answer without saying so.
pub fn suppression_for<S: BuildHasher>(
    a: &Fingerprint,
    b: &Fingerprint,
    suppressed_pairs: &HashSet<(i64, i64), S>,
) -> Option<Suppression> {
    let pair = if a.symbol_id <= b.symbol_id {
        (a.symbol_id, b.symbol_id)
    } else {
        (b.symbol_id, a.symbol_id)
    };
    if suppressed_pairs.contains(&pair) {
        return Some(Suppression::Calls);
    }
    match (&a.owner_name, &b.owner_name) {
        // Both non-null and equal. Two free functions are NOT "the same owner":
        // that is the ordinary case, and treating `None == None` as a shared
        // owner would suppress every finding in the repository.
        (Some(x), Some(y)) if x == y => Some(Suppression::SharedOwner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(symbol_id: i64, simhash: u64, owner: Option<&str>) -> Fingerprint {
        Fingerprint {
            simhash,
            symbol_id,
            owner_name: owner.map(str::to_string),
        }
    }

    fn no_calls() -> HashSet<(i64, i64)> {
        HashSet::new()
    }

    #[test]
    fn two_fingerprints_within_the_threshold_are_one_cluster() {
        // One bit apart.
        let fps = [fp(1, 0b1010, None), fp(2, 0b1011, None)];

        assert_eq!(cluster(&fps, 12, &no_calls()), vec![vec![0, 1]]);
    }

    #[test]
    fn two_fingerprints_past_the_threshold_are_not_a_cluster() {
        let fps = [fp(1, 0, None), fp(2, u64::MAX, None)];

        assert!(
            cluster(&fps, 12, &no_calls()).is_empty(),
            "64 bits apart is not a near-duplicate"
        );
    }

    #[test]
    fn a_pair_exactly_at_the_threshold_still_clusters() {
        // Threshold is inclusive: `> threshold` is the rejection, so a pair
        // exactly at it is a finding. An off-by-one here changes what the tool
        // reports across the whole repository.
        let fps = [fp(1, 0, None), fp(2, 0b1111, None)];

        assert_eq!(cluster(&fps, 4, &no_calls()), vec![vec![0, 1]]);
        assert_eq!(cluster(&fps, 3, &no_calls()), [] as [Vec<usize>; 0]);
    }

    #[test]
    fn three_mutually_similar_candidates_are_one_cluster_not_three_pairs() {
        let fps = [
            fp(1, 0b0000, None),
            fp(2, 0b0001, None),
            fp(3, 0b0011, None),
        ];

        assert_eq!(cluster(&fps, 12, &no_calls()), vec![vec![0, 1, 2]]);
    }

    #[test]
    fn a_chain_past_the_threshold_is_split_rather_than_joined() {
        // A resembles B, B resembles C, A does not resemble C. Closing that
        // transitively is what put 3209 symbols in one group on the real store,
        // with a widest pair of 47 bits against a threshold of 12 — a group the
        // report then described using a number the threshold says is
        // impossible. A resembles B, and that is the whole finding.
        let fps = [
            fp(1, 0b0000_0000, None),
            fp(2, 0b0000_1111, None),
            fp(3, 0b1111_1111, None),
        ];

        assert_eq!(
            cluster(&fps, 4, &no_calls()),
            vec![vec![0, 1]],
            "C is 8 bits from A: it belongs to neither group, and alone it is \
             not a finding"
        );
    }

    /// Every pair of every reported group is within the threshold.
    ///
    /// The contract the report renders — "same shape, N bits apart", where N is
    /// the widest pair — is only meaningful if N cannot exceed the threshold.
    fn assert_bounded(groups: &[Vec<usize>], fps: &[Fingerprint], threshold: u32) {
        for group in groups {
            for (n, &i) in group.iter().enumerate() {
                for &j in &group[n + 1..] {
                    let d = hamming(fps[i].simhash, fps[j].simhash);
                    assert!(
                        d <= threshold,
                        "group {group:?} holds a pair {d} bits apart, above the \
                         threshold of {threshold}"
                    );
                }
            }
        }
    }

    /// A spread of shapes dense enough that a connected-component pass chains
    /// most of them together: consecutive values differ by few bits.
    fn a_chainable_spread() -> Vec<Fingerprint> {
        (0..64u64)
            .map(|n| fp(n as i64 + 1, (1u64 << n) - 1, None))
            .collect()
    }

    #[test]
    fn no_reported_group_is_wider_than_the_threshold() {
        let fps = a_chainable_spread();
        for threshold in [1, 4, 12, 20] {
            let groups = cluster(&fps, threshold, &no_calls());
            assert!(
                !groups.is_empty(),
                "threshold {threshold} must still find something to bound"
            );
            assert_bounded(&groups, &fps, threshold);
        }
    }

    #[test]
    fn the_connected_component_pass_would_have_failed_that_bound() {
        // The control: the same input under the old rule produces a group far
        // wider than the threshold, so the test above is not passing because
        // the fixture is easy.
        let fps = a_chainable_spread();
        let mut uf = UnionFind::new(fps.len());
        for i in 0..fps.len() {
            for j in (i + 1)..fps.len() {
                if hamming(fps[i].simhash, fps[j].simhash) <= 12 {
                    uf.union(i, j);
                }
            }
        }
        let component = uf.groups().into_iter().max_by_key(Vec::len).expect("one");
        let widest = component
            .iter()
            .flat_map(|&i| component.iter().map(move |&j| (i, j)))
            .map(|(i, j)| hamming(fps[i].simhash, fps[j].simhash))
            .max()
            .expect("pairs");
        assert!(
            widest > 12,
            "the fixture must actually chain: widest pair was {widest}"
        );
    }

    #[test]
    fn the_partition_does_not_depend_on_the_order_the_candidates_arrived_in() {
        // Determinism is tie-broken on the fingerprint, not on the row id or the
        // position in the candidate list — both of which move when a file is
        // edited above the symbol.
        let fps = a_chainable_spread();
        let shapes = |groups: Vec<Vec<usize>>, source: &[Fingerprint]| {
            let mut out: Vec<Vec<u64>> = groups
                .iter()
                .map(|g| {
                    let mut s: Vec<u64> = g.iter().map(|&i| source[i].simhash).collect();
                    s.sort_unstable();
                    s
                })
                .collect();
            out.sort();
            out
        };

        let forward = shapes(cluster(&fps, 12, &no_calls()), &fps);

        let mut reversed: Vec<Fingerprint> = fps.clone();
        reversed.reverse();
        let backward = shapes(cluster(&reversed, 12, &no_calls()), &reversed);

        assert_eq!(forward, backward, "the same index, two arrival orders");
    }

    #[test]
    fn a_suppressed_pair_cannot_rejoin_through_a_third_member() {
        // Three identical shapes, one pair suppressed by a call edge. Under a
        // connected component the third member reinstates the pair the
        // suppression removed, which makes the suppression decorative.
        let fps = [
            fp(1, 0b1010, None),
            fp(2, 0b1010, None),
            fp(3, 0b1010, None),
        ];
        let calls = HashSet::from([(1i64, 2i64)]);

        for group in cluster(&fps, 12, &calls) {
            assert!(
                !(group.contains(&0) && group.contains(&1)),
                "the suppressed pair is back in {group:?}"
            );
        }
    }

    #[test]
    fn two_unrelated_clusters_stay_apart() {
        let fps = [
            fp(1, 0b0000, None),
            fp(2, 0b0001, None),
            fp(3, u64::MAX, None),
            fp(4, u64::MAX - 1, None),
        ];

        assert_eq!(cluster(&fps, 4, &no_calls()), vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn a_lone_candidate_is_not_a_finding() {
        let fps = [fp(1, 0, None)];

        assert_eq!(cluster(&fps, 12, &no_calls()), [] as [Vec<usize>; 0]);
    }

    #[test]
    fn no_candidates_is_no_clusters_not_a_panic() {
        assert_eq!(cluster(&[], 12, &no_calls()), [] as [Vec<usize>; 0]);
    }

    #[test]
    fn a_confident_call_edge_suppresses_the_pair() {
        let fps = [fp(1, 0b1010, None), fp(2, 0b1010, None)];
        let calls = HashSet::from([(1i64, 2i64)]);

        assert!(
            cluster(&fps, 12, &calls).is_empty(),
            "a wrapper resembling what it wraps is the design"
        );
    }

    #[test]
    fn suppression_does_not_care_which_way_the_call_went() {
        let fps = [fp(9, 0b1010, None), fp(5, 0b1010, None)];
        let calls = HashSet::from([(5i64, 9i64)]);

        assert_eq!(cluster(&fps, 12, &calls), [] as [Vec<usize>; 0]);
    }

    #[test]
    fn two_methods_of_the_same_type_are_suppressed() {
        let fps = [fp(1, 0b1010, Some("Store")), fp(2, 0b1010, Some("Store"))];

        assert_eq!(cluster(&fps, 12, &no_calls()), [] as [Vec<usize>; 0]);
    }

    #[test]
    fn two_methods_of_different_types_are_not_suppressed() {
        // The interesting finding: the same logic written into two types.
        let fps = [fp(1, 0b1010, Some("Store")), fp(2, 0b1010, Some("Cache"))];

        assert_eq!(cluster(&fps, 12, &no_calls()), vec![vec![0, 1]]);
    }

    #[test]
    fn two_free_functions_do_not_share_an_owner() {
        // Both owners are NULL. Reading that as "same owner" would suppress
        // every free-function finding in the repository — the common case.
        let fps = [fp(1, 0b1010, None), fp(2, 0b1010, None)];

        assert_eq!(cluster(&fps, 12, &no_calls()), vec![vec![0, 1]]);
    }

    #[test]
    fn a_method_and_a_free_function_are_not_suppressed() {
        let fps = [fp(1, 0b1010, Some("Store")), fp(2, 0b1010, None)];

        assert_eq!(cluster(&fps, 12, &no_calls()), vec![vec![0, 1]]);
    }

    /// Suppressing one pair must not break the cluster the other two form.
    #[test]
    fn suppressing_one_pair_leaves_the_rest_of_the_cluster() {
        let fps = [
            fp(1, 0b1010, None),
            fp(2, 0b1010, None),
            fp(3, 0b1010, None),
        ];
        let calls = HashSet::from([(1i64, 2i64)]);

        // 1-2 is suppressed. 1-3 and 2-3 are not, and either is a finding, but
        // a partition has to choose: symbol 3 can hold one of them, not both.
        // The alternative is overlapping groups, which shows the same symbol
        // twice and leaves the reader to work out it is one decision. The pair
        // the suppression removed stays removed either way, which is the point.
        assert_eq!(cluster(&fps, 12, &calls), vec![vec![0, 2]]);
    }

    #[test]
    fn suppression_names_its_reason() {
        let calls = HashSet::from([(1i64, 2i64)]);
        assert_eq!(
            suppression_for(&fp(1, 0, None), &fp(2, 0, None), &calls),
            Some(Suppression::Calls)
        );
        assert_eq!(
            suppression_for(&fp(3, 0, Some("T")), &fp(4, 0, Some("T")), &no_calls()),
            Some(Suppression::SharedOwner)
        );
        assert_eq!(
            suppression_for(&fp(3, 0, Some("T")), &fp(4, 0, Some("U")), &no_calls()),
            None
        );
    }

    #[test]
    fn union_find_keeps_groups_sorted_and_ordered() {
        let mut uf = UnionFind::new(6);
        uf.union(4, 1);
        uf.union(5, 2);
        uf.union(1, 0);

        assert_eq!(uf.groups(), vec![vec![0, 1, 4], vec![2, 5]]);
    }

    #[test]
    fn union_find_survives_a_long_chain() {
        // Path halving: a degenerate chain must not become a linear find.
        let mut uf = UnionFind::new(1000);
        for i in 1..1000 {
            uf.union(i - 1, i);
        }

        let groups = uf.groups();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 1000);
    }
}
