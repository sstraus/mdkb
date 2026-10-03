//! Measure one embedding model for the multilingual recall evaluation.
//!
//! Prints one JSON object: load time, RSS, warm latency, throughput, and the raw
//! top-k cosines of every query against the corpus, so floors and recall are
//! computed offline (see `docs/multilingual-embedding-eval.md`).
//!
//!   FASTEMBED_CACHE_DIR=<dir> cargo run --release --example multilingual_eval -- \
//!       <model> <data-dir> <memory-recall.json>
//!
//! `<data-dir>` holds `corpus.json`, `it_set.json` and `sample.json`. They stay out
//! of the repository: they quote private prompts and internal documents.
//! CPU and RSS fields are null on platforms without Unix resource usage.

use std::time::Instant;

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use serde_json::{Value, json};

const TOP_K: usize = 10;

struct Spec {
    model: EmbeddingModel,
    query_prefix: &'static str,
    passage_prefix: &'static str,
}

fn spec(key: &str) -> Spec {
    let plain = |model| Spec {
        model,
        query_prefix: "",
        passage_prefix: "",
    };
    let e5 = |model| Spec {
        model,
        query_prefix: "query: ",
        passage_prefix: "passage: ",
    };
    match key {
        "minilm-l6" => plain(EmbeddingModel::AllMiniLML6V2),
        "para-ml-l12-q" => plain(EmbeddingModel::ParaphraseMLMiniLML12V2Q),
        "para-ml-l12" => plain(EmbeddingModel::ParaphraseMLMiniLML12V2),
        "e5-small" => e5(EmbeddingModel::MultilingualE5Small),
        "e5-base" => e5(EmbeddingModel::MultilingualE5Base),
        other => panic!("unknown model key {other}"),
    }
}

#[cfg(unix)]
fn rusage() -> (Option<f64>, Option<u64>) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    // macOS reports ru_maxrss in bytes.
    (
        Some(tv(ru.ru_utime) + tv(ru.ru_stime)),
        Some(ru.ru_maxrss as u64),
    )
}

