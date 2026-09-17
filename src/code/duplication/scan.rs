//! Turning candidates into fingerprints, through the cache where it is warm.
//!
//! The complexity filter is two-stage because it has to be. `min_lines` is a
//! predicate SQL can evaluate and is what keeps this from reading the whole
//! repository; `min_nodes` is the filter that actually means something — twenty
//! lines of `match` arms and twenty lines of one expression are not comparable
//! findings — and it does not exist until something has parsed the body. So SQL
//! narrows first, the cache answers for every body seen before, and only what
//! is left is parsed.

use std::collections::HashMap;
use std::path::Path;

use super::body::{body_hash, body_text, structural_simhash};
use super::candidates::DupCandidate;
use super::cluster::Fingerprint;
use super::store::{BodyArtifacts, DupDb};
use crate::code::indexing::pipeline::create_parser;
use crate::code::parsing::language::Language;
use crate::code::parsing::parser::LanguageParser;

/// Shortest span SQL will hand on. Four lines of body under a signature is the
/// smallest thing worth a reader's attention.
pub const MIN_BODY_LINES: u32 = 5;

/// Fewest named AST nodes a body may hold and still be reported.
///
/// The filter that carries the meaning: a three-node accessor is identical to
/// every other accessor ever written, and reporting those buries the findings
/// that matter under hundreds that do not.
pub const MIN_BODY_NODES: u32 = 30;

/// A candidate that survived both filters, with what it was judged on.
#[derive(Debug, Clone, PartialEq)]
pub struct Scanned {
    pub candidate: DupCandidate,
    /// Content-addressed cache key of the body text.
    pub body_hash: String,
    pub simhash: u64,
    pub nodes: u32,
}

impl Scanned {
    /// What the clusterer compares.
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint {
            simhash: self.simhash,
            symbol_id: self.candidate.id,
            owner_name: self.candidate.owner_name.clone(),
        }
    }
}

/// Fingerprint every candidate, reading each file at most once and parsing only
/// the bodies the cache has not seen.
///
/// `read_file` is given a repository-relative path and returns its contents;
/// `None` means the file is gone, which is a stale index and not an error worth
/// aborting a whole scan over.
///
/// Every body is written back through [`DupDb::upsert_structural`] whether it
/// was a hit or a miss, so `last_seen` advances for bodies that are still live
/// and [`DupDb::gc`] does not reclaim them. The upsert leaves the embedding
/// column alone, so a hit costs a row update and never an ONNX pass.
pub fn fingerprint_candidates(
    candidates: &[DupCandidate],
    dup: &DupDb,
    read_file: &mut dyn FnMut(&str) -> Option<String>,
    min_nodes: u32,
    now: i64,
) -> rusqlite::Result<Vec<Scanned>> {
    let mut parsers: HashMap<Language, Option<Box<dyn LanguageParser>>> = HashMap::new();
    let mut out = Vec::new();
    // The candidate list arrives ordered by file, so one slot holds the file
    // being worked through. A path that failed to read stays recorded, so a
    // missing file is attempted once and not once per symbol in it.
    let mut loaded: Option<(String, Option<String>)> = None;

    for candidate in candidates {
        let path = candidate.file_path.as_str();
        if loaded.as_ref().is_none_or(|(p, _)| p != path) {
            loaded = Some((path.to_string(), read_file(path)));
        }
        let Some((_, Some(source))) = &loaded else {
            continue;
        };
        let Some(body) = body_text(source, candidate.line_start, candidate.line_end) else {
            continue;
        };

        let hash = body_hash(body);
        let cached = dup.get(&hash)?;
        let Some(artifacts) = cached.or_else(|| fingerprint_body(&mut parsers, path, body)) else {
            continue;
        };
        dup.upsert_structural(&hash, artifacts.simhash, artifacts.nodes, now)?;

        if artifacts.nodes < min_nodes {
            continue;
        }
        out.push(Scanned {
            candidate: candidate.clone(),
            body_hash: hash,
            simhash: artifacts.simhash,
            nodes: artifacts.nodes,
        });
    }
    Ok(out)
}

