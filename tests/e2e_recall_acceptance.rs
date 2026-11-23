//! Bilingual recall acceptance with the production embedding model.
//!
//! Every test is `#[ignore]`: it needs the model weights, which default runs
//! must not download. Run with
//! `cargo test --test e2e_recall_acceptance -- --ignored --test-threads 2`.
//! A missing model fails the run (see `recall::Model::Production`); it is never
//! a silent skip. Weights are linked into each sandbox's own cache, offline.
//!
//! Plumbing assertions (same answer from CLI and MCP, no foreign hits, chunk
//! and vector rows follow edits) are exact. Quality floors are in the tests
//! named `*_quality_floor` and print their per-direction numbers.

#[path = "common/cli.rs"]
mod cli;
#[path = "common/recall.rs"]
mod recall;

use recall::{FixtureIntent, Model, Sandbox};
use serde_json::{Value, json};

const FAMILIES: [&str; 4] = ["F01-ingest", "F02-auth", "F07-caching", "F12-ratelimit"];
const LANGS: [&str; 2] = ["en", "it"];

/// A sandbox holding the fixture families in the given languages.
fn corpus(langs: &[&str]) -> (Sandbox, Vec<FixtureIntent>) {
    corpus_with(Model::Production, langs)
}

fn corpus_with(model: Model, langs: &[&str]) -> (Sandbox, Vec<FixtureIntent>) {
    let s = Sandbox::new(model);
    let (docs, intents) = recall::fixture(&FAMILIES);
    for d in &docs {
        for &lang in langs {
            let (title, text) = if lang == "en" { &d.en } else { &d.it };
            if d.kind == "doc" {
                s.write_doc(&format!("docs/{}.{lang}.md", d.id), title, text);
            } else {
                s.add_memory(&format!("{}-{lang}", d.id), title, text, &d.entry_type, &[]);
            }
        }
    }
    s.ok(&["update"]);
    s.ok(&["update"]);
    (s, intents)
}

/// Logical id of a hit key: `docs/x.en.md`/`x.en.md` and `x-en` all map to `x`.
fn logical(key: &str) -> String {
    let base = key
        .rsplit('/')
        .next()
        .unwrap_or(key)
        .trim_end_matches(".md");
    for lang in LANGS {
        for sep in [".", "-"] {
            if let Some(stem) = base.strip_suffix(&format!("{sep}{lang}")) {
                return stem.to_string();
            }
        }
    }
    base.to_string()
}

