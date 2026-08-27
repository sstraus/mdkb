//! Where a symbol sits in its language's namespace.
//!
//! A relationship resolved by bare name matches every symbol of that name in the
//! repository. `module_path` is the address that tells those apart, and it is
//! also what an import has to be matched against.
//!
//! Two halves make one address, and which half carries it depends on the
//! language:
//!
//! - **Declared.** The source states the namespace — `package` in Java, Kotlin
//!   and PHP, `namespace` in C# and C++, `class_name` in GDScript. Every parser
//!   for those languages already reads it and threads it down the walk, so the
//!   whole address arrives from PARSE and the path says nothing.
//! - **Derived from the path.** Rust, Python, TypeScript, JavaScript and Go have
//!   no statement to read (or, for Go, none that gives the import path), so the
//!   file's position *is* the address. Those parsers thread an empty string,
//!   which is the room left for in-file nesting to extend the derived base.
//!
//! [`module_path_for`] takes both halves and returns the one address.

use crate::code::parsing::language::Language;

/// The address of a symbol found in `rel_path`, where `declared` is whatever
/// namespace its parser read out of the source (empty when there is none).
///
/// `rel_path` is relative to the indexed root and uses `/` separators.
pub fn module_path_for(language: Language, rel_path: &str, declared: &str) -> Option<Box<str>> {
    let declared = (!declared.is_empty()).then_some(declared);
    match namespace_of(language, rel_path) {
        // The source names the namespace in full, or names none at all.
        Namespace::Declared => declared.map(Into::into),
        Namespace::Path {
            base,
            nesting_separator,
        } => match declared {
            Some(nesting) => Some(format!("{base}{nesting_separator}{nesting}").into()),
            None => Some(base.into()),
        },
    }
}

/// Which half of the address a language's files carry.
enum Namespace {
    /// The parser holds the whole address, so the path contributes nothing.
    Declared,
    /// The path holds the address; anything the parser tracks nests inside it,
    /// joined by `nesting_separator`.
    Path {
        base: String,
        nesting_separator: &'static str,
    },
}

fn namespace_of(language: Language, rel_path: &str) -> Namespace {
    let rel_path = rel_path.trim_start_matches("./");
    match language {
        Language::Java
        | Language::Kotlin
        | Language::CSharp
        | Language::Php
        | Language::Cpp
        | Language::Gdscript => Namespace::Declared,

        Language::Rust => Namespace::Path {
            base: rust_module_path(rel_path),
            nesting_separator: "::",
        },
        Language::Python => Namespace::Path {
            base: python_module_path(rel_path),
            nesting_separator: ".",
        },
        // A module specifier resolves to the path without its extension, so that
        // is the address. Go is here rather than under `Declared` because its
        // `package` clause is not what an import writes: `import ".../internal/store"`
        // names the directory, and the clause only names how call sites abbreviate it.
        Language::TypeScript | Language::JavaScript => Namespace::Path {
            base: strip_extension(rel_path).to_string(),
            nesting_separator: ".",
        },
        Language::Go => Namespace::Path {
            base: parent_dir(rel_path).to_string(),
            nesting_separator: ".",
        },
        // Swift, C and Lua have no module system this index can resolve against,
        // and their parsers declare nothing. The file is the only address there
        // is; it still separates two same-named symbols in different files.
        Language::Swift | Language::C | Language::Lua => Namespace::Path {
            base: rel_path.to_string(),
            nesting_separator: ".",
        },
    }
}

/// `src/a/b.rs` and `src/a/b/mod.rs` are both `crate::a::b`; `src/lib.rs` and
/// `src/main.rs` are `crate` itself.
///
/// A file outside `src/` belongs to its own crate — `tests/x.rs`, `benches/x.rs`
/// and `examples/x.rs` are each compiled separately — so the target directory is
/// dropped and the file names the crate root.
fn rust_module_path(rel_path: &str) -> String {
    let stripped = rel_path
        .strip_prefix("src/")
        .or_else(|| {
            rel_path
                .split_once('/')
                .filter(|(dir, _)| matches!(*dir, "tests" | "benches" | "examples"))
                .map(|(_, rest)| rest)
        })
        .unwrap_or(rel_path);

    let segments: Vec<&str> = strip_extension(stripped)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    // `lib`, `main` and `mod` name the module they sit in, not a module of it.
    let segments: Vec<&str> = match segments.split_last() {
        Some((last, head)) if matches!(*last, "lib" | "main" | "mod") => head.to_vec(),
        _ => segments,
    };

    if segments.is_empty() {
        "crate".to_string()
    } else {
        format!("crate::{}", segments.join("::"))
    }
}

/// `pkg/sub/mod.py` is `pkg.sub.mod`, and `pkg/__init__.py` is `pkg`.
///
/// A leading `src/` is dropped: under the src layout the package root is `src/`,
/// and an import says `pkg.mod`, never `src.pkg.mod`.
///
/// DEFERRED (2026-08-26) — the design says to trim up to the highest directory
/// holding an `__init__.py`, which needs filesystem access this stage does not
/// have. The rule below is exact for a repository root or `src/` package root,
/// which is the standard layout; a package nested deeper produces a longer path
/// than its imports use, so those imports do not narrow. That costs recall, not
/// correctness.
fn python_module_path(rel_path: &str) -> String {
    let stripped = rel_path.strip_prefix("src/").unwrap_or(rel_path);
    let segments: Vec<&str> = strip_extension(stripped)
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let segments = match segments.split_last() {
        Some((last, head)) if *last == "__init__" => head.to_vec(),
        _ => segments,
    };
    segments.join(".")
}

