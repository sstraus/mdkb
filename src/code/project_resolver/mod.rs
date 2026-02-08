//! Project-level import path resolution.
//!
//! Each language has a [`ProjectResolver`] implementation that reads
//! project configuration files (tsconfig.json, go.mod, etc.) and
//! resolves import paths to filesystem paths.

pub mod csharp;
pub mod go;
pub mod java;
pub mod javascript;
pub mod kotlin;
pub mod php;
pub mod python;
pub mod swift;
pub mod typescript;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;

/// A resolved import path mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMapping {
    /// The import prefix/alias (e.g., "@components" or "mymodule").
    pub prefix: String,
    /// The filesystem path it maps to (relative to project root).
    pub target: PathBuf,
}

/// Source roots discovered from project configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRoot {
    /// Filesystem path (relative to project root).
    pub path: PathBuf,
    /// Whether this is a test source root.
    pub is_test: bool,
}

/// Project configuration resolved from language-specific config files.
#[derive(Debug, Clone, Default)]
pub struct ProjectConfig {
    /// Module/package name (e.g., go module path, npm package name).
    pub module_name: Option<String>,
    /// Import path aliases (e.g., tsconfig paths, PSR-4 namespaces).
    pub path_mappings: Vec<PathMapping>,
    /// Source roots (e.g., src/main/java, Sources/).
    pub source_roots: Vec<SourceRoot>,
    /// Additional metadata (language-specific).
    pub metadata: HashMap<String, String>,
}

/// Resolves project-level import paths for a specific language.
pub trait ProjectResolver: Send + Sync + std::fmt::Debug {
    /// Which language this resolver handles.
    fn language(&self) -> Language;

    /// Config filenames this resolver looks for (e.g., ["tsconfig.json"]).
    fn config_files(&self) -> &[&str];

    /// Try to resolve project configuration from a root directory.
    ///
    /// Searches for config files and parses them to extract path mappings,
    /// source roots, and module names.
    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig>;

    /// Resolve an import path to a filesystem path.
    ///
    /// Returns the resolved path relative to the project root, or None
    /// if the import is external (not resolvable to a local file).
    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf>;
}

/// Create a project resolver for the given language.
pub fn create_resolver(language: Language) -> Option<Box<dyn ProjectResolver>> {
    match language {
        Language::TypeScript => Some(Box::new(typescript::TypeScriptResolver)),
        Language::JavaScript => Some(Box::new(javascript::JavaScriptResolver)),
        Language::Go => Some(Box::new(go::GoResolver)),
        Language::Python => Some(Box::new(python::PythonResolver)),
        Language::Java => Some(Box::new(java::JavaResolver)),
        Language::Kotlin => Some(Box::new(kotlin::KotlinResolver)),
        Language::Php => Some(Box::new(php::PhpResolver)),
        Language::Swift => Some(Box::new(swift::SwiftResolver)),
        Language::CSharp => Some(Box::new(csharp::CSharpResolver)),
        // Rust, C, C++, GDScript, Lua don't have project-level config
        _ => None,
    }
}

/// Find a config file by searching from the given directory upward.
fn find_config_file(start: &Path, filename: &str) -> Option<PathBuf> {
    let candidate = start.join(filename);
    if candidate.is_file() {
        return Some(candidate);
    }
    // Don't search parent directories - project root is explicit
    None
}

/// Read a JSON config file and parse it.
fn read_json_file(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    // Strip comments (//...) and trailing commas for JSON5-like tolerance
    let cleaned = strip_json_comments(&content);
    serde_json::from_str(&cleaned).ok()
}

/// Strip single-line comments and trailing commas from JSON-like content.
///
/// This provides basic JSON5/JSONC tolerance for config files like tsconfig.json
/// that commonly use comments and trailing commas.
fn strip_json_comments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape_next = false;

    while let Some(ch) = chars.next() {
        if escape_next {
            result.push(ch);
            escape_next = false;
            continue;
        }

        if in_string {
            result.push(ch);
            if ch == '\\' {
                escape_next = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                result.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                // Single-line comment: skip to end of line
                chars.next();
                for c in chars.by_ref() {
                    if c == '\n' {
                        result.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                // Block comment: skip to */
                chars.next();
                let mut prev = ' ';
                for c in chars.by_ref() {
                    if prev == '*' && c == '/' {
                        break;
                    }
                    prev = c;
                }
            }
            _ => result.push(ch),
        }
    }

    // Strip trailing commas before } or ]
    let mut cleaned = String::with_capacity(result.len());
    let bytes = result.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b',' {
            // Look ahead for closing bracket/brace (skipping whitespace)
            let mut j = i + 1;
            while j < len && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < len && (bytes[j] == b'}' || bytes[j] == b']') {
                // Skip the trailing comma
                i += 1;
                continue;
            }
        }
        cleaned.push(bytes[i] as char);
        i += 1;
    }

    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_resolver_supported() {
        assert!(create_resolver(Language::TypeScript).is_some());
        assert!(create_resolver(Language::JavaScript).is_some());
        assert!(create_resolver(Language::Go).is_some());
        assert!(create_resolver(Language::Python).is_some());
        assert!(create_resolver(Language::Java).is_some());
        assert!(create_resolver(Language::Kotlin).is_some());
        assert!(create_resolver(Language::Php).is_some());
        assert!(create_resolver(Language::Swift).is_some());
        assert!(create_resolver(Language::CSharp).is_some());
    }

    #[test]
    fn test_create_resolver_unsupported() {
        assert!(create_resolver(Language::Rust).is_none());
        assert!(create_resolver(Language::C).is_none());
        assert!(create_resolver(Language::Cpp).is_none());
        assert!(create_resolver(Language::Lua).is_none());
        assert!(create_resolver(Language::Gdscript).is_none());
    }

    #[test]
    fn test_strip_json_comments() {
        let input = r#"{
  // This is a comment
  "key": "value", // trailing comment
  "arr": [1, 2, 3,],
  /* block comment */
  "nested": {
    "a": true,
  }
}"#;
        let result = strip_json_comments(input);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["arr"][0], 1);
        assert_eq!(parsed["nested"]["a"], true);
    }

    #[test]
    fn test_strip_json_comments_preserves_strings() {
        let input = r#"{"url": "https://example.com/path"}"#;
        let result = strip_json_comments(input);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["url"], "https://example.com/path");
    }
}
