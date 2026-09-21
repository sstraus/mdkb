//! Ordering the findings, and writing them down.
//!
//! A flat list of similar pairs is noise. What a reader can act on is a short
//! list where the first entry is the one worth fixing, so the ranking has to
//! encode why duplication costs anything: a copy in another module is one
//! nobody will find when they fix the original, and a copy behind a `pub`
//! signature is one other people are already calling.
//!
//! The cluster identity is deliberately built from names, not ids. Symbol ids
//! do not survive a reparse — `split_by_reuse` reassigns them — so a hash over
//! ids would change under the user, and the ignore-list keyed on it would
//! forget every decision they had made.

use std::collections::BTreeSet;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::candidates::DupCandidate;
use crate::code::parsing::language::Language;
use crate::code::symbol::Visibility;

/// Lines of a body the report prints before eliding the rest.
///
/// A 200-line duplicated function pasted twice is not evidence, it is a wall.
/// The head of a body is enough to recognise it; the `file:line` above it is
/// how the reader gets the rest.
const MAX_SNIPPET_LINES: usize = 20;

/// What made a cluster a finding.
///
/// Kept apart rather than folded into one number: hamming bits and cosine are
/// different scales, and printing them in one column would invite a reader to
/// compare 12 against 0.84.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Evidence {
    /// The bodies have the same shape — within `hamming` bits of each other,
    /// with identifiers and literals ignored.
    Structural { hamming: u32 },
    /// The bodies mean the same thing — cosine between their embeddings.
    Semantic { similarity: f32 },
}

impl Evidence {
    /// How the score reads in the report.
    fn describe(self) -> String {
        match self {
            Self::Structural { hamming } => format!("same shape, {hamming} bits apart"),
            Self::Semantic { similarity } => format!("cosine {similarity:.3}"),
        }
    }

    /// Which pass found the cluster, for the machine-readable surfaces.
    fn kind(self) -> &'static str {
        match self {
            Self::Structural { .. } => "structural",
            Self::Semantic { .. } => "semantic",
        }
    }

    /// Which named bucket a finding falls in, given the structural cut the run
    /// used.
    ///
    /// The bands are the ones CHANGES.md measured by hand: 0 bits was 6 of 6
    /// true, 1–3 stayed near that, 4 and 5 are drawn apart because precision
    /// visibly degrades between them, and everything landing exactly on the
    /// cut is its own bucket — that is where two thirds of a sweep's claimed
    /// lines sat, and hand precision there was about 2 in 6. Semantic findings
    /// get their own bucket rather than a hamming distance they were never
    /// measured on.
    ///
    /// `hamming == cut` is checked before the named bands, so a run configured
    /// with a cut inside 1..=5 still puts that exact distance in "at cut"
    /// rather than the band it would otherwise land in. A structural distance
    /// past every named band — only reachable when `cut` itself is configured
    /// above 6 — falls back to "at cut" too: clustering bounds every member's
    /// distance to `cut` by construction, so nothing past the named bands was
    /// ever part of what was hand-classified.
    pub fn bucket(self, cut: u32) -> Bucket {
        match self {
            Self::Semantic { .. } => Bucket::Cosine,
            Self::Structural { hamming } if hamming == cut => Bucket::AtCut,
            Self::Structural { hamming: 0 } => Bucket::Zero,
            Self::Structural { hamming } if (1..=3).contains(&hamming) => Bucket::OneToThree,
            Self::Structural { hamming: 4 } => Bucket::Four,
            Self::Structural { hamming: 5 } => Bucket::Five,
            Self::Structural { .. } => Bucket::AtCut,
        }
    }
}

/// A named range of hamming distance, or the semantic pass, ordered
/// most-trustworthy first.
///
/// The [`Ord`] derive is the point: it is declaration order, and declaration
/// order is trust order, so sorting a slice of buckets ascending is sorting it
/// by how much a reader should believe the findings in it — which is exactly
/// what [`rank`] and the report table need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bucket {
    Zero,
    OneToThree,
    Four,
    Five,
    AtCut,
    Cosine,
}

impl Bucket {
    /// Every bucket, most trustworthy first.
    const ALL: [Bucket; 6] = [
        Self::Zero,
        Self::OneToThree,
        Self::Four,
        Self::Five,
        Self::AtCut,
        Self::Cosine,
    ];

    /// How the bucket reads in a report.
    fn label(self) -> &'static str {
        match self {
            Self::Zero => "0",
            Self::OneToThree => "1-3",
            Self::Four => "4",
            Self::Five => "5",
            Self::AtCut => "at cut",
            Self::Cosine => "cosine",
        }
    }
}

/// A group of symbols reported as one finding.
#[derive(Debug, Clone)]
pub struct Cluster {
    pub members: Vec<DupCandidate>,
    pub evidence: Evidence,
    /// Set when this cluster was accepted once and has gained a member since.
    /// `None` is the ordinary case: never accepted, or unchanged since.
    pub accepted: Option<AcceptedSnapshot>,
}

/// What somebody reviewed when they accepted a cluster.
///
/// Kept on the resurfaced cluster so the report can say what is new rather than
/// showing the whole finding again as if nobody had ever looked at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedSnapshot {
    /// The ignore entry that accepted it.
    pub entry_id: String,
    /// The member keys that were reviewed — see [`member_key`].
    pub members: BTreeSet<String>,
}

/// How wide a symbol reaches, largest first.
///
/// An explicit ladder because [`Visibility`]'s discriminants are storage
/// numbers, not an order: `Package` is 4 and `Crate` is 1, but package reaches
/// further than crate. Ordering on the discriminant would rank backwards.
fn reach(visibility: Visibility) -> u8 {
    match visibility {
        Visibility::Public => 5,
        Visibility::Package => 4,
        Visibility::Crate => 3,
        Visibility::Module => 2,
        Visibility::Restricted => 1,
        Visibility::Private => 0,
    }
}

