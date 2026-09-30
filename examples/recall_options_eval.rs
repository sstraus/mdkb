//! Raw scores for the recall-gating options evaluation
//! (`docs/recall-options-eval.md`). Same data directory and query sets as
//! `multilingual_eval`; every gate is computed offline from what this prints.
//!
//!   FASTEMBED_CACHE_DIR=<dir> cargo run --release --example recall_options_eval -- \
//!       embed <model> <data-dir> <memory-recall.json>
//!   ... -- lex <data-dir> <memory-recall.json>
//!   ... -- rerank <reranker> <data-dir> <memory-recall.json> <pool.json>
//!
//! `embed` prints the top-50 cosines and the mean and standard deviation over the
//! whole pool for every query; `lex` the top-50 BM25 hits of the production
//! recall expression (`build_recall_query`, FTS5 `porter unicode61`); `rerank`
//! the cross-encoder score of every candidate listed in `pool.json` plus its
//! per-prompt latency on real production chunks. `<data-dir>/translations.json`
//! (optional) adds machine-translated query sets: `{name: {"pos": [..], "neg": [..]}}`.

use std::collections::BTreeMap;
use std::time::Instant;

use fastembed::{
    EmbeddingModel, InitOptions, RerankInitOptions, RerankerModel, TextEmbedding, TextRerank,
};
use mdkb::store::search::build_recall_query;
use serde_json::{Map, Value, json};

const TOP_N: usize = 50;

fn rusage() -> (f64, u64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + f64::from(t.tv_usec) / 1e6;
    // macOS reports ru_maxrss in bytes.
    (tv(ru.ru_utime) + tv(ru.ru_stime), ru.ru_maxrss as u64)
}

fn quantiles(mut ms: Vec<f64>) -> Value {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pct = |p: f64| ms[(((ms.len() - 1) as f64) * p).round() as usize];
    json!({"p50_ms": pct(0.5), "p95_ms": pct(0.95), "n": ms.len()})
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn read(path: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The two pools and every query set, by name. Sets ending in `_pos`/`_neg` run
/// against the Italian-set corpus, except `en_pos`/`en_neg` (the held-out
/// memory fixture).
struct Data {
    corpus: Vec<(String, String, String)>,   // id, file, text
    memories: Vec<(String, String, String)>, // id, "title content", tags
    sets: BTreeMap<String, Vec<String>>,
    sample_chunks: Vec<String>,
}

fn load(data: &str, fixture: &str) -> Data {
    let corpus = read(&format!("{data}/corpus.json"))
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let s = |k: &str| c[k].as_str().unwrap().to_string();
            (s("id"), s("file"), s("text"))
        })
        .collect();
    let it = read(&format!("{data}/it_set.json"));
    let fix = read(fixture);
    let memories = fix["memories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let s = |k: &str| m[k].as_str().unwrap().to_string();
            (
                s("id"),
                format!("{} {}", s("title"), s("content")),
                strings(&m["tags"]).join(" "),
            )
        })
        .collect();
    let pairs = it["pairs"].as_array().unwrap();
    let field = |k: &str| -> Vec<String> {
        pairs
            .iter()
            .map(|p| p[k].as_str().unwrap().to_string())
            .collect()
    };
    let mut sets = BTreeMap::new();
    sets.insert("it_pos".into(), field("q"));
    sets.insert("it_neg".into(), strings(&it["negs"]));
    sets.insert("tr_pos".into(), field("en"));
    sets.insert("tr_neg".into(), strings(&it["negs_en"]));
    sets.insert(
        "en_pos".into(),
        fix["recall"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["query"].as_str().unwrap().to_string())
            .collect(),
    );
    sets.insert("en_neg".into(), strings(&fix["negatives"]));
    if let Ok(text) = std::fs::read_to_string(format!("{data}/translations.json")) {
        let tr: Value = serde_json::from_str(&text).unwrap();
        for (name, v) in tr.as_object().unwrap() {
            sets.insert(format!("{name}_pos"), strings(&v["pos"]));
            sets.insert(format!("{name}_neg"), strings(&v["neg"]));
        }
    }
    let sample = read(&format!("{data}/sample.json"));
    Data {
        corpus,
        memories,
        sets,
        sample_chunks: strings(&sample["chunks"]),
    }
}

fn is_en(set: &str) -> bool {
    set.starts_with("en_")
}

