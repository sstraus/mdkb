//! The dependency direction, asserted mechanically.
//!
//! Story 019-3248: the shared application layer lived in `cli::handlers`, so the
//! MCP and daemon adapters depended on the command-line adapter. The CLI was the
//! de-facto core of the program and every layer above it had its dependency
//! direction inverted.
//!
//! A grep is the right tool here rather than a type-level boundary: Rust has no
//! way to say "this module may not be named from that one" within a crate, and
//! the failure mode is someone reaching for a convenient function, which a
//! compile error would catch but nothing else does. The check reads source, so
//! it fails at the moment the reference is written rather than at review time.

use std::path::Path;

/// Source files under `dir`, recursively.
fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// Everything before `mod tests`. Test code may reach for CLI helpers to build
/// a fixture; production code may not, and conflating the two would either
/// weaken the rule or forbid something harmless.
fn production_source(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    match text.find("\nmod tests {") {
        Some(i) => text[..i].to_string(),
        None => text,
    }
}

/// `#[ignore]`d because it currently FAILS, on purpose.
///
/// It is the executable definition of "done" for story 019-3248, written before
/// the work rather than after, so the target is a command anyone can run rather
/// than a paragraph someone has to interpret:
///
/// ```text
/// cargo test --test layering -- --ignored
/// ```
///
/// Five references remain, covering seven symbols: `handle_update`,
/// `handle_session_index`, `handle_hybrid_search`, `handle_mget`,
/// `hybrid_search_fts`, `sync_memory_files` and
/// `projection_file_and_row_counts`. Unlike `Context`, these are not a
/// mechanical move — `handle_update` is the whole indexing pipeline and the
/// story requires it be split by responsibility, not relocated wholesale. Remove
/// the `#[ignore]` in the commit that empties the list.
#[test]
fn no_adapter_reaches_into_the_cli_adapter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src/mcp"), &mut files);
    rust_files(&root.join("src/daemon"), &mut files);
    assert!(
        !files.is_empty(),
        "the check must actually find source files"
    );

    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        for (i, line) in production_source(file).lines().enumerate() {
            // Skip doc comments and ordinary comments: naming the old location
            // while explaining why something moved is exactly the kind of note
            // that should survive.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            if line.contains("cli::handlers") {
                offenders.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(root).unwrap_or(file).display(),
                    i + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "src/mcp and src/daemon must not depend on the command-line adapter — \
         shared application logic belongs in `crate::core`. Offenders:\n{}",
        offenders.join("\n")
    );
}