impl Cluster {
    /// Distinct modules the members live in.
    ///
    /// A member with no module path counts under its file, so a language the
    /// parser gives no modules for still separates two files instead of
    /// collapsing the whole repository into one module.
    pub fn module_spread(&self) -> usize {
        self.members
            .iter()
            .map(|c| c.module_path.as_deref().unwrap_or(&c.file_path))
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// Distinct files the members live in.
    pub fn file_spread(&self) -> usize {
        self.members
            .iter()
            .map(|c| c.file_path.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }

    /// The widest reach among the members.
    ///
    /// The widest, not the average: one `pub` copy of a private helper is
    /// already public API, and averaging would hide it behind its siblings.
    pub fn reach(&self) -> u8 {
        self.members
            .iter()
            .map(|c| reach(c.visibility))
            .max()
            .unwrap_or(0)
    }

    /// Lines that would go away if the cluster became one function.
    ///
    /// Every copy but the largest — the one that stays.
    pub fn duplicated_lines(&self) -> u32 {
        let total: u32 = self.members.iter().map(DupCandidate::lines).sum();
        let kept = self
            .members
            .iter()
            .map(DupCandidate::lines)
            .max()
            .unwrap_or(0);
        total - kept
    }

    /// Identity that survives a reparse. See [`cluster_hash`].
    pub fn cluster_hash(&self) -> String {
        cluster_hash(&self.members)
    }

    /// The identity as the report prints it — see [`short_hash`].
    pub fn short_hash(&self) -> String {
        short_hash(&self.cluster_hash())
    }

    /// The members as the ignore-list compares them.
    pub fn member_keys(&self) -> BTreeSet<String> {
        self.members.iter().map(member_key).collect()
    }

    /// The same members in the pre-`owner_name` key format, for matching a
    /// snapshot that was written before the field existed.
    pub fn legacy_member_keys(&self) -> BTreeSet<String> {
        self.members.iter().map(legacy_member_key).collect()
    }

    /// The members that were not in the accepted snapshot — everything a
    /// reviewer has not seen. Empty when the cluster was never accepted.
    pub fn added_members(&self) -> Vec<&DupCandidate> {
        let Some(accepted) = &self.accepted else {
            return Vec::new();
        };
        self.members
            .iter()
            .filter(|m| !accepted.members.contains(&member_key(m)))
            .collect()
    }

    /// See [`Evidence::bucket`].
    pub fn bucket(&self, cut: u32) -> Bucket {
        self.evidence.bucket(cut)
    }
}

/// Hex characters of the identity the report and the ignore-list id print.
///
/// Twelve, not the whole 64: a reader retypes this, and the display prefix only
/// has to be unambiguous inside one report. The stored identity is never
/// truncated — [`cluster_hash`] returns all of it, and matching never looks at
/// the prefix.
pub const SHORT_HASH_LEN: usize = 12;

/// How one member is identified, for the cluster hash and for the ignore-list.
///
/// Six fields, unit-separated: repo-relative path, language, qualified path,
/// kind, name and signature. The pair `(module_path, name)` this used to be was
/// two ways wrong — two overloads share it, and it names a different symbol the
/// moment the file is renamed under an unchanged module path — and being wrong
/// here is not a cosmetic problem: the ignore-list compares these, so a key
/// that collides silently suppresses somebody else's finding.
///
/// A unit separator cannot occur in a path, an identifier or a module path, so
/// `("a::b", "c")` cannot collide with `("a", "b::c")`.
///
/// Not the line numbers, which move when anything above the symbol is edited,
/// and not the id, which `split_by_reuse` reassigns on every reparse.
pub fn member_key(c: &DupCandidate) -> String {
    [
        c.file_path.as_str(),
        c.language.as_deref().unwrap_or(""),
        c.module_path.as_deref().unwrap_or(""),
        c.kind.as_str(),
        c.owner_name.as_deref().unwrap_or(""),
        c.name.as_str(),
        c.signature.as_deref().unwrap_or(""),
    ]
    .join("\u{1f}")
}

/// Fields in a current [`member_key`]. Named so the legacy shim below states
/// what it is comparing against instead of hiding a literal.
const MEMBER_KEY_FIELDS: usize = 7;

/// The same key as it was written before `owner_name` was a field.
///
/// `member_key` gained a seventh field, and a stored acceptance holds the
/// six-field form. The two never compare equal — not even when `owner_name` is
/// `None`, because an empty value still contributes its `\u{1f}` separator — so
/// every duplication cluster a reviewer had accepted came back as an unreviewed
/// finding, and the `dup-ignore-*` entry that suppressed it stayed in memory
/// matching nothing.
///
/// Read-side only. Nothing writes this form again: a key recorded from now on
/// carries the owner, and this exists so a decision somebody already made
/// survives the upgrade that changed the format.
fn legacy_member_key(c: &DupCandidate) -> String {
    [
        c.file_path.as_str(),
        c.language.as_deref().unwrap_or(""),
        c.module_path.as_deref().unwrap_or(""),
        c.kind.as_str(),
        c.name.as_str(),
        c.signature.as_deref().unwrap_or(""),
    ]
    .join("\u{1f}")
}

/// Does this stored snapshot predate the `owner_name` field?
///
/// By field count, not by a version marker: none was written, and the count is
/// unambiguous because no field may contain the separator.
fn is_legacy_snapshot(members: &BTreeSet<String>) -> bool {
    members
        .iter()
        .any(|m| m.split('\u{1f}').count() < MEMBER_KEY_FIELDS)
}

/// Identity of a cluster: SHA-256 over its sorted member keys, in full.
///
/// All 64 hex characters, 256 bits. The first eight used to be stored, and 32
/// bits is a coin flip at a few tens of thousands of clusters — a collision
/// there makes one accepted cluster suppress an unrelated finding, silently.
/// The short form exists for printing only ([`short_hash`]).
///
/// Sorted, so the order the members came back in cannot change the answer.
pub fn cluster_hash(members: &[DupCandidate]) -> String {
    let mut keys: Vec<String> = members.iter().map(member_key).collect();
    keys.sort_unstable();
    let mut hasher = Sha256::new();
    // Record separator between members, unit separator inside them.
    hasher.update(keys.join("\u{1e}").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The identity a cluster had before `owner_name` joined the key.
///
/// Read-side only, and for one purpose: an accepted entry that recorded no
/// membership can be matched only by the hash prefix in its id, and that
/// prefix was computed over the old keys. Without this, every snapshot-less
/// acceptance made before the field was added stopped suppressing anything.
pub fn legacy_cluster_hash(members: &[DupCandidate]) -> String {
    let mut keys: Vec<String> = members.iter().map(legacy_member_key).collect();
    keys.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(keys.join("\u{1e}").as_bytes());
    format!("{:x}", hasher.finalize())
}

/// The first [`SHORT_HASH_LEN`] characters of an identity, for display.
pub fn short_hash(hash: &str) -> String {
    hash.chars().take(SHORT_HASH_LEN).collect()
}

/// Order clusters worst-first.
///
/// Bucket before anything else: hand-classifying showed the ≤3-bit bands were
/// true positives every time and the cut band was right about one time in
/// three, so a finding the report can vouch for outranks one it cannot,
/// however far the untrustworthy one spreads — the module-spread order used to
/// run first and put exactly those noisy cut-band clusters on top. Within a
/// bucket the old lexicographic order still applies, spread before reach,
/// because among equally-trustworthy findings distance is still the stronger
/// signal: two copies in one file sit under one reader's eyes and get fixed
/// together, while two copies in different modules diverge — one gets the bug
/// fix and the other does not. Reach breaks the tie between equally-distant
/// clusters, then the line count, then the hash so that two runs over an
/// unchanged repository print the same report.
pub fn rank(clusters: &mut [Cluster], cut: u32) {
    clusters.sort_by(|a, b| {
        a.bucket(cut)
            .cmp(&b.bucket(cut))
            .then(b.module_spread().cmp(&a.module_spread()))
            .then(b.file_spread().cmp(&a.file_spread()))
            .then(b.reach().cmp(&a.reach()))
            .then(b.duplicated_lines().cmp(&a.duplicated_lines()))
            .then(a.cluster_hash().cmp(&b.cluster_hash()))
    });
}

/// Clusters and duplicated lines per bucket, most-trustworthy first, skipping
/// buckets nothing landed in.
///
/// Shared by [`render`] and [`render_json`] so the prose table and the
/// machine-readable summary can never disagree about what they counted.
fn bucket_summary(clusters: &[Cluster], cut: u32) -> Vec<(Bucket, usize, u32)> {
    Bucket::ALL
        .into_iter()
        .filter_map(|bucket| {
            let members: Vec<&Cluster> = clusters
                .iter()
                .filter(|c| c.bucket(cut) == bucket)
                .collect();
            if members.is_empty() {
                return None;
            }
            let lines: u32 = members.iter().map(|c| c.duplicated_lines()).sum();
            Some((bucket, members.len(), lines))
        })
        .collect()
}

/// The markdown report.
///
/// `snippet` yields a member's body, or `None` when the file has changed under
/// the index — a stale line range prints no code rather than the wrong code.
///
/// `cut` is the structural threshold the scan actually ran with, the same one
/// [`rank`] orders by. It is a parameter rather than
/// [`super::body::SIMHASH_HAMMING_THRESHOLD`] because a repository that
/// configured `code.duplication.hamming_threshold` away from the default would
/// otherwise get a table whose "at cut" row counts against a cut its clusters
/// were never bounded by, disagreeing with the order printed beneath it.
pub fn render(
    clusters: &[Cluster],
    cut: u32,
    snippet: &mut dyn FnMut(&DupCandidate) -> Option<String>,
) -> String {
    if clusters.is_empty() {
        return "# Duplication\n\nNo clusters found.\n".to_string();
    }

    let mut out = String::from("# Duplication\n\n");
    let total = total_lines(clusters);
    out.push_str(&format!(
        "{} cluster{}, {total} duplicated line{}.\n",
        clusters.len(),
        if clusters.len() == 1 { "" } else { "s" },
        if total == 1 { "" } else { "s" },
    ));

    out.push_str("\n| bucket | clusters | lines |\n|---|---:|---:|\n");
    for (bucket, n, lines) in bucket_summary(clusters, cut) {
        out.push_str(&format!("| {} | {n} | {lines} |\n", bucket.label()));
    }

    for (n, cluster) in clusters.iter().enumerate() {
        out.push('\n');
        render_cluster(&mut out, n + 1, cluster, snippet);
    }
    out
}

/// The same findings as JSON.
///
/// No snippets: a machine-readable surface carries the `file:line` and the
/// consumer reads the body itself, so this never touches the disk. It also
/// carries `evidence.hamming`, which the prose report only spells out — that is
/// what lets a caller bucket the findings by distance, and the distance is
/// where the report's signal actually lives. `buckets` is the same summary
/// [`render`] prints as a table, pre-computed rather than left for a caller to
/// re-derive from `evidence.hamming` — `cut` is the scan's own threshold, as
/// in [`render`].
pub fn render_json(clusters: &[Cluster], cut: u32) -> String {
    let buckets: Vec<serde_json::Value> = bucket_summary(clusters, cut)
        .into_iter()
        .map(|(bucket, n, lines)| {
            serde_json::json!({
                "bucket": bucket.label(),
                "clusters": n,
                "duplicated_lines": lines,
            })
        })
        .collect();
    let findings: Vec<serde_json::Value> = clusters.iter().map(cluster_json).collect();
    let value = serde_json::json!({
        "clusters": clusters.len(),
        "duplicated_lines": total_lines(clusters),
        "buckets": buckets,
        "findings": findings,
    });
    format!("{}\n", serde_json::to_string_pretty(&value).unwrap())
}

fn cluster_json(cluster: &Cluster) -> serde_json::Value {
    let evidence = match cluster.evidence {
        Evidence::Structural { hamming } => serde_json::json!({
            "kind": cluster.evidence.kind(),
            "hamming": hamming,
        }),
        Evidence::Semantic { similarity } => serde_json::json!({
            "kind": cluster.evidence.kind(),
            "similarity": similarity,
        }),
    };
    let added: BTreeSet<String> = cluster
        .added_members()
        .into_iter()
        .map(member_key)
        .collect();
    let members: Vec<serde_json::Value> = cluster
        .members
        .iter()
        .map(|m| {
            serde_json::json!({
                "file": m.file_path,
                // 1-based, as in the prose report. The stored rows are 0-based
                // tree-sitter rows; a consumer opening an editor at the number
                // it was given must land on the symbol.
                "line_start": m.line_start + 1,
                "line_end": m.line_end + 1,
                "name": m.name,
                "module": m.module_path,
                // True only on a cluster that grew after being accepted: the
                // member nobody has reviewed yet.
                "new": added.contains(&member_key(m)),
            })
        })
        .collect();
    serde_json::json!({
        // The full identity, never the display prefix: a consumer that stores
        // this and compares it later must not be handed a truncation.
        "hash": cluster.cluster_hash(),
        "short_hash": cluster.short_hash(),
        "accepted_entry": cluster.accepted.as_ref().map(|a| a.entry_id.clone()),
        "name": cluster.members.first().map(|c| c.name.as_str()),
        "copies": cluster.members.len(),
        "module_spread": cluster.module_spread(),
        "file_spread": cluster.file_spread(),
        "visibility": visibility_label(cluster.reach()),
        "duplicated_lines": cluster.duplicated_lines(),
        "evidence": evidence,
        "members": members,
    })
}

/// The same findings as CSV, one row per member.
///
/// Per member rather than per cluster because the cluster columns repeat
/// cheaply and a spreadsheet can group them, while a per-cluster row would have
/// to fold the locations into one cell — which is where CSV stops being
/// readable by anything.
pub fn render_csv(clusters: &[Cluster]) -> String {
    let mut out = String::from(
        "cluster_hash,cluster_name,copies,module_spread,file_spread,visibility,\
         duplicated_lines,evidence,hamming,similarity,file,line_start,line_end,symbol\n",
    );
    for cluster in clusters {
        let (hamming, similarity) = match cluster.evidence {
            Evidence::Structural { hamming } => (hamming.to_string(), String::new()),
            Evidence::Semantic { similarity } => (String::new(), format!("{similarity:.3}")),
        };
        let name = cluster.members.first().map_or("", |c| c.name.as_str());
        // Every one of these walks the whole member list, and `cluster_hash`
        // sorts and hashes it. They are cluster-wide values: computing them
        // once per member makes a wide cluster cost O(members^2).
        let cluster_hash = cluster.cluster_hash();
        let copies = cluster.members.len();
        let module_spread = cluster.module_spread();
        let file_spread = cluster.file_spread();
        let visibility = visibility_label(cluster.reach());
        let duplicated_lines = cluster.duplicated_lines();
        for member in &cluster.members {
            let symbol = match &member.module_path {
                Some(module) => format!("{module}::{}", member.name),
                None => member.name.to_string(),
            };
            out.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
                cluster_hash,
                csv_field(name),
                copies,
                module_spread,
                file_spread,
                visibility,
                duplicated_lines,
                cluster.evidence.kind(),
                hamming,
                similarity,
                csv_field(&member.file_path),
                member.line_start + 1,
                member.line_end + 1,
                csv_field(&symbol),
            ));
        }
    }
    out
}

/// A field quoted only when it has to be.
///
/// A path may legally hold a comma or a quote, and one such path would shift
/// every column to its right for the rest of the row — a corruption a reader
/// has no way to notice.
pub fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn total_lines(clusters: &[Cluster]) -> u32 {
    clusters.iter().map(Cluster::duplicated_lines).sum()
}

fn render_cluster(
    out: &mut String,
    n: usize,
    cluster: &Cluster,
    snippet: &mut dyn FnMut(&DupCandidate) -> Option<String>,
) {
    let name = cluster
        .members
        .first()
        .map_or("(empty)", |c| c.name.as_str());
    out.push_str(&format!("## {n}. `{name}` — {}\n\n", cluster.short_hash()));
    if let Some(accepted) = &cluster.accepted {
        out.push_str(&format!(
            "**Changed since accepted** (`{}`): {} new member{}. The rest was \
             reviewed and stays accepted.\n\n",
            accepted.entry_id,
            cluster.added_members().len(),
            if cluster.added_members().len() == 1 {
                ""
            } else {
                "s"
            },
        ));
    }
    out.push_str(&format!(
        "{} copies across {} module{} · {} · {} duplicated lines · {}\n\n",
        cluster.members.len(),
        cluster.module_spread(),
        if cluster.module_spread() == 1 {
            ""
        } else {
            "s"
        },
        visibility_label(cluster.reach()),
        cluster.duplicated_lines(),
        cluster.evidence.describe(),
    ));

    let added: BTreeSet<String> = cluster
        .added_members()
        .into_iter()
        .map(member_key)
        .collect();
    for member in &cluster.members {
        // Only the new members are marked: a reader who accepted this cluster
        // once has to look at what they have not seen, not at all of it again.
        let marker = if added.contains(&member_key(member)) {
            "**NEW** "
        } else {
            ""
        };
        // Display is 1-based; the stored rows are 0-based tree-sitter rows.
        out.push_str(&format!(
            "- {marker}`{}:{}-{}` — {}\n",
            member.file_path,
            member.line_start + 1,
            member.line_end + 1,
            qualified(member),
        ));
    }

    if let Some(first) = cluster.members.first() {
        if let Some(body) = snippet(first) {
            out.push_str(&format!("\n```{}\n", fence_tag(&first.file_path)));
            out.push_str(&elide(&body));
            out.push_str("```\n");
        }
    }
}

/// `module::name`, or just the name when the parser gave no module.
fn qualified(candidate: &DupCandidate) -> String {
    match &candidate.module_path {
        Some(module) => format!("`{module}::{}`", candidate.name),
        None => format!("`{}`", candidate.name),
    }
}

fn visibility_label(reach: u8) -> &'static str {
    match reach {
        5 => "public",
        4 => "package",
        3 => "crate",
        2 => "module",
        1 => "restricted",
        _ => "private",
    }
}

/// The fence tag for a path, or none when the language is unknown.
///
/// Extension only: [`Language::from_path`] falls back to reading a shebang off
/// disk, and a report must render the same whether or not the file is still
/// there.
fn fence_tag(path: &str) -> &'static str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Language::from_extension)
        .map_or("", Language::config_key)
}