/// Parse one body and fingerprint it. `None` when the language is unsupported
/// or its parser will not build.
///
/// The body text is parsed on its own rather than located inside the file's
/// tree, and that is the point: the cache is keyed on the body text, so the
/// fingerprint has to be a function of that text and nothing else. A subtree
/// taken from the enclosing file would carry its context into the hash, and the
/// same function copied into another file would no longer be a cache hit.
fn fingerprint_body(
    parsers: &mut HashMap<Language, Option<Box<dyn LanguageParser>>>,
    path: &str,
    body: &str,
) -> Option<BodyArtifacts> {
    let language = Language::from_path(Path::new(path))?;
    let parser = parsers
        .entry(language)
        .or_insert_with(|| create_parser(language))
        .as_mut()?;
    let tree = parser.tree(body)?;
    let (simhash, nodes) = structural_simhash(tree.root_node());
    Some(BodyArtifacts {
        simhash,
        nodes,
        embedding: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body long enough to clear `MIN_BODY_NODES`, parameterised so two of
    /// them differ only in names.
    fn rust_body(name: &str, acc: &str) -> String {
        format!(
            "fn {name}(items: &[u32]) -> u32 {{\n\
             \x20   let mut {acc} = 0;\n\
             \x20   for item in items {{\n\
             \x20       if item % 2 == 0 {{\n\
             \x20           {acc} += item * 2;\n\
             \x20       }} else {{\n\
             \x20           {acc} -= item;\n\
             \x20       }}\n\
             \x20   }}\n\
             \x20   {acc}\n\
             }}"
        )
    }

    fn candidate(id: i64, name: &str, path: &str, start: u32, end: u32) -> DupCandidate {
        DupCandidate {
            id,
            name: name.to_string(),
            file_path: path.to_string(),
            module_path: None,
            kind: "Function".to_string(),
            language: Some("rust".to_string()),
            signature: None,
            owner_name: None,
            visibility: crate::code::symbol::Visibility::Private,
            line_start: start,
            line_end: end,
        }
    }

    /// A reader over an in-memory file set that counts what it was asked for.
    struct Files {
        files: HashMap<String, String>,
        reads: Vec<String>,
    }

    impl Files {
        fn new(files: &[(&str, &str)]) -> Self {
            Self {
                files: files
                    .iter()
                    .map(|(p, c)| ((*p).to_string(), (*c).to_string()))
                    .collect(),
                reads: Vec::new(),
            }
        }

        fn reader(&mut self) -> impl FnMut(&str) -> Option<String> + '_ {
            |path: &str| {
                self.reads.push(path.to_string());
                self.files.get(path).cloned()
            }
        }
    }

    #[test]
    fn a_cold_scan_parses_and_caches_every_body() {
        let source = rust_body("total", "sum");
        let end = source.lines().count() as u32 - 1;
        let mut files = Files::new(&[("src/a.rs", &source)]);
        let dup = DupDb::in_memory().unwrap();

        let scanned = fingerprint_candidates(
            &[candidate(1, "total", "src/a.rs", 0, end)],
            &dup,
            &mut files.reader(),
            MIN_BODY_NODES,
            100,
        )
        .unwrap();

        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].nodes >= MIN_BODY_NODES, "{}", scanned[0].nodes);
        assert_eq!(dup.count().unwrap(), 1, "the body is now cached");
        assert_eq!(
            dup.get(&scanned[0].body_hash).unwrap().unwrap().simhash,
            scanned[0].simhash
        );
    }

    /// The two-stage filter's warm path must select exactly what the cold path
    /// selected. Proven by making the cold path impossible on the second run:
    /// the same body under an extension no parser handles can only be answered
    /// from the cache, because the cache is keyed on the body, not the file.
    #[test]
    fn the_warm_path_selects_the_same_candidates_as_the_cold_path() {
        let source = rust_body("total", "sum");
        let end = source.lines().count() as u32 - 1;
        let dup = DupDb::in_memory().unwrap();

        let mut cold_files = Files::new(&[("src/a.rs", &source)]);
        let cold = fingerprint_candidates(
            &[candidate(1, "total", "src/a.rs", 0, end)],
            &dup,
            &mut cold_files.reader(),
            MIN_BODY_NODES,
            100,
        )
        .unwrap();

        let mut warm_files = Files::new(&[("src/a.unparseable", &source)]);
        let warm = fingerprint_candidates(
            &[candidate(1, "total", "src/a.unparseable", 0, end)],
            &dup,
            &mut warm_files.reader(),
            MIN_BODY_NODES,
            100,
        )
        .unwrap();

        assert_eq!(cold.len(), 1);
        assert_eq!(warm.len(), 1, "the cache answered where no parser could");
        assert_eq!(cold[0].body_hash, warm[0].body_hash);
        assert_eq!(cold[0].simhash, warm[0].simhash);
        assert_eq!(cold[0].nodes, warm[0].nodes);
    }

    #[test]
    fn a_warm_hit_refreshes_last_seen_so_gc_keeps_the_body() {
        let source = rust_body("total", "sum");
        let end = source.lines().count() as u32 - 1;
        let dup = DupDb::in_memory().unwrap();
        let mut files = Files::new(&[("src/a.rs", &source)]);
        let candidates = [candidate(1, "total", "src/a.rs", 0, end)];

        let first =
            fingerprint_candidates(&candidates, &dup, &mut files.reader(), MIN_BODY_NODES, 100)
                .unwrap();
        // An embedding the second pass must not cost again.
        dup.set_embedding(&first[0].body_hash, &[0.5, 0.25])
            .unwrap();

        let second =
            fingerprint_candidates(&candidates, &dup, &mut files.reader(), MIN_BODY_NODES, 999)
                .unwrap();

        let cached = dup.get(&second[0].body_hash).unwrap().unwrap();
        assert_eq!(
            cached.embedding,
            Some(vec![0.5, 0.25]),
            "a second pass must not discard the expensive column"
        );
        assert_eq!(dup.gc(&[second[0].body_hash.clone()]).unwrap(), 0);
    }

    #[test]
    fn each_file_is_read_once_however_many_candidates_it_holds() {
        let a = rust_body("total", "sum");
        let b = rust_body("aggregate", "acc");
        let source = format!("{a}\n{b}");
        let a_end = a.lines().count() as u32 - 1;
        let b_start = a_end + 1;
        let b_end = b_start + b.lines().count() as u32 - 1;
        let mut files = Files::new(&[("src/a.rs", &source)]);
        let dup = DupDb::in_memory().unwrap();

        fingerprint_candidates(
            &[
                candidate(1, "total", "src/a.rs", 0, a_end),
                candidate(2, "aggregate", "src/a.rs", b_start, b_end),
            ],
            &dup,
            &mut files.reader(),
            MIN_BODY_NODES,
            100,
        )
        .unwrap();

        assert_eq!(files.reads, ["src/a.rs"], "one read, not one per symbol");
    }

    #[test]
    fn a_file_that_cannot_be_read_is_attempted_once_and_skipped() {
        // A stale index against a deleted file. Retrying per symbol would be a
        // syscall per symbol for nothing.
        let mut files = Files::new(&[]);
        let dup = DupDb::in_memory().unwrap();

        let scanned = fingerprint_candidates(
            &[
                candidate(1, "a", "src/gone.rs", 0, 20),
                candidate(2, "b", "src/gone.rs", 30, 50),
            ],
            &dup,
            &mut files.reader(),
            MIN_BODY_NODES,
            100,
        )
        .unwrap();

        assert!(scanned.is_empty());
        assert_eq!(files.reads, ["src/gone.rs"]);
    }

    #[test]
    fn a_body_below_the_node_floor_is_dropped_but_still_cached() {
        // Caching it matters: the next scan must not re-parse a body only to
        // reject it again.
        let source = "fn tiny() -> u32 {\n    1\n}";
        let mut files = Files::new(&[("src/a.rs", source)]);
        let dup = DupDb::in_memory().unwrap();

        let scanned = fingerprint_candidates(
            &[candidate(1, "tiny", "src/a.rs", 0, 2)],
            &dup,
            &mut files.reader(),
            MIN_BODY_NODES,
            100,
        )
        .unwrap();

        assert!(scanned.is_empty(), "an accessor is not a finding");
        assert_eq!(dup.count().unwrap(), 1, "but it is remembered");
    }

    #[test]
    fn an_unsupported_language_is_skipped_not_fatal() {
        let mut files = Files::new(&[("README.md", "# not code\nat all\n")]);
        let dup = DupDb::in_memory().unwrap();

        let scanned = fingerprint_candidates(
            &[candidate(1, "x", "README.md", 0, 1)],
            &dup,
            &mut files.reader(),
            1,
            100,
        )
        .unwrap();

        assert!(scanned.is_empty());
        assert_eq!(dup.count().unwrap(), 0, "nothing to cache");
    }

    #[test]
    fn a_range_past_the_end_of_the_file_is_skipped() {
        let mut files = Files::new(&[("src/a.rs", "fn a() {}\n")]);
        let dup = DupDb::in_memory().unwrap();

        let scanned = fingerprint_candidates(
            &[candidate(1, "a", "src/a.rs", 0, 500)],
            &dup,
            &mut files.reader(),
            1,
            100,
        )
        .unwrap();

        assert!(scanned.is_empty(), "a stale range yields no body");
    }

    /// The end-to-end claim of the story: the same logic written twice with
    /// every name changed comes back as one cluster, with no embedding computed.
    #[test]
    fn two_renamed_copies_of_one_function_cluster_without_an_embedding() {
        use super::super::cluster::cluster;
        use crate::code::duplication::body::SIMHASH_HAMMING_THRESHOLD;

        let a = rust_body("total", "sum");
        let b = rust_body("aggregate", "acc");
        let a_end = a.lines().count() as u32 - 1;
        let mut files = Files::new(&[("src/a.rs", &a), ("src/b.rs", &b)]);
        let dup = DupDb::in_memory().unwrap();

        let scanned = fingerprint_candidates(
            &[
                candidate(1, "total", "src/a.rs", 0, a_end),
                candidate(2, "aggregate", "src/b.rs", 0, a_end),
            ],
            &dup,
            &mut files.reader(),
            MIN_BODY_NODES,
            100,
        )
        .unwrap();

        assert_eq!(scanned.len(), 2);
        assert_eq!(
            scanned[0].simhash, scanned[1].simhash,
            "renaming changes no node kind"
        );

        let fps: Vec<_> = scanned.iter().map(Scanned::fingerprint).collect();
        let no_calls = std::collections::HashSet::new();
        let clusters = cluster(&fps, SIMHASH_HAMMING_THRESHOLD, &no_calls);

        assert_eq!(clusters, vec![vec![0, 1]]);
        for s in &scanned {
            assert_eq!(
                dup.get(&s.body_hash).unwrap().unwrap().embedding,
                None,
                "the structural pass computes no embedding"
            );
        }
    }
}
