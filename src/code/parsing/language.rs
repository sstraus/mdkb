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
    Objc,
    Shell,
    Ruby,
    Sql,
    Json,
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
            "m" => Some(Self::Objc),
            "sh" | "bash" => Some(Self::Shell),
            "rb" => Some(Self::Ruby),
            "sql" => Some(Self::Sql),
            "json" => Some(Self::Json),
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

    /// Detect language from a path and, for ambiguous C headers, their source.
    pub fn from_path_and_content(path: &Path, content: &str) -> Option<Self> {
        let language = Self::from_path(path);
        if language == Some(Self::C)
            && path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("h"))
        {
            if is_objc_header(content) {
                return Some(Self::Objc);
            }
            if is_cpp_header(content) {
                return Some(Self::Cpp);
            }
        }
        language
    }

    /// Detect language from a shebang line (`#!`).
    ///
    /// Reads only the first 256 bytes to avoid loading large files.
    fn from_shebang(path: &Path) -> Option<Self> {
        use std::io::Read;
        let mut file = std::fs::File::open(path).ok()?;
        let mut buf = [0u8; 256];
        let n = file.read(&mut buf).ok()?;
        let line = std::str::from_utf8(&buf[..n]).ok()?.lines().next()?;
        if !line.starts_with("#!") {
            return None;
        }
        let shebang = line.trim_start_matches("#!");
        // Kernel shebangs name the interpreter first; later words are its
        // arguments. env adds its own options before that interpreter.
        let mut words = shebang.split_whitespace();
        let program = words.next()?.rsplit('/').next()?;
        let interpreter = if program == "env" {
            loop {
                let word = words.next()?;
                match word {
                    "-u" | "--unset" | "-C" | "--chdir" => {
                        words.next()?;
                    }
                    "--" => break words.next()?.rsplit('/').next()?,
                    _ if word.starts_with('-') || word.contains('=') => {}
                    _ => break word.rsplit('/').next()?,
                }
            }
        } else {
            program
        };
        match interpreter {
            "node" | "nodejs" | "bun" | "deno" => Some(Self::JavaScript),
            "ts-node" | "tsx" => Some(Self::TypeScript),
            "python" | "python3" | "python2" => Some(Self::Python),
            "sh" | "bash" => Some(Self::Shell),
            "ruby" => Some(Self::Ruby),
            // not supported yet
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
            Self::Objc => &["m", "h"],
            Self::Shell => &["sh", "bash"],
            Self::Ruby => &["rb"],
            Self::Sql => &["sql"],
            Self::Json => &["json"],
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
            Self::Objc => "objc",
            Self::Shell => "shell",
            Self::Ruby => "ruby",
            Self::Sql => "sql",
            Self::Json => "json",
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
            Self::Objc => "Objective-C",
            Self::Shell => "Shell",
            Self::Ruby => "Ruby",
            Self::Sql => "SQL",
            Self::Json => "JSON",
        }
    }
}

/// Objective-C declarations, not occurrences of `@interface` in comments/strings.
fn is_objc_header(content: &str) -> bool {
    // Every Objective-C declaration starts with @; plain C headers need no
    // extra parse. The AST still decides when @ appears in a comment/string.
    if !content.contains('@') {
        return false;
    }
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_objc::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(content, None) else {
        return false;
    };
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "class_interface"
                | "class_implementation"
                | "protocol_declaration"
                | "class_declaration"
                | "protocol_forward_declaration"
        ) {
            return true;
        }
        stack.extend(node.named_children(&mut node.walk()));
    }
    false
}

/// Whether a `.h` file contains a valid C++ declaration that C cannot express.
///
/// Inspecting the C++ AST rather than text avoids matching comments or strings.
fn is_cpp_header(content: &str) -> bool {
    let mut parser = tree_sitter::Parser::new();
    if parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .is_err()
    {
        return false;
    }
    let Some(tree) = parser.parse(content, None) else {
        return false;
    };
    if tree.root_node().has_error() {
        return false;
    }

    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if matches!(
            node.kind(),
            "class_specifier" | "namespace_definition" | "template_declaration"
        ) {
            return true;
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    false
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
    fn cpp_headers_select_cpp_grammar_from_declarations() {
        for content in [
            "class Widget { public: void draw(); };\n",
            "namespace engine { void draw(); }\n",
            "template <typename T> T identity(T value) { return value; }\n",
        ] {
            assert_eq!(
                Language::from_path_and_content(Path::new("widget.h"), content),
                Some(Language::Cpp),
                "C++ header syntax must select the C++ grammar: {content}"
            );
        }
    }

    #[test]
    fn ordinary_c_header_keeps_c_grammar() {
        assert_eq!(
            Language::from_path_and_content(
                Path::new("widget.h"),
                "typedef struct Widget { int size; } Widget;\n"
            ),
            Some(Language::C)
        );
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
    fn shell_shebang_selects_shell_grammar() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("deploy");
        std::fs::write(&script, "#!/bin/bash\necho hello\n").unwrap();
        assert_eq!(Language::from_path(&script), Some(Language::Shell));
    }

    /// `#!/bin/sh -e` and `#!/bin/bash -eu` are the usual way to write a shell
    /// script; the interpreter is the first word, not the last.
    #[test]
    fn shell_shebang_with_flags_selects_shell_grammar() {
        let dir = tempfile::tempdir().unwrap();
        for (name, line) in [
            ("a", "#!/bin/sh -e"),
            ("b", "#!/bin/bash -eu"),
            ("c", "#!/usr/bin/env bash"),
            ("d", "#!/usr/bin/env -S bash -eu"),
            ("e", "#!/usr/bin/env -u IGNORED /bin/sh -e"),
            ("f", "#!/usr/bin/env MODE=test bash -eu"),
        ] {
            let script = dir.path().join(name);
            std::fs::write(&script, format!("{line}\necho hello\n")).unwrap();
            assert_eq!(
                Language::from_path(&script),
                Some(Language::Shell),
                "shebang {line:?}"
            );
        }
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
