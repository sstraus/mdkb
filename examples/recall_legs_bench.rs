//! Per-leg timing of the UserPromptSubmit recall search (story 199-77a9).
//!
//!   cargo run --release --example recall_legs_bench -- <project-root> <prompts.json>
//!
//! Opens the store read-only and times, for every prompt, the legs the hook's
//! search closure runs: the embed, the memory hybrid search (injection query
//! and the wider ledger query), and the docs hybrid search split into its FTS
//! leg, its chunk-vector leg (`vec_chunks` KNN), its doc-vector leg
//! (`vec_documents` KNN) and the whole `hybrid_search_fts_scored`.

use std::time::Instant;

use mdkb::core::Context;
use mdkb::domain::SearchQuery;
use mdkb::store::{memory, search, vectors};
use zerocopy::AsBytes;

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let root = &args[1];
    let prompts: Vec<String> =
        serde_json::from_str(&std::fs::read_to_string(&args[2]).unwrap()).unwrap();
    let rw = std::env::var("RW").is_ok();
    let ctx = if rw {
        Context::open(root).unwrap()
    } else {
        Context::open_read_only(root).unwrap()
    };
    let cfg = mdkb::Config::load_or_default(&ctx.config_path);
    let svc = mdkb::llm::get_cached_service().unwrap();
    let _ = svc.embed_query("warmup").unwrap();
    let limit = cfg.hooks.recall_limit.max(1);
    let docs_pool = cfg.hooks.recall_docs_limit * 4;
    println!(
        "{:>3} {:>7} {:>8} {:>8} | {:>8} {:>8} {:>8} {:>8} | {:>8}",
        "#", "embed", "mem", "mem*3", "docs_fts", "vec_chnk", "vec_doc", "docs_all", "total"
    );
    for pass in 0..2 {
        println!("-- pass {pass}");
        for (i, prompt) in prompts.iter().enumerate() {
            let t = Instant::now();
            let emb = svc.embed_query(prompt).unwrap();
            let embed = ms(t);
            let q = search::build_recall_query(prompt).unwrap();
            let t_all = Instant::now();

            let t = Instant::now();
            memory::search_entries_hybrid_fts(
                &ctx.conn, &q, prompt, Some(&emb), limit, None, &cfg.search.memory,
            )
            .unwrap();
            let mem = ms(t);
            let t = Instant::now();
            memory::search_entries_hybrid_fts(
                &ctx.conn, &q, prompt, Some(&emb), limit * 3, None, &cfg.search.memory,
            )
            .unwrap();
            let mem3 = ms(t);

            let tel = if rw {
                let t = Instant::now();
                let ev = mdkb::store::stats::QueryEvent {
                    query_hash: format!("bench{i}"),
                    query_text: String::new(),
                    search_type: "recall".into(),
                    result_count: 1,
                    latency_ms: 1,
                    top_score: None,
                    session_id: None,
                };
                mdkb::store::stats::record_query_event(&ctx.conn, &ev, 30).unwrap();
                ms(t)
            } else {
                0.0
            };
            print!("tel={tel:.1} ");
            let t = Instant::now();
            let sq = SearchQuery {
                limit: docs_pool * 2,
                ..Default::default()
            };
            search::search_fts(&ctx.conn, &q, &sq).unwrap();
            let fts = ms(t);

            let bytes = emb.as_bytes();
            let t = Instant::now();
            let n: usize = ctx
                .conn
                .prepare("SELECT chunk_id, distance FROM vec_chunks WHERE embedding MATCH ?1 ORDER BY distance LIMIT ?2")
                .unwrap()
                .query_map(rusqlite::params![bytes, (docs_pool * 2 * 5) as i64], |r| r.get::<_, i64>(0))
                .unwrap()
                .count();
            let vchunk = ms(t);
            let _ = n;
            let t = Instant::now();
            vectors::vector_search(&ctx.conn, &emb, docs_pool * 2 * 5).unwrap();
            let vdoc = ms(t);

            let t = Instant::now();
            mdkb::core::search::hybrid_search_fts_scored(
                &ctx, &q, Some(&emb), docs_pool, None, false,
            )
            .unwrap();
            let docs = ms(t);
            println!(
                "{i:>3} {embed:>7.1} {mem:>8.1} {mem3:>8.1} | {fts:>8.1} {vchunk:>8.1} {vdoc:>8.1} {docs:>8.1} | {:>8.1}",
                ms(t_all)
            );
        }
    }
}