fn embedding_spec(key: &str) -> (EmbeddingModel, &'static str, &'static str) {
    match key {
        "minilm-l6" => (EmbeddingModel::AllMiniLML6V2, "", ""),
        "e5-small" => (EmbeddingModel::MultilingualE5Small, "query: ", "passage: "),
        "e5-large" => (EmbeddingModel::MultilingualE5Large, "query: ", "passage: "),
        other => panic!("unknown model key {other}"),
    }
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn embed(cmd_model: &str, d: &Data) -> Value {
    let (model_id, qp, pp) = embedding_spec(cmd_model);
    let cache = std::env::var("FASTEMBED_CACHE_DIR").expect("set FASTEMBED_CACHE_DIR");
    let t = Instant::now();
    let model = TextEmbedding::try_new(
        InitOptions::new(model_id)
            .with_cache_dir(cache.into())
            .with_show_download_progress(false),
    )
    .unwrap();
    let load_ms = t.elapsed().as_secs_f64() * 1e3;
    let (_, rss_loaded) = rusage();
    let embed_all = |texts: Vec<String>| -> Vec<Vec<f32>> {
        model
            .embed(texts, Some(32))
            .unwrap()
            .into_iter()
            .map(normalize)
            .collect()
    };
    let docs = embed_all(
        d.corpus
            .iter()
            .map(|(_, _, t)| format!("{pp}{t}"))
            .collect(),
    );
    let mems = embed_all(
        d.memories
            .iter()
            .map(|(_, t, _)| format!("{pp}{t}"))
            .collect(),
    );
    let mut out = Map::new();
    for (set, queries) in &d.sets {
        let (pool, ids): (&[Vec<f32>], Vec<&str>) = if is_en(set) {
            (&mems, d.memories.iter().map(|m| m.0.as_str()).collect())
        } else {
            (&docs, d.corpus.iter().map(|c| c.0.as_str()).collect())
        };
        let q = embed_all(queries.iter().map(|q| format!("{qp}{q}")).collect());
        let rows: Vec<Value> = q
            .iter()
            .map(|qv| {
                let mut s: Vec<(usize, f32)> = pool
                    .iter()
                    .enumerate()
                    .map(|(i, dv)| (i, qv.iter().zip(dv).map(|(a, b)| a * b).sum()))
                    .collect();
                let n = s.len() as f32;
                let mean = s.iter().map(|x| x.1).sum::<f32>() / n;
                let std = (s.iter().map(|x| (x.1 - mean).powi(2)).sum::<f32>() / n).sqrt();
                s.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                let top: Vec<Value> = s
                    .iter()
                    .take(TOP_N)
                    .map(|(i, c)| json!([ids[*i], c]))
                    .collect();
                json!({"top": top, "mean": mean, "std": std})
            })
            .collect();
        out.insert(set.clone(), Value::Array(rows));
    }
    let (_, rss_peak) = rusage();
    json!({"model": cmd_model, "load_ms": load_ms, "rss_loaded_bytes": rss_loaded,
           "rss_peak_bytes": rss_peak, "sets": out})
}

/// BM25 over the production recall expression. The Italian-set corpus is indexed
/// per chunk (title = file) because every other score in this eval is per chunk;
/// memories are indexed with the columns of `memory_fts`.
fn lex(d: &Data) -> Value {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE VIRTUAL TABLE docs USING fts5(id UNINDEXED, title, body, tokenize = 'porter unicode61');
         CREATE VIRTUAL TABLE mems USING fts5(id, title, content, tags, tokenize = 'porter unicode61');",
    )
    .unwrap();
    for (id, file, text) in &d.corpus {
        conn.execute(
            "INSERT INTO docs(id, title, body) VALUES (?1, ?2, ?3)",
            [id, file, text],
        )
        .unwrap();
    }
    for (id, text, tags) in &d.memories {
        conn.execute(
            "INSERT INTO mems(id, title, content, tags) VALUES (?1, '', ?2, ?3)",
            [id, text, tags],
        )
        .unwrap();
    }
    let mut out = Map::new();
    let mut ms = Vec::new();
    for (set, queries) in &d.sets {
        let table = if is_en(set) { "mems" } else { "docs" };
        let sql = format!(
            "SELECT id, bm25({table}) FROM {table} WHERE {table} MATCH ?1 ORDER BY bm25({table}) LIMIT {TOP_N}"
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let rows: Vec<Value> = queries
            .iter()
            .map(|q| {
                let t = Instant::now();
                let top: Vec<Value> = match build_recall_query(q) {
                    None => Vec::new(),
                    Some(expr) => stmt
                        .query_map([expr], |r| {
                            // bm25() is lower-is-better; negate so every score here ranks descending.
                            Ok(json!([r.get::<_, String>(0)?, -r.get::<_, f64>(1)?]))
                        })
                        .unwrap()
                        .map(|r| r.unwrap())
                        .collect(),
                };
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                json!({"top": top})
            })
            .collect();
        out.insert(set.clone(), Value::Array(rows));
    }
    json!({"model": "bm25", "latency": quantiles(ms), "sets": out})
}

fn reranker(key: &str) -> RerankerModel {
    match key {
        "jina-v2-ml" => RerankerModel::JINARerankerV2BaseMultiligual,
        "bge-v2-m3" => RerankerModel::BGERerankerV2M3,
        "bge-base" => RerankerModel::BGERerankerBase,
        other => panic!("unknown reranker key {other}"),
    }
}

/// `pool.json`: `[{"set": name, "i": query index, "cands": [ids]}]`.
fn rerank(key: &str, d: &Data, pool_path: &str) -> Value {
    let cache = std::env::var("FASTEMBED_CACHE_DIR").expect("set FASTEMBED_CACHE_DIR");
    let t = Instant::now();
    let model = TextRerank::try_new(
        RerankInitOptions::new(reranker(key))
            .with_cache_dir(cache.into())
            .with_show_download_progress(false),
    )
    .unwrap();
    let load_ms = t.elapsed().as_secs_f64() * 1e3;
    let (_, rss_loaded) = rusage();
    let text: BTreeMap<&str, &str> = d
        .corpus
        .iter()
        .map(|(id, _, t)| (id.as_str(), t.as_str()))
        .chain(
            d.memories
                .iter()
                .map(|(id, t, _)| (id.as_str(), t.as_str())),
        )
        .collect();

    // Latency on the hook path: one prompt against k real production chunks, warm.
    let prompts = [&d.sets["it_pos"][..], &d.sets["it_neg"][..]].concat();
    let chunks: Vec<&str> = d.sample_chunks.iter().map(String::as_str).collect();
    let time_k = |k: usize| {
        let ms: Vec<f64> = prompts
            .iter()
            .enumerate()
            .map(|(n, p)| {
                let cands: Vec<&str> = (0..k).map(|j| chunks[(n * k + j) % chunks.len()]).collect();
                let t = Instant::now();
                model.rerank(p.as_str(), cands, false, Some(k)).unwrap();
                t.elapsed().as_secs_f64() * 1e3
            })
            .collect();
        quantiles(ms)
    };
    time_k(5);
    let lat5 = time_k(5);
    let lat10 = time_k(10);

    let (cpu0, _) = rusage();
    let mut scores = Map::new();
    for item in read(pool_path).as_array().unwrap() {
        let set = item["set"].as_str().unwrap();
        let i = item["i"].as_u64().unwrap() as usize;
        let ids = strings(&item["cands"]);
        let docs: Vec<&str> = ids.iter().map(|id| text[id.as_str()]).collect();
        let res = model
            .rerank(d.sets[set][i].as_str(), docs, false, Some(16))
            .unwrap();
        let row: Map<String, Value> = res
            .iter()
            .map(|r| (ids[r.index].clone(), json!(r.score)))
            .collect();
        scores.insert(format!("{set}/{i}"), Value::Object(row));
    }
    let (cpu1, rss_peak) = rusage();
    json!({"model": key, "load_ms": load_ms, "rss_loaded_bytes": rss_loaded,
           "rss_peak_bytes": rss_peak, "latency_top5": lat5, "latency_top10": lat10,
           "pool_cpu_s": cpu1 - cpu0, "scores": scores})
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let out = match a[1].as_str() {
        "embed" => embed(&a[2], &load(&a[3], &a[4])),
        "lex" => lex(&load(&a[2], &a[3])),
        "rerank" => rerank(&a[2], &load(&a[3], &a[4]), &a[5]),
        other => panic!("unknown command {other}"),
    };
    println!("{out}");
}