// --- the ignore-list ---
//
// Intentional duplication reported forever makes the tool useless after the
// first run. slopo keeps a flat list of hashes with no reason attached; this
// keeps a `decision` in the memory store, so the accepted cluster carries *why*
// it was accepted, is searchable like any other decision, and can be superseded
// when the reason stops holding. No new schema — the store already models all
// of it.
//
// MIGRATION. Entries written before the membership was recorded carry an id
// derived from the old eight-character digest over `(module_path, name)` pairs.
// That digest cannot be un-hashed, so there is nothing to convert: the
// membership it stood for is not recoverable from it, and neither is the new
// identity. Such an entry is left exactly as it is and keeps matching by id
// prefix — which, since the member key changed, means it matches nothing, and
// its cluster is reported again. Re-running `mdkb dup` and accepting it once
// more writes the membership down and is the whole migration. Nothing here
// rewrites an existing ignore entry: the rationale on it is a human's, and a
// pass that edited it would be editing something it cannot re-derive.

/// The fence that holds the reviewed membership inside an ignore entry.
///
/// A fenced block rather than a new column: the entry is a `decision` like any
/// other, projected to disk and read by people, and the membership has to
/// survive that round trip. `text` and not `json` so the projected markdown
/// does not invite a renderer to reformat it.
const MEMBERS_FENCE: &str = "```text dup-members";

/// The tag every ignore entry carries.
pub const IGNORE_TAG: &str = "dup-ignore";

/// The memory id for an accepted cluster.
///
/// Namespaced rather than the bare hash: a memory store holds ids a human
/// chose, and a hex string is exactly the kind of id somebody else might pick.
/// The prefix also makes the whole list greppable.
///
/// Takes the short form, because this is the string a reader retypes. It is a
/// handle, not the identity: matching compares membership (see
/// [`is_ignored`]), so a shortened id can never be what decides whether a
/// finding is suppressed.
pub fn ignore_entry_id(cluster_hash: &str) -> String {
    format!("{IGNORE_ID_PREFIX}{}", short_hash(cluster_hash))
}

/// The namespace every ignore entry's id starts with.
pub const IGNORE_ID_PREFIX: &str = "dup-ignore-";

/// Record a cluster as accepted duplication.
///
/// Written through [`handle_memory_add`](crate::core::memory::handle_memory_add)
/// rather than straight into the table, so the entry is projected to disk,
/// indexed and embedded like every other decision. An ignore nobody can find
/// later is how a flat file behaves.
///
/// `source_type` is `user_statement`: somebody looked at the finding and
/// decided. Nothing here is inferred.
pub fn ignore_cluster(
    ctx: &crate::core::Context,
    cluster: &Cluster,
    rationale: &str,
) -> crate::error::Result<String> {
    let hash = cluster.cluster_hash();
    let id = ignore_entry_id(&hash);
    let name = cluster
        .members
        .first()
        .map_or("(empty)", |c| c.name.as_str());
    let locations = cluster.members.iter().fold(String::new(), |mut acc, m| {
        use std::fmt::Write;
        // Display is 1-based, as everywhere else the reader sees a line number.
        let _ = writeln!(acc, "- `{}:{}`", m.file_path, m.line_start + 1);
        acc
    });
    // The membership is written down, not just the digest, because the digest
    // cannot be un-hashed: a cluster that gains a member gets a different one,
    // and an ignore keyed on it would silently stop matching. The reviewed set
    // is what the decision was actually about.
    let members = cluster
        .member_keys()
        .into_iter()
        .collect::<Vec<_>>()
        .join("\n");
    let content = format!(
        "{rationale}\n\nCluster `{hash}`:\n\n{locations}\n\
         Reviewed membership — a cluster is still accepted while its members are \
         a subset of this list.\n\n{MEMBERS_FENCE}\n{members}\n```\n"
    );

    crate::core::memory::handle_memory_add(
        ctx,
        &id,
        &format!("Accepted duplication: {name}"),
        "decision",
        Some(IGNORE_TAG),
        &content,
        None,
        None,
        None,
        Some("user_statement"),
        &[],
        None,
        None,
        false,
    )?;
    Ok(id)
}

/// Parse the reviewed membership out of an ignore entry's content.
///
/// `None` when there is no fenced block — an entry somebody wrote by hand, or
/// one written before the membership was recorded. Those keep the old
/// behaviour: the id is the whole decision (see [`Accepted::members`]).
fn parse_members(content: &str) -> Option<BTreeSet<String>> {
    let rest = content.split_once(MEMBERS_FENCE)?.1;
    let block = rest.split_once("\n```")?.0;
    Some(
        block
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// One standing accepted-duplication decision.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub entry_id: String,
    /// The reviewed membership, or `None` for an entry that recorded none — a
    /// hand-written one, or one from before the membership was stored. Such an
    /// entry can only match by id, exactly as it did before.
    pub members: Option<BTreeSet<String>>,
}

/// Every accepted-duplication decision currently standing.
///
/// [`resolve_active`](crate::store::memory_graph::resolve_active) is the store's
/// own answer to "is this entry still in force", so a superseded *or* expired
/// entry stops filtering and the cluster comes back — which is the point of
/// keeping this in the memory store rather than a text file. The untracked form
/// because an audit is a read: it runs over every cluster, must work on a
/// read-only connection, and must not inflate the access counts that rank
/// memory search.
///
/// The entry type and tag are checked too. An unrelated entry that happens to
/// carry this id prefix must not silently delete a finding: suppression fails
/// open here for the same reason it does in the call-graph pass.
pub fn accepted_clusters(conn: &rusqlite::Connection) -> crate::error::Result<Vec<Accepted>> {
    let ids = crate::store::memory::list_entry_ids_with_prefix(conn, IGNORE_ID_PREFIX)?;
    let mut out = Vec::new();
    for id in ids {
        let Some(entry) = crate::store::memory_graph::resolve_active(conn, &id)? else {
            continue;
        };
        if entry.entry_type != crate::store::memory::EntryType::Decision
            || !entry.tags.iter().any(|t| t == IGNORE_TAG)
        {
            continue;
        }
        out.push(Accepted {
            members: parse_members(&entry.content),
            entry_id: entry.id,
        });
    }
    Ok(out)
}

/// What the ignore-list says about one cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nobody accepted it; report it as a finding.
    Report,
    /// Accepted, and it has gained nothing since; suppress it.
    Accepted,
    /// Accepted once, and it has grown since. Report it, carrying what was
    /// reviewed so only the new members have to be looked at.
    Changed(AcceptedSnapshot),
}

