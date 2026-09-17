//! Replay harness for story 091-a2a9 / plan Step 9.
//!
//! The story's last criterion is about the store on disk, not about a fixture:
//! the candidates that carry one budget-limit lesson have to end in one cluster
//! after the recluster pass. That can only be answered against a real store, and
//! a real store cannot be committed — its candidates are distilled from a
//! developer's transcripts and its lessons quote them.
//!
//! So the harness runs against a COPY named by an environment variable and does
//! nothing when it is absent. It never writes to the original.
//!
//! ```text
//! cp ~/repo/.mdkb/index.sqlite /tmp/recluster-probe.sqlite
//! MDKB_RECLUSTER_STORE=/tmp/recluster-probe.sqlite \
//! cargo nextest run --test prior_recluster_replay -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;

use mdkb::store::Store;
use mdkb::store::priors::recluster;

/// Group the candidates by cluster before and after the pass, and report what
/// happened to the lessons that name a budget limit.
#[test]
#[ignore = "needs a copy of a real store (see module docs)"]
fn reclustering_a_real_store_collapses_the_budget_lesson() {
    let Ok(path) = std::env::var("MDKB_RECLUSTER_STORE") else {
        eprintln!("MDKB_RECLUSTER_STORE unset — nothing to replay");
        return;
    };
    let mut store = Store::open(&path).expect("the copy must open");

    let clusters_of = |store: &Store| -> BTreeMap<String, Vec<String>> {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT c.cluster_id, k.lesson
                   FROM prior_candidates c
                   LEFT JOIN prior_clusters k ON k.id = c.cluster_id",
            )
            .unwrap();
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })
            .unwrap();
        for row in rows {
            let (id, lesson) = row.unwrap();
            out.entry(id).or_default().push(lesson);
        }
        out
    };
    let is_budget = |l: &String| {
        let l = l.to_lowercase();
        l.contains("budget") || l.contains("limit reached")
    };
    let budget_groups = |map: &BTreeMap<String, Vec<String>>| {
        map.values().filter(|ls| ls.iter().any(is_budget)).count()
    };

    let before = clusters_of(&store);
    let budget_before = budget_groups(&before);
    println!("before: {} clusters hold candidates", before.len());
    println!("        the budget lesson is spread over {budget_before}");

    let report = recluster(store.conn_mut(), 0).expect("the pass must run");

    let after = clusters_of(&store);
    let budget_after = budget_groups(&after);
    println!("after : {} clusters hold candidates", after.len());
    println!("        the budget lesson is spread over {budget_after}");
    println!(
        "moved {} / emptied {} / newly promotable {}",
        report.moved,
        report.emptied.len(),
        report.newly_promotable.len()
    );

    // A group that mixes the budget lesson with something else is the failure
    // this threshold is chosen to avoid, and it is worth more than the count.
    for lessons in after.values().filter(|ls| ls.iter().any(is_budget)) {
        for foreign in lessons.iter().filter(|l| !is_budget(l)) {
            println!("MIXED IN: {foreign}");
        }
    }

    assert_eq!(
        budget_after, 1,
        "the budget lesson must end in exactly one cluster"
    );
    assert!(
        budget_before > budget_after,
        "the pass has to change something, or the store was already merged"
    );
}
