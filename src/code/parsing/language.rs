//! Language detection and enumeration.
//!
//! Identifies programming languages from file extensions and provides
//! metadata (human-readable names, config keys, extension lists).

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Supported programming languages for code intelligence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    C,
    Cpp,
    CSharp,
    Java,
    Kotlin,
    Php,
    Swift,
    Lua,
    Gdscript,
}

impl Language {
    /// Detect language from a file extension (without leading dot).
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "jsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "ts" | "tsx" | "mts" | "cts" => Some(Self::TypeScript),
            "go" => Some(Self::Go),
            "c" | "h" => Some(Self::C),
            "cpp" | "hpp" | "cc" | "cxx" | "hxx" => Some(Self::Cpp),
            "cs" | "csx" => Some(Self::CSharp),
            "java" => Some(Self::Java),
            "kt" | "kts" => Some(Self::Kotlin),
            "php" | "php3" | "php4" | "php5" | "php7" | "php8" | "phtml" => Some(Self::Php),
            "swift" => Some(Self::Swift),
            "lua" => Some(Self::Lua),
            "gd" => Some(Self::Gdscript),
            _ => None,
        }
    }

    /// Detect language from a file path.
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::from_extension)
    }

    /// File extensions associated with this language.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            Self::Rust => &["rs"],
            Self::Python => &["py", "pyi"],
            Self::JavaScript => &["js", "jsx", "mjs", "cjs"],
            Self::TypeScript => &["ts", "tsx", "mts", "cts"],
            Self::Go => &["go"],
            Self::C => &["c", "h"],
            Self::Cpp => &["cpp", "hpp", "cc", "cxx", "hxx"],
            Self::CSharp => &["cs", "csx"],
            Self::Java => &["java"],
            Self::Kotlin => &["kt", "kts"],
            Self::Php => &["php", "php3", "php4", "php5", "php7", "php8", "phtml"],
            Self::Swift => &["swift"],
            Self::Lua => &["lua"],
            Self::Gdscript => &["gd"],
        }
    }

    /// Configuration key (lowercase identifier).
    pub fn config_key(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Go => "go",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Php => "php",
            Self::Swift => "swift",
            Self::Lua => "lua",
            Self::Gdscript => "gdscript",
        }
    }

    /// Human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::JavaScript => "JavaScript",
            Self::TypeScript => "TypeScript",
            Self::Go => "Go",
            Self::C => "C",
            Self::Cpp => "C++",
            Self::CSharp => "C#",
            Self::Java => "Java",
            Self::Kotlin => "Kotlin",
            Self::Php => "PHP",
            Self::Swift => "Swift",
            Self::Lua => "Lua",
            Self::Gdscript => "GDScript",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_extension() {
        assert_eq!(Language::from_extension("rs"), Some(Language::Rust));
        assert_eq!(Language::from_extension("RS"), Some(Language::Rust));
        assert_eq!(Language::from_extension("py"), Some(Language::Python));
        assert_eq!(Language::from_extension("ts"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("tsx"), Some(Language::TypeScript));
        assert_eq!(Language::from_extension("go"), Some(Language::Go));
        assert_eq!(Language::from_extension("php"), Some(Language::Php));
        assert_eq!(Language::from_extension("txt"), None);
    }

    #[test]
    fn test_from_path() {
        assert_eq!(
            Language::from_path(Path::new("main.rs")),
            Some(Language::Rust)
        );
        assert_eq!(
            Language::from_path(Path::new("src/app.tsx")),
            Some(Language::TypeScript)
        );
        assert_eq!(Language::from_path(Path::new("README.md")), None);
    }

    #[test]
    fn test_extensions_contain_primary() {
        assert!(Language::Rust.extensions().contains(&"rs"));
        assert!(Language::Python.extensions().contains(&"py"));
        assert!(Language::Go.extensions().contains(&"go"));
    }

    #[test]
    fn test_display() {
        assert_eq!(Language::Rust.to_string(), "Rust");
        assert_eq!(Language::Cpp.to_string(), "C++");
        assert_eq!(Language::CSharp.to_string(), "C#");
    }
}