/// Match one cluster against the standing decisions.
///
/// Subset, not equality. A cluster is still the thing somebody accepted as long
/// as every member of it was in what they reviewed: losing a member, or a
/// cluster splitting into two halves, changes nothing about the decision — both
/// halves were accepted. Gaining one does: nobody has looked at the new copy,
/// and the whole point of an ignore-list is that a human decided, so it
/// resurfaces rather than inheriting an acceptance it was never part of.
///
/// Equality was never a real option: it is what the digest already gave, and it
/// is exactly why a removal used to make a finding reappear as new.
pub fn verdict(accepted: &[Accepted], cluster: &Cluster) -> Verdict {
    let keys = cluster.member_keys();
    let legacy_keys = cluster.legacy_member_keys();
    let hash = cluster.cluster_hash();
    let legacy_hash = legacy_cluster_hash(&cluster.members);
    let mut grown: Option<&Accepted> = None;
    let mut by_prefix = 0usize;
    for entry in accepted {
        let Some(members) = &entry.members else {
            // No recorded membership: it can only speak for the cluster its id
            // was derived from, and the id holds a prefix of that identity. Any
            // prefix is accepted — 12 characters is what the report prints, but
            // somebody who copied more, or fewer, meant the same cluster.
            let typed = entry
                .entry_id
                .strip_prefix(IGNORE_ID_PREFIX)
                .unwrap_or_default();
            // Either identity: an id written before `owner_name` joined the
            // key holds a prefix of the old hash, and the decision it records
            // is no less a decision for that.
            if !typed.is_empty() && (hash.starts_with(typed) || legacy_hash.starts_with(typed)) {
                by_prefix += 1;
            }
            continue;
        };
        // Compare in the format the snapshot was written in. A decision does
        // not stop being a decision because the key format changed under it.
        let keys = if is_legacy_snapshot(members) {
            &legacy_keys
        } else {
            &keys
        };
        if keys.is_subset(members) {
            return Verdict::Accepted;
        }
        // A strict superset means the reviewed cluster grew. Keep looking —
        // another entry may cover it outright — but remember the best match.
        if members.is_subset(keys) && !members.is_empty() {
            let better =
                grown.is_none_or(|g| g.members.as_ref().is_none_or(|m| m.len() < members.len()));
            if better {
                grown = Some(entry);
            }
        }
    }
    // Exactly one snapshot-less entry named this cluster. Two means the
    // prefixes somebody typed are ambiguous, and suppression fails open: a
    // false positive costs the reader one line, a false suppression deletes a
    // finding with nothing saying it happened.
    if by_prefix == 1 {
        return Verdict::Accepted;
    }
    match grown {
        Some(entry) => Verdict::Changed(AcceptedSnapshot {
            entry_id: entry.entry_id.clone(),
            members: entry.members.clone().unwrap_or_default(),
        }),
        None => Verdict::Report,
    }
}

/// Whether a cluster is currently suppressed by the ignore-list.
pub fn is_ignored(conn: &rusqlite::Connection, cluster: &Cluster) -> crate::error::Result<bool> {
    Ok(verdict(&accepted_clusters(conn)?, cluster) == Verdict::Accepted)
}

/// Drop the clusters somebody already accepted, and mark the ones that have
/// grown since they were accepted so the report can say what is new.
pub fn filter_ignored(
    conn: &rusqlite::Connection,
    clusters: Vec<Cluster>,
) -> crate::error::Result<Vec<Cluster>> {
    let accepted = accepted_clusters(conn)?;
    let mut kept = Vec::with_capacity(clusters.len());
    for mut cluster in clusters {
        match verdict(&accepted, &cluster) {
            Verdict::Accepted => {}
            Verdict::Report => kept.push(cluster),
            Verdict::Changed(snapshot) => {
                cluster.accepted = Some(snapshot);
                kept.push(cluster);
            }
        }
    }
    Ok(kept)
}

