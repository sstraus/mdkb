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
use mdkb::store::priors::{curate_cluster_family, recluster};

/// Story 177: replay the reviewed family on a copy of the store only. These
/// identities are deliberately explicit: curation must not infer equivalence
/// from a looser global cosine threshold.
#[test]
#[ignore = "needs a copy of the TUICommander store"]
fn reclustering_the_ask_only_when_blocked_family() {
    const FAMILY: [&str; 6] = [
        "clu-449bf2aa8924c0cc",
        "clu-252d28552e2cc728",
        "clu-011d16b56648b731",
        "clu-852ced6803b68c33",
        "clu-3bc9d365182d9ec1",
        "clu-7a3afa490c4f1125",
    ];
    let path = std::env::var("MDKB_RECLUSTER_STORE").expect("set copy path under ~/Gits");
    let copy_root = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join("Gits/.tmp");
    assert!(
        std::fs::canonicalize(&path)
            .unwrap()
            .starts_with(std::fs::canonicalize(copy_root).unwrap()),
        "replay must write only to a disposable copy under ~/Gits/.tmp"
    );
    let mut store = Store::open(&path).expect("open copied store");
    mdkb::store::schema::init_schema(store.conn()).expect("migrate copied store");
    let ids: Vec<String> = {
        let mut stmt = store
            .conn()
            .prepare(
                "SELECT c.id FROM prior_candidates c
                 WHERE c.cluster_id IN (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .unwrap();
        stmt.query_map(FAMILY, |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };
    let groups = |store: &Store| -> usize {
        let mut distinct = std::collections::HashSet::new();
        for id in &ids {
            let cluster: String = store
                .conn()
                .query_row(
                    "SELECT cluster_id FROM prior_candidates WHERE id=?1",
                    [id],
                    |row| row.get(0),
                )
                .unwrap();
            distinct.insert(cluster);
        }
        distinct.len()
    };

    let linked = |store: &Store| -> i64 {
        store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM prior_clusters k
             JOIN memory_entries m ON m.id=k.promoted_memory_id
             WHERE k.id IN (?1, ?2, ?3, ?4, ?5, ?6)",
                FAMILY,
                |row| row.get(0),
            )
            .unwrap()
    };
    let before = groups(&store);
    let links_before = linked(&store);
    let automatic = recluster(store.conn_mut(), chrono::Utc::now().timestamp()).unwrap();
    let after_automatic = groups(&store);
    let unrelated_memberships = |store: &Store| -> BTreeMap<String, Option<String>> {
        let mut stmt = store
            .conn()
            .prepare("SELECT id, cluster_id FROM prior_candidates")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .filter(|(id, _)| !ids.contains(id))
            .collect()
    };
    let unrelated_before = unrelated_memberships(&store);
    let curated =
        curate_cluster_family(store.conn(), &FAMILY, chrono::Utc::now().timestamp()).unwrap();
    let after = groups(&store);
    let second = recluster(store.conn_mut(), chrono::Utc::now().timestamp()).unwrap();
    println!(
        "ask-only-when-blocked: {} candidate rows, {before} groups before, {after_automatic} after automatic, {after} after curation; automatic {} moved/{} emptied; curation {} moved/{} emptied; promoted links {links_before}/{}",
        ids.len(),
        automatic.moved,
        automatic.emptied.len(),
        curated.moved,
        curated.emptied.len(),
        linked(&store),
    );
    assert_eq!(
        ids.len(),
        34,
        "the replay copy must contain the measured family"
    );
    assert_eq!(before, 6);
    assert_eq!(after_automatic, 6);
    assert_eq!(after, 1);
    assert_eq!(
        groups(&store),
        1,
        "the next automatic pass must retain curation"
    );
    assert_eq!(second.moved, 0);
    assert_eq!(unrelated_memberships(&store), unrelated_before);
    assert_eq!(links_before, 5);
    assert_eq!(linked(&store), links_before);
}

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