#[cfg(not(unix))]
fn rusage() -> (Option<f64>, Option<u64>) {
    (None, None)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

fn quantiles(mut ms: Vec<f64>) -> Value {
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    json!({"p50_ms": pct(&ms, 0.5), "p95_ms": pct(&ms, 0.95), "n": ms.len()})
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn prefixed(prefix: &str, texts: &[String]) -> Vec<String> {
    texts.iter().map(|t| format!("{prefix}{t}")).collect()
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter_mut().for_each(|x| *x /= n);
    }
    v
}

fn embed_all(m: &TextEmbedding, texts: Vec<String>) -> Vec<Vec<f32>> {
    m.embed(texts, Some(32))
        .unwrap()
        .into_iter()
        .map(normalize)
        .collect()
}

/// Top-k (doc id, cosine) of every query, best first.
fn rank(queries: &[Vec<f32>], docs: &[Vec<f32>], ids: &[String]) -> Value {
    let rows: Vec<Value> = queries
        .iter()
        .map(|q| {
            let mut s: Vec<(usize, f32)> = docs
                .iter()
                .enumerate()
                .map(|(i, d)| (i, q.iter().zip(d).map(|(a, b)| a * b).sum()))
                .collect();
            s.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            Value::Array(
                s.iter()
                    .take(TOP_K)
                    .map(|(i, c)| json!([ids[*i], c]))
                    .collect(),
            )
        })
        .collect();
    Value::Array(rows)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (key, data, fixture) = (&args[1], &args[2], &args[3]);
    let read = |p: String| -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    };
    let corpus = read(format!("{data}/corpus.json"));
    let it = read(format!("{data}/it_set.json"));
    let sample = read(format!("{data}/sample.json"));
    let fix = read(fixture.clone());
    let sp = spec(key);

    let cache = std::env::var("FASTEMBED_CACHE_DIR").expect("set FASTEMBED_CACHE_DIR");
    let (_, rss_start) = rusage();
    let t = Instant::now();
    let model = TextEmbedding::try_new(
        InitOptions::new(sp.model.clone())
            .with_cache_dir(cache.into())
            .with_show_download_progress(false),
    )
    .unwrap();
    let load_ms = t.elapsed().as_secs_f64() * 1e3;
    let dim = model.embed(vec!["x"], None).unwrap()[0].len();
    let (_, rss_loaded) = rusage();

    let one = |text: String| {
        let t = Instant::now();
        model.embed(vec![text], Some(1)).unwrap();
        t.elapsed().as_secs_f64() * 1e3
    };
    let pairs = it["pairs"].as_array().unwrap();
    let it_q: Vec<String> = pairs
        .iter()
        .map(|p| p["q"].as_str().unwrap().to_string())
        .collect();
    let negs = strings(&it["negs"]);
    let it_queries = prefixed(sp.query_prefix, &[it_q.clone(), negs.clone()].concat());

    // Warm-up, then single-call latency: a prompt (the hook path) and a chunk.
    for q in it_queries.iter().take(5) {
        one(q.clone());
    }
    let prompt_ms: Vec<f64> = (0..3)
        .flat_map(|_| {
            it_queries
                .iter()
                .map(|q| one(q.clone()))
                .collect::<Vec<_>>()
        })
        .collect();
    let chunk_texts = prefixed(sp.passage_prefix, &strings(&sample["chunks"]));
    let chunk_ms: Vec<f64> = chunk_texts
        .iter()
        .take(80)
        .map(|c| one(c.clone()))
        .collect();

    // Reindex throughput: production embeds in batches of 32, chunks and memory entries.
    let mem_texts = prefixed(sp.passage_prefix, &strings(&sample["memory"]));
    let batch = |texts: &[String]| {
        let (cpu0, _) = rusage();
        let t = Instant::now();
        model.embed(texts.to_vec(), Some(32)).unwrap();
        let wall = t.elapsed().as_secs_f64();
        let (cpu1, _) = rusage();
        let cpu = cpu1.zip(cpu0).map(|(end, start)| end - start);
        json!({"texts": texts.len(), "wall_s": wall, "cpu_s": cpu,
               "wall_ms_per_text": wall * 1e3 / texts.len() as f64,
               "cpu_ms_per_text": cpu.map(|s| s * 1e3 / texts.len() as f64)})
    };
    let chunk_batch = batch(&chunk_texts);
    let mem_batch = batch(&mem_texts);

    // Retrieval scores.
    let ids: Vec<String> = corpus
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap().to_string())
        .collect();
    let docs = embed_all(
        &model,
        corpus
            .as_array()
            .unwrap()
            .iter()
            .map(|c| format!("{}{}", sp.passage_prefix, c["text"].as_str().unwrap()))
            .collect(),
    );
    let it_emb = embed_all(&model, it_queries);
    let (pos, neg) = it_emb.split_at(it_q.len());
    let it_pos = rank(pos, &docs, &ids);
    let it_neg = rank(neg, &docs, &ids);
    // English translations of the same prompts: the control for the Italian path.
    let en_texts: Vec<String> = pairs
        .iter()
        .map(|p| p["en"].as_str().unwrap().to_string())
        .chain(strings(&it["negs_en"]))
        .collect();
    let en_tr_emb = embed_all(&model, prefixed(sp.query_prefix, &en_texts));
    let (tpos, tneg) = en_tr_emb.split_at(it_q.len());
    let tr_pos = rank(tpos, &docs, &ids);
    let tr_neg = rank(tneg, &docs, &ids);
    // Cosine of each Italian prompt with its own English translation.
    let pair_cos: Vec<f32> = pos
        .iter()
        .zip(tpos)
        .map(|(a, b)| a.iter().zip(b).map(|(x, y)| x * y).sum())
        .collect();

    // English held-out fixture: memory text is "{title} {content}" as in core/memory.rs.
    let mems = fix["memories"].as_array().unwrap();
    let mem_ids: Vec<String> = mems
        .iter()
        .map(|m| m["id"].as_str().unwrap().to_string())
        .collect();
    let mem_emb = embed_all(
        &model,
        mems.iter()
            .map(|m| {
                format!(
                    "{}{} {}",
                    sp.passage_prefix,
                    m["title"].as_str().unwrap(),
                    m["content"].as_str().unwrap()
                )
            })
            .collect(),
    );
    let en_q = fix["recall"].as_array().unwrap();
    let en_pos_emb = embed_all(
        &model,
        en_q.iter()
            .map(|r| format!("{}{}", sp.query_prefix, r["query"].as_str().unwrap()))
            .collect(),
    );
    let en_neg_emb = embed_all(
        &model,
        strings(&fix["negatives"])
            .into_iter()
            .map(|q| format!("{}{q}", sp.query_prefix))
            .collect(),
    );

    let (cpu_total, rss_peak) = rusage();
    let out = json!({
        "model": key, "dim": dim,
        "load_ms": load_ms, "rss_start_bytes": rss_start, "rss_loaded_bytes": rss_loaded, "rss_peak_bytes": rss_peak,
        "cpu_total_s": cpu_total,
        "prompt_latency": quantiles(prompt_ms),
        "chunk_latency_single": quantiles(chunk_ms),
        "chunk_batch32": chunk_batch, "memory_batch32": mem_batch,
        "it_pos": it_pos, "it_neg": it_neg,
        "tr_pos": tr_pos, "tr_neg": tr_neg, "pair_cos": pair_cos,
        "it_expected": pairs.iter().map(|p| p["expected"].clone()).collect::<Vec<_>>(),
        "en_pos": rank(&en_pos_emb, &mem_emb, &mem_ids),
        "en_expected": en_q.iter().map(|r| r["expected_ids"].clone()).collect::<Vec<_>>(),
        "en_neg": rank(&en_neg_emb, &mem_emb, &mem_ids),
    });
    println!("{out}");
}