/// The head of a body, with a marker where the rest was cut.
fn elide(body: &str) -> String {
    let lines: Vec<&str> = body.lines().collect();
    let mut out = String::new();
    for line in lines.iter().take(MAX_SNIPPET_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    if lines.len() > MAX_SNIPPET_LINES {
        out.push_str(&format!(
            "… {} more lines\n",
            lines.len() - MAX_SNIPPET_LINES
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(
        id: i64,
        name: &str,
        file: &str,
        module: Option<&str>,
        visibility: Visibility,
        lines: u32,
    ) -> DupCandidate {
        DupCandidate {
            id,
            name: name.to_string(),
            file_path: file.to_string(),
            module_path: module.map(str::to_string),
            kind: "Function".to_string(),
            language: Some("rust".to_string()),
            signature: None,
            owner_name: None,
            visibility,
            line_start: 10,
            line_end: 10 + lines - 1,
        }
    }

    fn cluster(members: Vec<DupCandidate>) -> Cluster {
        Cluster {
            members,
            evidence: Evidence::Semantic { similarity: 0.8 },
            accepted: None,
        }
    }

    /// A decision made before `owner_name` joined the key still suppresses.
    ///
    /// The key gained a seventh field, and a six-field stored key never
    /// compares equal to a seven-field one — not even when `owner_name` is
    /// `None`, because the empty value still contributes its separator. So
    /// every duplication a reviewer had accepted came back as an unreviewed
    /// finding, and the `dup-ignore-*` entry that suppressed it stayed in
    /// memory matching nothing. Nobody was told, because the change rode in a
    /// commit about computing cluster values once.
    #[test]
    fn a_snapshot_written_before_owner_name_still_suppresses_its_cluster() {
        let c = cluster(vec![
            member(1, "parse", "a.rs", Some("m"), Visibility::Public, 20),
            member(2, "parse", "b.rs", Some("m"), Visibility::Public, 20),
        ]);
        let stored = Accepted {
            entry_id: format!("{IGNORE_ID_PREFIX}deadbeefcafe"),
            members: Some(c.legacy_member_keys()),
        };
        assert_eq!(
            verdict(&[stored], &c),
            Verdict::Accepted,
            "the format changed under the decision; the decision did not change"
        );
    }

    /// And the id-only form too, whose prefix was computed over the old keys.
    #[test]
    fn a_snapshotless_entry_keyed_on_the_old_hash_still_suppresses() {
        let c = cluster(vec![
            member(1, "parse", "a.rs", Some("m"), Visibility::Public, 20),
            member(2, "parse", "b.rs", Some("m"), Visibility::Public, 20),
        ]);
        let old = legacy_cluster_hash(&c.members);
        assert_ne!(
            old,
            c.cluster_hash(),
            "the fixture is only meaningful while the two identities differ"
        );
        let stored = Accepted {
            entry_id: format!("{IGNORE_ID_PREFIX}{}", short_hash(&old)),
            members: None,
        };
        assert_eq!(verdict(&[stored], &c), Verdict::Accepted);
    }

    /// A current snapshot keeps being compared in the current format: the shim
    /// is a read-side fallback, not a loosening of the key.
    #[test]
    fn a_current_snapshot_is_still_matched_on_the_full_key() {
        let c = cluster(vec![
            member(1, "parse", "a.rs", Some("m"), Visibility::Public, 20),
            member(2, "parse", "b.rs", Some("m"), Visibility::Public, 20),
        ]);
        let stored = Accepted {
            entry_id: format!("{IGNORE_ID_PREFIX}{}", c.short_hash()),
            members: Some(c.member_keys()),
        };
        assert_eq!(verdict(&[stored], &c), Verdict::Accepted);

        let other = cluster(vec![member(
            3,
            "render",
            "c.rs",
            Some("m"),
            Visibility::Public,
            20,
        )]);
        assert_eq!(
            verdict(&[stored_for(&c)], &other),
            Verdict::Report,
            "an unrelated cluster is not suppressed by either format"
        );
    }

    fn stored_for(c: &Cluster) -> Accepted {
        Accepted {
            entry_id: format!("{IGNORE_ID_PREFIX}{}", c.short_hash()),
            members: Some(c.member_keys()),
        }
    }

    /// Two fixed clusters, ranked twice with the spread moved from one to the
    /// other. Asserting a single run would pass on the hash tie-break alone —
    /// which is a test that cannot fail. The winner has to *change* when the
    /// spread moves, and the hash order is the same in both runs, so only the
    /// spread term can produce both answers.
    #[test]
    fn a_cluster_spanning_two_modules_outranks_one_inside_a_single_file() {
        let ranked_with_spread_on = |spread_on_left: bool| {
            let (left_file, right_file) = if spread_on_left {
                ("src/b.rs", "src/c.rs")
            } else {
                ("src/a.rs", "src/d.rs")
            };
            let (left_module, right_module) = if spread_on_left {
                (Some("beta"), Some("gamma"))
            } else {
                (Some("alpha"), Some("delta"))
            };
            let mut clusters = vec![
                cluster(vec![
                    member(1, "a", "src/a.rs", Some("alpha"), Visibility::Private, 10),
                    member(2, "b", left_file, left_module, Visibility::Private, 10),
                ]),
                cluster(vec![
                    member(3, "c", "src/c.rs", Some("gamma"), Visibility::Private, 10),
                    member(4, "d", right_file, right_module, Visibility::Private, 10),
                ]),
            ];
            rank(&mut clusters, 6);
            clusters[0].members[0].name.clone()
        };

        assert_eq!(ranked_with_spread_on(true), "a", "the spread cluster wins");
        assert_eq!(ranked_with_spread_on(false), "c", "and again when it moves");
    }

    /// Same shape of proof: the winner must follow the `pub`, not the hash.
    #[test]
    fn a_public_cluster_outranks_an_equally_distant_private_one() {
        let ranked_with_public_on = |public_on_left: bool| {
            let (left, right) = if public_on_left {
                (Visibility::Public, Visibility::Private)
            } else {
                (Visibility::Private, Visibility::Public)
            };
            let mut clusters = vec![
                cluster(vec![
                    member(1, "a", "src/a.rs", Some("alpha"), left, 10),
                    member(2, "b", "src/b.rs", Some("beta"), left, 10),
                ]),
                cluster(vec![
                    member(3, "c", "src/c.rs", Some("gamma"), right, 10),
                    member(4, "d", "src/d.rs", Some("delta"), right, 10),
                ]),
            ];
            rank(&mut clusters, 6);
            clusters[0].members[0].name.clone()
        };

        assert_eq!(ranked_with_public_on(true), "a");
        assert_eq!(
            ranked_with_public_on(false),
            "c",
            "the winner follows the pub"
        );
    }

    /// The widest member, not the average: one `pub` copy of a private helper
    /// is already public API.
    #[test]
    fn one_public_member_makes_the_cluster_public() {
        let mixed = cluster(vec![
            member(1, "a", "src/a.rs", Some("alpha"), Visibility::Private, 10),
            member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 10),
        ]);

        assert_eq!(mixed.reach(), reach(Visibility::Public));
    }

    /// `Package` is discriminant 4 and `Crate` is 1, so ordering on the stored
    /// number would rank package below crate — backwards.
    #[test]
    fn reach_is_a_ladder_not_the_stored_discriminant() {
        assert!(reach(Visibility::Public) > reach(Visibility::Package));
        assert!(reach(Visibility::Package) > reach(Visibility::Crate));
        assert!(reach(Visibility::Crate) > reach(Visibility::Module));
        assert!(reach(Visibility::Module) > reach(Visibility::Restricted));
        assert!(reach(Visibility::Restricted) > reach(Visibility::Private));
    }

    #[test]
    fn the_cluster_hash_survives_a_reparse_that_reassigns_ids() {
        let before = [
            member(
                1,
                "parse",
                "src/a.rs",
                Some("alpha"),
                Visibility::Public,
                10,
            ),
            member(2, "parse", "src/b.rs", Some("beta"), Visibility::Public, 10),
        ];
        // Same code, reparsed: new ids, and the symbols moved down the file.
        let after = [
            DupCandidate {
                id: 907,
                line_start: 400,
                line_end: 409,
                ..before[0].clone()
            },
            DupCandidate {
                id: 908,
                line_start: 512,
                line_end: 521,
                ..before[1].clone()
            },
        ];

        assert_eq!(cluster_hash(&before), cluster_hash(&after));
    }

    #[test]
    fn the_cluster_hash_ignores_the_order_the_members_came_back_in() {
        let a = member(
            1,
            "parse",
            "src/a.rs",
            Some("alpha"),
            Visibility::Public,
            10,
        );
        let b = member(2, "parse", "src/b.rs", Some("beta"), Visibility::Public, 10);

        assert_eq!(cluster_hash(&[a.clone(), b.clone()]), cluster_hash(&[b, a]));
    }

    #[test]
    fn different_members_hash_differently() {
        let a = [member(
            1,
            "parse",
            "src/a.rs",
            Some("alpha"),
            Visibility::Public,
            10,
        )];
        let b = [member(
            1,
            "parse",
            "src/a.rs",
            Some("beta"),
            Visibility::Public,
            10,
        )];

        assert_ne!(cluster_hash(&a), cluster_hash(&b));
    }

    /// The separators exist for this: without them `("a::b", "c")` and
    /// `("a", "b::c")` would concatenate to the same bytes.
    #[test]
    fn a_module_boundary_cannot_be_faked_by_a_name() {
        let a = [member(
            1,
            "c",
            "src/a.rs",
            Some("a::b"),
            Visibility::Public,
            10,
        )];
        let b = [member(
            1,
            "b::c",
            "src/a.rs",
            Some("a"),
            Visibility::Public,
            10,
        )];

        assert_ne!(cluster_hash(&a), cluster_hash(&b));
    }

    /// The whole digest is kept; only the printed form is short.
    ///
    /// Eight hex characters used to be stored, and 32 bits collide at a few
    /// tens of thousands of clusters — a collision there makes one accepted
    /// cluster suppress an unrelated finding with nothing in the output saying
    /// so.
    #[test]
    fn the_identity_is_stored_whole_and_only_displayed_short() {
        let hash = cluster_hash(&[member(
            1,
            "a",
            "src/a.rs",
            Some("alpha"),
            Visibility::Public,
            10,
        )]);

        assert_eq!(hash.len(), 64, "256 bits, not a truncation");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{hash}");

        let short = short_hash(&hash);
        assert_eq!(short.len(), SHORT_HASH_LEN);
        assert!(
            (12..=16).contains(&SHORT_HASH_LEN),
            "a display prefix short enough to retype and long enough to be unambiguous"
        );
        assert!(hash.starts_with(&short), "the short form is a prefix of it");
    }

    #[test]
    fn duplicated_lines_counts_every_copy_but_the_one_that_stays() {
        let c = cluster(vec![
            member(1, "a", "src/a.rs", None, Visibility::Private, 10),
            member(2, "b", "src/b.rs", None, Visibility::Private, 8),
            member(3, "c", "src/c.rs", None, Visibility::Private, 6),
        ]);

        assert_eq!(c.duplicated_lines(), 14, "8 + 6; the 10-line copy stays");
    }

    #[test]
    fn a_member_with_no_module_still_counts_as_its_own_file() {
        // Collapsing every module-less symbol into one bucket would report a
        // whole repository of C as a single module.
        let c = cluster(vec![
            member(1, "a", "src/a.c", None, Visibility::Private, 10),
            member(2, "b", "src/b.c", None, Visibility::Private, 10),
        ]);

        assert_eq!(c.module_spread(), 2);
    }

    #[test]
    fn the_report_carries_file_line_the_score_the_hash_and_the_code() {
        let c = cluster(vec![
            member(1, "parse", "src/a.rs", Some("alpha"), Visibility::Public, 3),
            member(2, "parse", "src/b.rs", Some("beta"), Visibility::Public, 3),
        ]);
        let short = c.short_hash();

        let out = render(&[c], 6, &mut |_| {
            Some("fn parse() {\n    todo!()\n}\n".into())
        });

        assert!(out.contains(&short), "the cluster hash:\n{out}");
        // 1-based display over 0-based storage: line_start 10 renders as 11.
        assert!(out.contains("`src/a.rs:11-13`"), "file:line:\n{out}");
        assert!(out.contains("`src/b.rs:11-13`"), "both members:\n{out}");
        assert!(out.contains("cosine 0.800"), "the score:\n{out}");
        assert!(out.contains("```rust"), "a fenced snippet:\n{out}");
        assert!(out.contains("fn parse() {"), "the code:\n{out}");
        assert!(out.contains("`alpha::parse`"), "the qualified name:\n{out}");
    }

    #[test]
    fn a_structural_finding_does_not_print_a_cosine_it_never_measured() {
        let c = Cluster {
            members: vec![
                member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 3),
                member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 3),
            ],
            evidence: Evidence::Structural { hamming: 4 },
            accepted: None,
        };

        let out = render(&[c], 6, &mut |_| None);

        assert!(out.contains("same shape, 4 bits apart"), "{out}");
        assert!(!out.contains("cosine"), "{out}");
    }

    #[test]
    fn a_body_the_reader_cannot_be_shown_prints_no_code_rather_than_wrong_code() {
        // The file changed under the index: the stored line range no longer
        // points at the symbol, so there is nothing honest to print.
        let c = cluster(vec![
            member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 3),
            member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 3),
        ]);

        let out = render(&[c], 6, &mut |_| None);

        assert!(!out.contains("```"), "no empty fence:\n{out}");
        assert!(
            out.contains("`src/a.rs:11-13`"),
            "the location still:\n{out}"
        );
    }

    #[test]
    fn a_long_body_is_elided_rather_than_pasted_whole() {
        let body = (0..100).fold(String::new(), |mut acc, i| {
            use std::fmt::Write;
            let _ = writeln!(acc, "line {i}");
            acc
        });
        let c = cluster(vec![
            member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 100),
            member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 100),
        ]);

        let out = render(&[c], 6, &mut |_| Some(body.clone()));

        assert!(out.contains("line 19"), "the head is shown:\n{out}");
        assert!(!out.contains("line 20"), "the tail is not:\n{out}");
        assert!(out.contains("… 80 more lines"), "{out}");
    }

    #[test]
    fn an_unknown_extension_opens_a_plain_fence_not_a_broken_one() {
        assert_eq!(fence_tag("src/a.rs"), "rust");
        assert_eq!(fence_tag("notes.xyz"), "");
        assert_eq!(fence_tag("Makefile"), "");
    }

    // --- the machine-readable surfaces ---

    /// The reason JSON exists at all: the prose spells the distance out in a
    /// sentence, and a caller that wants to bucket findings by it — where the
    /// report's signal actually lives — cannot parse a sentence.
    #[test]
    fn json_carries_the_distance_so_a_caller_can_bucket_on_it() {
        let near = Cluster {
            members: vec![
                member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 3),
                member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 3),
            ],
            evidence: Evidence::Structural { hamming: 0 },
            accepted: None,
        };
        let at_the_cut = Cluster {
            members: vec![
                member(3, "c", "src/c.rs", Some("gamma"), Visibility::Private, 3),
                member(4, "d", "src/d.rs", Some("delta"), Visibility::Private, 3),
            ],
            evidence: Evidence::Structural { hamming: 6 },
            accepted: None,
        };

        let out = render_json(&[near, at_the_cut], 6);
        let value: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");

        let findings = value["findings"].as_array().unwrap();
        assert_eq!(value["clusters"], 2);
        let distances: Vec<u64> = findings
            .iter()
            .map(|f| f["evidence"]["hamming"].as_u64().unwrap())
            .collect();
        assert_eq!(distances, vec![0, 6], "both buckets are distinguishable");
        assert_eq!(findings[0]["evidence"]["kind"], "structural");
        assert!(
            findings[0]["evidence"].get("similarity").is_none(),
            "a structural finding must not carry a cosine it never measured"
        );
    }

    #[test]
    fn json_reports_a_semantic_finding_as_a_cosine_and_no_hamming() {
        let out = render_json(
            &[cluster(vec![
                member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 3),
                member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 3),
            ])],
            6,
        );
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();

        let evidence = &value["findings"][0]["evidence"];
        assert_eq!(evidence["kind"], "semantic");
        assert!(evidence.get("hamming").is_none(), "{evidence}");
        assert!((evidence["similarity"].as_f64().unwrap() - 0.8).abs() < 1e-6);
    }

    /// Same 1-based display as the prose report: a consumer opening an editor
    /// at the number it was handed must land on the symbol.
    #[test]
    fn json_line_numbers_match_what_the_prose_report_prints() {
        let c = cluster(vec![
            member(1, "parse", "src/a.rs", Some("alpha"), Visibility::Public, 3),
            member(2, "parse", "src/b.rs", Some("beta"), Visibility::Public, 3),
        ]);
        let prose = render(std::slice::from_ref(&c), 6, &mut |_| None);
        let value: serde_json::Value = serde_json::from_str(&render_json(&[c], 6)).unwrap();

        assert!(prose.contains("`src/a.rs:11-13`"), "{prose}");
        assert_eq!(value["findings"][0]["members"][0]["line_start"], 11);
        assert_eq!(value["findings"][0]["members"][0]["line_end"], 13);
    }

    #[test]
    fn csv_writes_one_row_per_member_under_a_repeated_cluster_key() {
        let c = Cluster {
            members: vec![
                member(1, "parse", "src/a.rs", Some("alpha"), Visibility::Public, 3),
                member(2, "parse", "src/b.rs", Some("beta"), Visibility::Public, 3),
                member(3, "parse", "src/c.rs", Some("gamma"), Visibility::Public, 3),
            ],
            evidence: Evidence::Structural { hamming: 2 },
            accepted: None,
        };
        let hash = c.cluster_hash();

        let out = render_csv(&[c]);
        let rows: Vec<&str> = out.lines().collect();

        assert_eq!(rows.len(), 4, "a header and three members:\n{out}");
        assert!(rows[0].starts_with("cluster_hash,"), "{}", rows[0]);
        for row in &rows[1..] {
            assert!(row.starts_with(&format!("{hash},parse,3,")), "{row}");
        }
        assert!(
            rows[1].contains("src/a.rs,11,13,alpha::parse"),
            "{}",
            rows[1]
        );
        assert!(rows[3].contains("src/c.rs"), "{}", rows[3]);
    }

    /// One comma in a path would shift every column to its right for the rest
    /// of the row, and a reader has no way to notice.
    #[test]
    fn a_path_holding_a_comma_is_quoted_rather_than_shifting_the_columns() {
        let c = cluster(vec![
            member(1, "a", "src/od,d.rs", None, Visibility::Private, 3),
            member(2, "b", "src/b.rs", None, Visibility::Private, 3),
        ]);

        let out = render_csv(&[c]);
        let row = out.lines().nth(1).unwrap();

        assert!(row.contains("\"src/od,d.rs\""), "{row}");
        assert_eq!(
            row.matches(',').count() - 1,
            out.lines().next().unwrap().matches(',').count(),
            "the quoted comma must not count as a separator:\n{out}"
        );
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("plain"), "plain", "no gratuitous quoting");
    }

    #[test]
    fn the_machine_readable_surfaces_agree_with_the_prose_on_the_totals() {
        let clusters = vec![
            cluster(vec![
                member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
                member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 8),
            ]),
            cluster(vec![
                member(3, "c", "src/c.rs", Some("gamma"), Visibility::Public, 6),
                member(4, "d", "src/d.rs", Some("delta"), Visibility::Public, 4),
            ]),
        ];
        let prose = render(&clusters, 6, &mut |_| None);
        let value: serde_json::Value = serde_json::from_str(&render_json(&clusters, 6)).unwrap();

        // 8 duplicated in the first cluster, 4 in the second.
        assert!(
            prose.contains("2 clusters, 12 duplicated lines."),
            "{prose}"
        );
        assert_eq!(value["clusters"], 2);
        assert_eq!(value["duplicated_lines"], 12);
        assert_eq!(
            render_csv(&clusters).lines().count(),
            5,
            "a header and four members"
        );
    }

    #[test]
    fn no_clusters_renders_an_empty_payload_in_every_machine_format() {
        let value: serde_json::Value = serde_json::from_str(&render_json(&[], 6)).unwrap();

        assert_eq!(value["clusters"], 0);
        assert_eq!(value["duplicated_lines"], 0);
        assert!(value["findings"].as_array().unwrap().is_empty());
        assert!(value["buckets"].as_array().unwrap().is_empty());
        assert_eq!(render_csv(&[]).lines().count(), 1, "the header alone");
    }

    #[test]
    fn an_empty_report_says_so_instead_of_printing_a_bare_heading() {
        let out = render(&[], 6, &mut |_| None);

        assert!(out.contains("No clusters found"), "{out}");
    }

    #[test]
    fn ranking_two_identical_clusters_is_deterministic() {
        // Same spread, reach and line count: only the hash separates them, and
        // it must separate them the same way every run.
        let build = || {
            vec![
                cluster(vec![
                    member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
                    member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 10),
                ]),
                cluster(vec![
                    member(3, "c", "src/c.rs", Some("gamma"), Visibility::Public, 10),
                    member(4, "d", "src/d.rs", Some("delta"), Visibility::Public, 10),
                ]),
            ]
        };
        let mut first = build();
        let mut second = build();
        second.reverse();

        rank(&mut first, 6);
        rank(&mut second, 6);

        assert_eq!(first[0].cluster_hash(), second[0].cluster_hash());
        assert_eq!(first[1].cluster_hash(), second[1].cluster_hash());
    }

    // --- buckets ---

    #[test]
    fn bucket_boundaries_match_the_hand_classified_bands() {
        let structural = |hamming| Evidence::Structural { hamming };
        let cut = 6;

        assert_eq!(structural(0).bucket(cut), Bucket::Zero);
        assert_eq!(structural(1).bucket(cut), Bucket::OneToThree);
        assert_eq!(structural(3).bucket(cut), Bucket::OneToThree);
        assert_eq!(structural(4).bucket(cut), Bucket::Four);
        assert_eq!(structural(5).bucket(cut), Bucket::Five);
        assert_eq!(structural(6).bucket(cut), Bucket::AtCut);
        assert_eq!(
            Evidence::Semantic { similarity: 0.9 }.bucket(cut),
            Bucket::Cosine,
            "a semantic finding never reads the cut"
        );
    }

    /// `hamming == cut` is checked before the named bands, so a cut configured
    /// inside 1..=5 still claims that exact distance for "at cut" rather than
    /// losing it to the band it would otherwise land in.
    #[test]
    fn a_cut_configured_inside_a_named_band_still_wins_that_distance() {
        assert_eq!(Evidence::Structural { hamming: 3 }.bucket(3), Bucket::AtCut);
        assert_eq!(
            Evidence::Structural { hamming: 2 }.bucket(3),
            Bucket::OneToThree,
            "still strictly under the cut"
        );
    }

    /// Only reachable when `cut` itself is configured above 6: clustering
    /// bounds every member's distance to `cut` by construction, so nothing
    /// past the named bands was ever part of the hand-classified sample.
    #[test]
    fn a_distance_past_every_named_band_falls_back_to_at_cut() {
        assert_eq!(
            Evidence::Structural { hamming: 7 }.bucket(10),
            Bucket::AtCut
        );
    }

    #[test]
    fn the_bucket_table_rows_sum_to_the_headline_totals() {
        let clusters = vec![
            Cluster {
                members: vec![
                    member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
                    member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 10),
                ],
                evidence: Evidence::Structural { hamming: 0 },
                accepted: None,
            },
            Cluster {
                members: vec![
                    member(3, "c", "src/c.rs", Some("gamma"), Visibility::Public, 10),
                    member(4, "d", "src/d.rs", Some("delta"), Visibility::Public, 8),
                ],
                evidence: Evidence::Structural { hamming: 6 },
                accepted: None,
            },
            cluster(vec![
                member(5, "e", "src/e.rs", Some("epsilon"), Visibility::Public, 10),
                member(6, "f", "src/f.rs", Some("zeta"), Visibility::Public, 6),
            ]),
        ];

        let out = render(&clusters, 6, &mut |_| None);
        let table_start = out
            .find("| bucket |")
            .unwrap_or_else(|| panic!("a bucket table:\n{out}"));
        let rows: Vec<&str> = out[table_start..]
            .lines()
            .skip(2) // header row, then the `|---|---:|---:|` separator
            .take_while(|line| line.starts_with('|'))
            .collect();

        let mut cluster_sum = 0usize;
        let mut line_sum = 0u32;
        for row in &rows {
            let cols: Vec<&str> = row.trim_matches('|').split('|').map(str::trim).collect();
            cluster_sum += cols[1].parse::<usize>().expect(row);
            line_sum += cols[2].parse::<u32>().expect(row);
        }

        assert_eq!(rows.len(), 3, "one row per bucket that got a hit:\n{out}");
        assert_eq!(cluster_sum, clusters.len(), "{out}");
        assert_eq!(line_sum, total_lines(&clusters), "{out}");
        assert!(out.contains("3 clusters, 24 duplicated lines."), "{out}");
    }

    #[test]
    fn json_carries_the_same_bucket_summary_as_the_table() {
        let clusters = vec![
            Cluster {
                members: vec![
                    member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
                    member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 10),
                ],
                evidence: Evidence::Structural { hamming: 0 },
                accepted: None,
            },
            Cluster {
                members: vec![
                    member(3, "c", "src/c.rs", Some("gamma"), Visibility::Public, 10),
                    member(4, "d", "src/d.rs", Some("delta"), Visibility::Public, 8),
                ],
                evidence: Evidence::Structural { hamming: 6 },
                accepted: None,
            },
        ];

        let value: serde_json::Value = serde_json::from_str(&render_json(&clusters, 6)).unwrap();
        let buckets = value["buckets"].as_array().unwrap();

        assert_eq!(buckets.len(), 2, "{buckets:?}");
        assert_eq!(buckets[0]["bucket"], "0");
        assert_eq!(buckets[0]["clusters"], 1);
        assert_eq!(buckets[0]["duplicated_lines"], 10);
        assert_eq!(buckets[1]["bucket"], "at cut");
        assert_eq!(buckets[1]["clusters"], 1);
        assert_eq!(buckets[1]["duplicated_lines"], 8);
    }

    /// Both surfaces bucket against the cut the scan ran with, not against the
    /// default constant. A repository that lowered
    /// `code.duplication.hamming_threshold` to 4 has its 4-bit clusters *at*
    /// its cut; counting them in the "4" band would print a table that
    /// disagrees with the order [`rank`] gave the same clusters.
    #[test]
    fn both_surfaces_bucket_against_the_configured_cut_not_the_default() {
        let clusters = vec![Cluster {
            members: vec![
                member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
                member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 8),
            ],
            evidence: Evidence::Structural { hamming: 4 },
            accepted: None,
        }];

        let prose = render(&clusters, 4, &mut |_| None);
        assert!(prose.contains("| at cut | 1 | 8 |"), "{prose}");
        assert!(!prose.contains("| 4 |"), "not the default band:\n{prose}");

        let value: serde_json::Value = serde_json::from_str(&render_json(&clusters, 4)).unwrap();
        assert_eq!(value["buckets"][0]["bucket"], "at cut");

        // The same clusters under the default cut land in the "4" band instead.
        let value: serde_json::Value = serde_json::from_str(&render_json(&clusters, 6)).unwrap();
        assert_eq!(value["buckets"][0]["bucket"], "4");
    }

    /// The whole point of the change: the bucket a finding falls in decides
    /// the order before spread does, so a trustworthy single-module finding
    /// is not buried under a noisy one just because the noisy one spreads
    /// wider.
    #[test]
    fn a_0_bit_single_module_cluster_outranks_a_6_bit_12_module_one() {
        let trustworthy = Cluster {
            members: vec![
                member(
                    1,
                    "trustworthy",
                    "src/a.rs",
                    Some("alpha"),
                    Visibility::Private,
                    10,
                ),
                member(
                    2,
                    "trustworthy",
                    "src/a.rs",
                    Some("alpha"),
                    Visibility::Private,
                    10,
                ),
            ],
            evidence: Evidence::Structural { hamming: 0 },
            accepted: None,
        };
        let noisy = Cluster {
            members: (0..12)
                .map(|i| {
                    let file = format!("src/m{i}.rs");
                    let module = format!("mod{i}");
                    member(
                        100 + i,
                        "noisy",
                        &file,
                        Some(&module),
                        Visibility::Public,
                        10,
                    )
                })
                .collect(),
            evidence: Evidence::Structural { hamming: 6 },
            accepted: None,
        };
        assert!(
            noisy.module_spread() > trustworthy.module_spread(),
            "the noisy cluster must actually be the one that spreads wider"
        );

        let mut clusters = vec![noisy, trustworthy];
        rank(&mut clusters, 6);

        assert_eq!(
            clusters[0].members[0].name, "trustworthy",
            "bucket must win over spread"
        );
    }

    // --- the ignore-list ---

    use crate::store::memory::{
        EntryStatus, EntryType, MemoryEntry, SourceType, add_entry, get_entry_without_tracking,
    };
    use crate::store::memory_graph::{MemoryRelation, TargetKind, add_edge};
    use rusqlite::Connection;

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::store::schema::init_schema(&conn).unwrap();
        conn
    }

    fn decision(id: &str, tags: Vec<String>, entry_type: EntryType) -> MemoryEntry {
        let now = chrono::Utc::now().timestamp();
        MemoryEntry {
            id: id.to_string(),
            title: "Accepted duplication".to_string(),
            content: "Two adapters, deliberately not shared.".to_string(),
            entry_type,
            tags,
            status: EntryStatus::Active,
            created_at: now,
            updated_at: now,
            superseded_by: None,
            access_count: 0,
            last_accessed: None,
            source_path: None,
            confirmations: 0,
            corrections: 0,
            last_confirmed_at: None,
            last_refuted_at: None,
            source_type: SourceType::UserStatement,
            expires_at: None,
            due_at: None,
        }
    }

    fn ignored_cluster() -> Cluster {
        cluster(vec![
            member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
            member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 10),
        ])
    }

    /// Accept `cluster`, recording its membership the way [`ignore_cluster`]
    /// does. Written through `add_entry` rather than through the store's own
    /// entry point so the test needs no `Context`; the round trip through the
    /// real writer is covered by
    /// `an_accepted_cluster_is_written_as_a_tagged_decision_that_says_why`.
    fn accept(conn: &Connection, cluster: &Cluster) -> String {
        let id = ignore_entry_id(&cluster.cluster_hash());
        let members: Vec<String> = cluster.member_keys().into_iter().collect();
        let mut entry = decision(&id, vec![IGNORE_TAG.to_string()], EntryType::Decision);
        entry.content = format!(
            "Two adapters, deliberately not shared.\n\n{MEMBERS_FENCE}\n{}\n```\n",
            members.join("\n")
        );
        add_entry(conn, &entry).unwrap();
        id
    }

    /// Losing a member must not undo the decision.
    ///
    /// The digest was taken over the whole membership, so deleting one copy
    /// produced a different one, the ignore stopped matching, and the finding
    /// somebody had already dismissed came back as new. Subset matching is the
    /// fix: the two copies that are left are still copies they accepted.
    #[test]
    fn a_member_that_went_away_leaves_the_decision_standing() {
        let conn = memory_db();
        let reviewed = cluster(vec![
            member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
            member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 10),
            member(3, "c", "src/c.rs", Some("gamma"), Visibility::Public, 10),
        ]);
        accept(&conn, &reviewed);

        let mut shrunk = reviewed.clone();
        shrunk.members.pop();
        assert_ne!(
            shrunk.cluster_hash(),
            reviewed.cluster_hash(),
            "the fixture is only meaningful if the digest did change"
        );

        assert!(is_ignored(&conn, &shrunk).unwrap());
        assert!(filter_ignored(&conn, vec![shrunk]).unwrap().is_empty());
    }

    /// A cluster that split into halves is still the cluster that was accepted.
    #[test]
    fn a_split_leaves_both_halves_accepted() {
        let conn = memory_db();
        let reviewed = cluster(vec![
            member(1, "a", "src/a.rs", Some("alpha"), Visibility::Public, 10),
            member(2, "b", "src/b.rs", Some("beta"), Visibility::Public, 10),
            member(3, "c", "src/c.rs", Some("gamma"), Visibility::Public, 10),
            member(4, "d", "src/d.rs", Some("delta"), Visibility::Public, 10),
        ]);
        accept(&conn, &reviewed);

        let left = cluster(reviewed.members[..2].to_vec());
        let right = cluster(reviewed.members[2..].to_vec());

        assert!(
            filter_ignored(&conn, vec![left, right]).unwrap().is_empty(),
            "both halves were reviewed, so both stay accepted"
        );
    }

    /// Gaining a member brings the cluster back, and says what is new.
    ///
    /// Nobody has looked at the new copy. Inheriting an acceptance it was never
    /// part of is the failure mode an ignore-list exists to prevent — but
    /// showing the whole finding again wastes the review that did happen, so
    /// the report carries what was accepted and marks only the addition.
    #[test]
    fn a_member_that_appeared_brings_the_cluster_back_labelled_changed() {
        let conn = memory_db();
        let reviewed = ignored_cluster();
        let id = accept(&conn, &reviewed);

        let mut grown = reviewed.clone();
        grown.members.push(member(
            3,
            "c",
            "src/c.rs",
            Some("gamma"),
            Visibility::Public,
            10,
        ));

        assert!(!is_ignored(&conn, &grown).unwrap());
        let kept = filter_ignored(&conn, vec![grown]).unwrap();
        assert_eq!(kept.len(), 1, "it has to resurface");
        let back = &kept[0];
        let snapshot = back.accepted.as_ref().expect("labelled as changed");
        assert_eq!(snapshot.entry_id, id);
        assert_eq!(
            back.added_members()
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>(),
            vec!["c"],
            "only the member nobody reviewed"
        );

        let out = render(kept.as_slice(), 6, &mut |_| None);
        assert!(out.contains("**Changed since accepted**"), "{out}");
        assert!(out.contains(&format!("`{id}`")), "{out}");
        assert!(out.contains("**NEW** `src/c.rs:"), "the addition:\n{out}");
        assert!(
            !out.contains("**NEW** `src/a.rs:"),
            "an already-reviewed member must not be marked:\n{out}"
        );
    }

    /// An entry with no recorded membership keeps working as it always did.
    ///
    /// Somebody may have written one by hand, and every entry written before
    /// the membership was stored is of that shape. It can only speak for the
    /// cluster its id came from, which is exactly the old behaviour.
    #[test]
    fn an_entry_without_a_recorded_membership_still_matches_by_id() {
        let conn = memory_db();
        let c = ignored_cluster();
        add_entry(
            &conn,
            &decision(
                &ignore_entry_id(&c.cluster_hash()),
                vec![IGNORE_TAG.to_string()],
                EntryType::Decision,
            ),
        )
        .unwrap();

        assert!(is_ignored(&conn, &c).unwrap());
        // ...and for nothing else. A snapshot-less entry must not start
        // suppressing clusters it was never about.
        let other = cluster(vec![member(
            9,
            "z",
            "src/z.rs",
            Some("zeta"),
            Visibility::Public,
            10,
        )]);
        assert!(!is_ignored(&conn, &other).unwrap());
    }

    /// A prefix of the identity is accepted; an ambiguous one is not.
    ///
    /// The report prints twelve characters and that is what a reader retypes,
    /// but somebody who copied ten, or all sixty-four, meant the same cluster.
    /// Two entries whose prefixes both fit is a question the code cannot
    /// answer, so it declines to suppress — a lost finding is silent, an extra
    /// one is a line.
    #[test]
    fn an_unambiguous_prefix_is_accepted_and_an_ambiguous_one_is_not() {
        let c = ignored_cluster();
        let hash = c.cluster_hash();
        let untyped = |p: &str| Accepted {
            entry_id: format!("{IGNORE_ID_PREFIX}{p}"),
            members: None,
        };

        for len in [8, 12, 16, 64] {
            let prefix: String = hash.chars().take(len).collect();
            assert_eq!(
                verdict(&[untyped(&prefix)], &c),
                Verdict::Accepted,
                "a {len}-character prefix names one cluster"
            );
        }

        // Two prefixes of the same hash: the shorter cannot be told from the
        // longer, so neither decides.
        let ten: String = hash.chars().take(10).collect();
        let twelve: String = hash.chars().take(12).collect();
        assert_eq!(
            verdict(&[untyped(&ten), untyped(&twelve)], &c),
            Verdict::Report,
            "ambiguous, so it fails open"
        );

        // A prefix of something else names nothing here.
        assert_eq!(verdict(&[untyped("ffffffffffff")], &c), Verdict::Report);
        assert_eq!(
            verdict(&[untyped("")], &c),
            Verdict::Report,
            "an empty prefix matches every cluster and must match none"
        );
    }

    /// The member key separates what `(module_path, name)` could not.
    ///
    /// Two overloads share a module and a name; a file rename keeps both while
    /// moving the symbol somewhere else entirely. Either collision means one
    /// accepted cluster silently suppresses a different finding.
    #[test]
    fn the_member_key_separates_overloads_and_follows_a_rename() {
        let base = member(
            1,
            "parse",
            "src/a.rs",
            Some("alpha"),
            Visibility::Public,
            10,
        );

        let mut overload = base.clone();
        overload.signature = Some("fn parse(s: &str) -> u32".to_string());
        assert_ne!(member_key(&base), member_key(&overload), "overloads");

        let mut renamed = base.clone();
        renamed.file_path = "src/b.rs".to_string();
        assert_ne!(member_key(&base), member_key(&renamed), "a renamed file");

        let mut other_language = base.clone();
        other_language.language = Some("python".to_string());
        assert_ne!(member_key(&base), member_key(&other_language), "language");

        let mut method = base.clone();
        method.kind = "Method".to_string();
        assert_ne!(member_key(&base), member_key(&method), "kind");
    }

    #[test]
    fn an_active_decision_filters_the_cluster_out_of_the_report() {
        let conn = memory_db();
        let c = ignored_cluster();
        add_entry(
            &conn,
            &decision(
                &ignore_entry_id(&c.cluster_hash()),
                vec![IGNORE_TAG.to_string()],
                EntryType::Decision,
            ),
        )
        .unwrap();

        assert!(is_ignored(&conn, &c).unwrap());
        assert!(filter_ignored(&conn, vec![c]).unwrap().is_empty());
    }

    #[test]
    fn a_superseded_decision_lets_the_cluster_come_back() {
        // The whole reason this lives in the memory store: a decision that
        // stopped holding stops hiding the finding, with no separate cleanup.
        let conn = memory_db();
        let c = ignored_cluster();
        let id = ignore_entry_id(&c.cluster_hash());
        add_entry(
            &conn,
            &decision(&id, vec![IGNORE_TAG.to_string()], EntryType::Decision),
        )
        .unwrap();
        add_entry(
            &conn,
            &decision(
                "newer-decision",
                vec![IGNORE_TAG.to_string()],
                EntryType::Decision,
            ),
        )
        .unwrap();

        add_edge(
            &conn,
            "newer-decision",
            &id,
            TargetKind::Memory,
            MemoryRelation::Supersedes,
        )
        .unwrap();

        assert_eq!(
            get_entry_without_tracking(&conn, &id)
                .unwrap()
                .unwrap()
                .status,
            EntryStatus::Superseded,
            "the edge flipped the status"
        );
        assert!(!is_ignored(&conn, &c).unwrap());
        assert_eq!(filter_ignored(&conn, vec![c]).unwrap().len(), 1);
    }

    #[test]
    fn a_cluster_nobody_accepted_is_not_filtered() {
        let conn = memory_db();

        assert_eq!(
            filter_ignored(&conn, vec![ignored_cluster()])
                .unwrap()
                .len(),
            1
        );
    }

    /// Suppression fails open here for the same reason it does in the
    /// call-graph pass: an entry that is not an accepted-duplication decision
    /// must not silently delete a finding.
    #[test]
    fn an_unrelated_entry_holding_the_id_does_not_hide_the_finding() {
        let conn = memory_db();
        let c = ignored_cluster();
        let id = ignore_entry_id(&c.cluster_hash());

        add_entry(
            &conn,
            &decision(&id, vec!["unrelated".into()], EntryType::Decision),
        )
        .unwrap();
        assert!(!is_ignored(&conn, &c).unwrap(), "wrong tag");

        crate::store::memory::update_entry(
            &conn,
            &decision(&id, vec![IGNORE_TAG.to_string()], EntryType::Topic),
        )
        .unwrap();
        assert!(!is_ignored(&conn, &c).unwrap(), "wrong type");
    }

    /// The id follows the cluster, not the ids or lines inside it — so an
    /// accepted cluster stays accepted across a reparse.
    #[test]
    fn the_ignore_id_is_namespaced_and_follows_the_cluster_hash() {
        let c = ignored_cluster();

        assert_eq!(
            ignore_entry_id("a1b2c3d4e5f60000ff"),
            "dup-ignore-a1b2c3d4e5f6",
            "the id carries the printable prefix, which is what a reader retypes"
        );
        assert!(ignore_entry_id(&c.cluster_hash()).ends_with(&c.short_hash()));
        assert_ne!(
            ignore_entry_id(&c.cluster_hash()),
            c.cluster_hash(),
            "a bare hash could collide with an id a human picked"
        );
    }

    /// The write path, end to end through the store's own entry point.
    ///
    /// `auto_embed_memory` is switched off in the config the context reads, so
    /// the test exercises the write without loading an ONNX model — the same
    /// hermetic switch the store's own tests use.
    #[test]
    fn an_accepted_cluster_is_written_as_a_tagged_decision_that_says_why() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join(".mdkb")).unwrap();
        std::fs::write(
            root.path().join(".mdkb/config.toml"),
            "[search]\nauto_embed_memory = false\n",
        )
        .unwrap();
        let ctx = crate::core::Context::open(root.path()).unwrap();
        let c = ignored_cluster();

        let id = ignore_cluster(&ctx, &c, "Two adapters, deliberately not shared.").unwrap();

        assert_eq!(id, ignore_entry_id(&c.cluster_hash()));
        let entry = get_entry_without_tracking(&ctx.conn, &id).unwrap().unwrap();
        assert_eq!(entry.entry_type, EntryType::Decision);
        assert!(
            entry.tags.iter().any(|t| t == IGNORE_TAG),
            "{:?}",
            entry.tags
        );
        assert_eq!(entry.status, EntryStatus::Active);
        assert_eq!(entry.source_type, SourceType::UserStatement);
        assert!(
            entry
                .content
                .contains("Two adapters, deliberately not shared."),
            "the rationale:\n{}",
            entry.content
        );
        assert!(
            entry.content.contains("src/a.rs:11"),
            "where:\n{}",
            entry.content
        );
        // And what it wrote is what the filter reads — including the
        // membership, which is the half a hand-edited entry can lose.
        assert_eq!(
            parse_members(&entry.content),
            Some(c.member_keys()),
            "the block the writer emits must be the block the matcher parses:\n{}",
            entry.content
        );
        assert!(is_ignored(&ctx.conn, &c).unwrap());
    }

    /// The ignore-list adds no schema and writes no SQL of its own.
    ///
    /// It goes through the store's API, so the entry gets the projection,
    /// index and revision history every other decision gets. A hand-written
    /// INSERT here would produce a row the rest of mdkb does not know about.
    #[test]
    fn the_ignore_list_reuses_the_memory_store_rather_than_adding_to_it() {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/code/duplication/report.rs"),
        )
        .unwrap();
        let code = source.split("#[cfg(test)]").next().unwrap();

        for sql in [
            "CREATE TABLE",
            "ALTER TABLE",
            "INSERT INTO",
            "UPDATE ",
            "memory_entries",
        ] {
            assert!(!code.contains(sql), "report.rs writes its own SQL: {sql}");
        }
    }

    #[test]
    fn filtering_keeps_the_clusters_nobody_accepted_and_their_order() {
        let conn = memory_db();
        let accepted = ignored_cluster();
        let other = cluster(vec![
            member(3, "c", "src/c.rs", Some("gamma"), Visibility::Public, 10),
            member(4, "d", "src/d.rs", Some("delta"), Visibility::Public, 10),
        ]);
        add_entry(
            &conn,
            &decision(
                &ignore_entry_id(&accepted.cluster_hash()),
                vec![IGNORE_TAG.to_string()],
                EntryType::Decision,
            ),
        )
        .unwrap();

        let kept = filter_ignored(&conn, vec![accepted, other.clone()]).unwrap();

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].cluster_hash(), other.cluster_hash());
    }

    #[test]
    fn an_empty_cluster_renders_instead_of_panicking() {
        // Not expected from the pipeline, which drops singletons — but a
        // renderer that panics turns a bad cluster into no report at all.
        let c = cluster(Vec::new());

        let out = render(&[c], 6, &mut |_| None);

        assert!(out.contains("(empty)"), "{out}");
    }
}