/// The directory holding `rel_path`, empty for a file at the root.
fn parent_dir(rel_path: &str) -> &str {
    match rel_path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

/// The path without its final extension. `a/b.tar.gz` keeps `a/b.tar`, and a
/// dot in a directory name is left alone.
fn strip_extension(path: &str) -> &str {
    match path.rsplit_once('.') {
        Some((head, ext)) if !ext.contains('/') && !head.is_empty() => head,
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rust_module_is_addressed_from_the_crate_root() {
        for (path, expected) in [
            ("src/lib.rs", "crate"),
            ("src/main.rs", "crate"),
            (
                "src/code/parsing/rust/parser.rs",
                "crate::code::parsing::rust::parser",
            ),
            ("src/code/indexing/mod.rs", "crate::code::indexing"),
        ] {
            assert_eq!(
                module_path_for(Language::Rust, path, "").as_deref(),
                Some(expected),
                "{path}"
            );
        }
    }

    /// A file and the `mod.rs` of the directory named after it are the same
    /// module. Addressing them differently would split one module in two and
    /// make an import match only half of it.
    #[test]
    fn a_rust_file_and_its_mod_rs_form_share_one_address() {
        assert_eq!(
            module_path_for(Language::Rust, "src/a/b.rs", ""),
            module_path_for(Language::Rust, "src/a/b/mod.rs", "")
        );
    }

    /// Each integration test, bench and example is its own crate, so the target
    /// directory is not part of any address.
    #[test]
    fn a_rust_target_directory_is_not_part_of_the_address() {
        for dir in ["tests", "benches", "examples"] {
            assert_eq!(
                module_path_for(Language::Rust, &format!("{dir}/e2e_hooks.rs"), "").as_deref(),
                Some("crate::e2e_hooks"),
                "{dir}"
            );
        }
    }

    /// `mod inner { }` puts its members below the file's own address, not beside
    /// it — otherwise every file's `tests` module would share one address.
    #[test]
    fn in_file_nesting_extends_the_derived_address() {
        assert_eq!(
            module_path_for(Language::Rust, "src/a/b.rs", "inner::deeper").as_deref(),
            Some("crate::a::b::inner::deeper")
        );
        assert_eq!(
            module_path_for(Language::Python, "pkg/mod.py", "Outer").as_deref(),
            Some("pkg.mod.Outer")
        );
    }

    #[test]
    fn a_python_module_is_addressed_by_dots() {
        for (path, expected) in [
            ("pkg/sub/mod.py", "pkg.sub.mod"),
            ("pkg/__init__.py", "pkg"),
            ("src/pkg/mod.py", "pkg.mod"),
            ("app.py", "app"),
        ] {
            assert_eq!(
                module_path_for(Language::Python, path, "").as_deref(),
                Some(expected),
                "{path}"
            );
        }
    }

    #[test]
    fn a_typescript_module_is_addressed_by_its_path() {
        assert_eq!(
            module_path_for(Language::TypeScript, "src/app/store.ts", "").as_deref(),
            Some("src/app/store")
        );
        assert_eq!(
            module_path_for(Language::JavaScript, "lib/util.mjs", "").as_deref(),
            Some("lib/util")
        );
    }

    /// A Go import names the directory, not the file and not the package clause.
    #[test]
    fn a_go_package_is_addressed_by_its_directory() {
        assert_eq!(
            module_path_for(Language::Go, "internal/store/db.go", "").as_deref(),
            Some("internal/store")
        );
        assert_eq!(
            module_path_for(Language::Go, "main.go", "").as_deref(),
            Some("")
        );
    }

    /// What the source declares is the whole address: joining it to the path
    /// would invent a namespace no import could match.
    #[test]
    fn a_declared_namespace_stands_alone() {
        for language in [
            Language::Java,
            Language::Kotlin,
            Language::CSharp,
            Language::Php,
            Language::Cpp,
            Language::Gdscript,
        ] {
            assert_eq!(
                module_path_for(language, "deep/nested/Thing.ext", "com.acme.thing").as_deref(),
                Some("com.acme.thing"),
                "{language:?}"
            );
            assert_eq!(
                module_path_for(language, "deep/nested/Thing.ext", ""),
                None,
                "{language:?} declares nothing, so it has no address"
            );
        }
    }

    /// No module system to resolve against, but the file still tells two
    /// same-named symbols apart.
    #[test]
    fn a_language_without_a_module_system_is_addressed_by_its_file() {
        for (language, path) in [
            (Language::Swift, "Sources/App/View.swift"),
            (Language::C, "src/util.h"),
            (Language::Lua, "lib/init.lua"),
        ] {
            assert_eq!(
                module_path_for(language, path, "").as_deref(),
                Some(path),
                "{language:?}"
            );
        }
    }
}