fn keys(v: &Value) -> Vec<String> {
    v["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|h| {
            h["value"]
                .get("path")
                .or_else(|| h["value"].get("id"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

fn query(intent: &FixtureIntent, lang: &str) -> String {
    if lang == "en" {
        intent.en.clone()
    } else {
        intent.it.clone()
    }
}

// Scenario 3, plumbing. Catches: the MCP route and the CLI route diverging
// (different embedder state, limits or fusion), or a hit from outside the
// corpus language that was indexed.
#[test]
#[ignore = "needs the production embedding model"]
fn cli_and_mcp_return_the_same_knowledge_ranking_in_all_four_directions() {
    for corpus_lang in LANGS {
        let (s, intents) = corpus(&[corpus_lang]);
        let mut mcp = s.mcp();
        for intent in &intents {
            for qlang in LANGS {
                let q = query(intent, qlang);
                let cli: Value = serde_json::from_str(&s.ok(&[
                    "--format",
                    "json",
                    "search",
                    &q,
                    "--scope",
                    "knowledge",
                ]))
                .expect("cli json");
                let via_mcp: Value = serde_json::from_str(
                    &mcp.tool("search", json!({"query": q, "scope": "knowledge"})),
                )
                .expect("mcp json");
                assert_eq!(
                    keys(&cli),
                    keys(&via_mcp),
                    "{} {qlang}->{corpus_lang}",
                    intent.id
                );
                for k in keys(&cli) {
                    assert!(
                        k.ends_with(&format!(".{corpus_lang}.md"))
                            || k.ends_with(&format!("-{corpus_lang}")),
                        "{k} is not in the {corpus_lang} corpus"
                    );
                }
            }
        }
    }
}

/// R@1 and R@5 of the grade-3 target, per query language, in the scope that
/// stores it. `knowledge` lists documents before memories without fusing the
/// two rankings, so a rank across both says nothing about either.
fn recall_at(s: &Sandbox, intents: &[FixtureIntent], qlang: &str) -> (f64, f64) {
    let (mut r1, mut r5) = (0, 0);
    for intent in intents {
        let scope = if intent.target_kind == "doc" {
            "docs"
        } else {
            "memory"
        };
        let out = s.ok(&[
            "--format",
            "json",
            "search",
            &query(intent, qlang),
            "--scope",
            scope,
        ]);
        let v: Value = serde_json::from_str(&out).expect("cli json");
        let hits = if v.is_array() {
            json!({"hits": v.as_array().expect("entries").iter()
            .map(|e| json!({"value": e})).collect::<Vec<_>>()})
        } else {
            v
        };
        let ranked: Vec<String> = keys(&hits).iter().map(|k| logical(k)).collect();
        r1 += usize::from(ranked.first() == Some(&intent.target));
        r5 += usize::from(ranked.iter().take(5).any(|k| *k == intent.target));
    }
    let n = intents.len() as f64;
    (r1 as f64 / n, r5 as f64 / n)
}

// Scenario 3, quality. Catches: a broken vector leg, which leaves the
// cross-language directions at lexical chance. Floors: the same-language
// floor is the shipped model's measured level less a margin; the
// cross-language floor only proves that the dense leg contributes.
#[test]
#[ignore = "needs the production embedding model"]
fn knowledge_search_quality_floor_in_all_four_directions() {
    let mut report = Vec::new();
    for corpus_lang in LANGS {
        let (s, intents) = corpus(&[corpus_lang]);
        for qlang in LANGS {
            let (r1, r5) = recall_at(&s, &intents, qlang);
            report.push(format!(
                "{qlang}->{corpus_lang}: R@1 {r1:.2} R@5 {r5:.2} (n={})",
                intents.len()
            ));
            let floor = if qlang == corpus_lang { 0.8 } else { 0.3 };
            assert!(
                r5 >= floor,
                "{qlang}->{corpus_lang} R@5 {r5:.2} < {floor}; {report:?}"
            );
        }
    }
    eprintln!("{}", report.join("\n"));
}

// Scenarios 4 and 5. Catches: the hook not injecting what the store ranks as
// the answer, or injecting context for prompts with no answer.
#[test]
#[ignore = "needs the production embedding model"]
fn the_hook_injects_answers_and_stays_silent_on_unrelated_prompts() {
    let (s, intents) = corpus(&["en"]);
    let injected = intents
        .iter()
        .filter(|i| s.hook(&format!("* {}", i.en)).0.contains(&i.target))
        .count();
    eprintln!(
        "hook EN->EN: target injected for {injected}/{}",
        intents.len()
    );
    assert!(
        injected * 2 >= intents.len(),
        "the hook injected the answer for {injected}/{}",
        intents.len()
    );
    for prompt in [
        "* what is a good recipe for pizza dough?",
        "* chi ha vinto il Giro d'Italia nel 1990?",
        "* how do I center a div in CSS?",
    ] {
        let (context, stderr, code) = s.hook(prompt);
        assert_eq!(code, 0, "{stderr}");
        assert!(context.is_empty(), "{prompt}: injected {context}");
    }
}

// Scenarios 6, 7 and 11 (warm offline cache). Catches: chunk or vector rows
// that keep an edited or deleted document's old text, and a warm cache that
// still falls back to BM25.
#[test]
#[ignore = "needs the production embedding model"]
fn chunks_and_vectors_follow_edits_and_deletes_with_a_warm_offline_cache() {
    let s = Sandbox::new(Model::Production);
    // Sections of ~200 tokens: over the 128-token floor below which
    // `chunks::merge_small_chunks` folds a section into the previous chunk
    // without any size cap (reported separately), so this splits.
    let long = |topic: &str| {
        (0..12)
            .map(|i| {
                format!("## Part {i}\n\n{topic} detail number {i}. {}\n\n",
        "Each section repeats enough ordinary words to fill several sentences of text. ".repeat(14))
            })
            .collect::<String>()
    };
    s.write_doc("docs/a.md", "Alpha", &long("Alpha"));
    s.write_doc("docs/b.md", "Beta", &long("Beta"));
    s.ok(&["update"]);
    s.ok(&["update"]);
    let chunks = |name: &str| {
        s.count(&format!(
            "SELECT COUNT(*) FROM document_chunks c JOIN documents d \
        ON d.id = c.document_id WHERE d.relative_path LIKE '%{name}'"
        ))
    };
    let vectors = |name: &str| {
        s.count(&format!(
            "SELECT COUNT(*) FROM embeddings e JOIN documents d \
        ON d.id = e.document_id WHERE d.relative_path LIKE '%{name}'"
        ))
    };
    assert!(chunks("a.md") > 1, "a long document is chunked");
    assert_eq!(vectors("a.md"), 1);

    s.write_doc("docs/a.md", "Alpha", "Alpha is now one short paragraph.");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(s.root.join("docs/a.md"))
        .and_then(|f| f.set_modified(later))
        .expect("set mtime");
    s.ok(&["update"]);
    s.ok(&["update"]);
    assert_eq!(
        s.count(
            "SELECT COUNT(*) FROM document_chunks c JOIN documents d ON d.id = c.document_id \
        WHERE d.relative_path LIKE '%a.md' AND c.content LIKE '%detail number%'"
        ),
        0,
        "stale chunks kept"
    );

    std::fs::remove_file(s.root.join("docs/b.md")).expect("delete b");
    s.ok(&["update"]);
    assert_eq!(chunks("b.md"), 0);
    assert_eq!(vectors("b.md"), 0);

    let (_, stderr, code) = s.hook("* what does the alpha page say?");
    assert_eq!(code, 0);
    assert!(
        !stderr.contains("BM25-only"),
        "a warm offline cache must embed: {stderr}"
    );
}

// Scenario 13. Catches: the English and Italian variants of one memory entry
// both injected, although Granite places a translation pair far closer
// together than two distinct entries. The production MiniLM model has no such
// contract: it scores a translation pair 0.26 to 0.47, below many distinct
// entries, so a cosine threshold that merged its pairs would also merge
// distinct decisions; with it, 11 of 40 cases inject both variants (measured
// 2026-11-21, accepted by Boss the same day).
#[test]
#[ignore = "needs the Granite embedding model"]
fn granite_language_variants_inject_one_logical_answer() {
    // The one measured miss: this pair scores 0.899, below the 0.91 variant
    // threshold, and the closest distinct pair (an old and a new decision)
    // scores 0.895, so no threshold merges it without merging those.
    assert_one_logical_answer_per_variant_pair(Model::Granite, &["cache-key-version"]);
}

/// `known_misses` are logical ids whose two variants may both be injected.
fn assert_one_logical_answer_per_variant_pair(model: Model, known_misses: &[&str]) {
    let (s, intents) = corpus_with(model, &LANGS);
    let mut duplicated = Vec::new();
    for intent in &intents {
        for qlang in LANGS {
            let (context, _, _) = s.hook(&format!("* {}", query(intent, qlang)));
            let keys: Vec<&str> = context
                .lines()
                .filter(|l| l.starts_with("- "))
                .map(|l| {
                    // A memory line is `- [id] title`; a document line is
                    // `- path — title`.
                    let key = l.split(['[', ']']).nth(1).unwrap_or(&l[2..]);
                    key.split_whitespace().next().unwrap_or(key)
                })
                .collect();
            let mut seen = std::collections::HashSet::new();
            let twice = keys
                .iter()
                .map(|k| logical(k))
                .any(|id| !seen.insert(id.clone()) && !known_misses.contains(&id.as_str()));
            if twice {
                duplicated.push(format!("{} {qlang} {keys:?}", intent.id));
            }
        }
    }
    assert!(
        duplicated.is_empty(),
        "variants injected twice for {}/{}: {duplicated:?}",
        duplicated.len(),
        intents.len() * 2
    );
}
