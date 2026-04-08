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
    ///
    /// Tries extension first, then falls back to shebang detection for
    /// extensionless scripts (e.g., `#!/usr/bin/env node`).
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|ext| ext.to_str())
            .and_then(Self::from_extension)
            .or_else(|| Self::from_shebang(path))
    }

    /// Detect language from a shebang line (`#!`).
    ///
    /// Reads only the first 256 bytes to avoid loading large files.
    fn from_shebang(path: &Path) -> Option<Self> {
        use std::io::Read;
        let mut file = std::fs::File::open(path).ok()?;
        let mut buf = [0u8; 256];
        let n = file.read(&mut buf).ok()?;
        let line = std::str::from_utf8(&buf[..n])
            .ok()?
            .lines()
            .next()?;
        if !line.starts_with("#!") {
            return None;
        }
        let shebang = line.trim_start_matches("#!");
        // Match interpreter name from "#!/usr/bin/env X" or "#!/usr/bin/X"
        let interpreter = shebang
            .rsplit('/')
            .next()
            .unwrap_or(shebang)
            .split_whitespace()
            .last()?;
        match interpreter {
            "node" | "nodejs" | "bun" | "deno" => Some(Self::JavaScript),
            "ts-node" | "tsx" => Some(Self::TypeScript),
            "python" | "python3" | "python2" => Some(Self::Python),
            "ruby" => None, // not supported yet
            "bash" | "sh" | "zsh" | "fish" => None,
            _ => None,
        }
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

    #[test]
    fn test_shebang_node() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("my-tool");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();
        assert_eq!(Language::from_path(&script), Some(Language::JavaScript));
    }

    #[test]
    fn test_shebang_python() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run");
        std::fs::write(&script, "#!/usr/bin/python3\nimport sys\n").unwrap();
        assert_eq!(Language::from_path(&script), Some(Language::Python));
    }

    #[test]
    fn test_shebang_bash_not_supported() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("deploy");
        std::fs::write(&script, "#!/bin/bash\necho hello\n").unwrap();
        assert_eq!(Language::from_path(&script), None);
    }

    #[test]
    fn test_shebang_not_present() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("data");
        std::fs::write(&script, "just some text\n").unwrap();
        assert_eq!(Language::from_path(&script), None);
    }

    #[test]
    fn test_extension_takes_priority_over_shebang() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("tool.py");
        std::fs::write(&script, "#!/usr/bin/env node\nconsole.log('hi');\n").unwrap();
        // Extension says Python, shebang says Node — extension wins
        assert_eq!(Language::from_path(&script), Some(Language::Python));
    }
}
