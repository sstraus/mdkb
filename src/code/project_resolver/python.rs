//! Python project resolver - parses pyproject.toml for module resolution.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    PathMapping, ProjectConfig, ProjectResolver, SourceRoot, find_config_file,
};

#[derive(Debug)]
pub struct PythonResolver;

impl ProjectResolver for PythonResolver {
    fn language(&self) -> Language {
        Language::Python
    }

    fn config_files(&self) -> &[&str] {
        &["pyproject.toml", "setup.cfg"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        // Try pyproject.toml first
        if let Some(config) = self.resolve_pyproject(project_root) {
            return Some(config);
        }

        // Fallback: detect common Python project layout
        self.detect_layout(project_root)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        // Convert dotted import to path (e.g., "mypackage.utils" -> "mypackage/utils")
        let path_part = import_path.replace('.', "/");

        // Check path mappings (package name -> source dir)
        for mapping in &config.path_mappings {
            if import_path == mapping.prefix
                || import_path.starts_with(&format!("{}.", mapping.prefix))
            {
                let remainder = import_path
                    .strip_prefix(&mapping.prefix)
                    .unwrap_or("")
                    .trim_start_matches('.');
                let remainder_path = remainder.replace('.', "/");
                let base = project_root.join(&mapping.target);
                return try_resolve_python_module(&base.join(remainder_path));
            }
        }

        // Try source roots
        for root in &config.source_roots {
            let candidate = project_root.join(&root.path).join(&path_part);
            if let Some(resolved) = try_resolve_python_module(&candidate) {
                return Some(resolved);
            }
        }

        // Try from project root
        let candidate = project_root.join(&path_part);
        try_resolve_python_module(&candidate)
    }
}

impl PythonResolver {
    fn resolve_pyproject(&self, project_root: &Path) -> Option<ProjectConfig> {
        let config_path = find_config_file(project_root, "pyproject.toml")?;
        let content = std::fs::read_to_string(&config_path).ok()?;
        let toml_value: toml::Value = content.parse().ok()?;

        let mut config = ProjectConfig::default();

        // Extract project name
        if let Some(name) = toml_value
            .get("project")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        {
            config.module_name = Some(name.to_string());
        }

        // Extract packages from [tool.setuptools.packages.find]
        if let Some(find) = toml_value
            .get("tool")
            .and_then(|t| t.get("setuptools"))
            .and_then(|s| s.get("packages"))
            .and_then(|p| p.get("find"))
        {
            if let Some(where_dirs) = find.get("where").and_then(|w| w.as_array()) {
                for dir in where_dirs {
                    if let Some(dir_str) = dir.as_str() {
                        config.source_roots.push(SourceRoot {
                            path: PathBuf::from(dir_str),
                            is_test: false,
                        });
                    }
                }
            }
        }

        // Extract package-dir from [tool.setuptools.package-dir]
        if let Some(package_dir) = toml_value
            .get("tool")
            .and_then(|t| t.get("setuptools"))
            .and_then(|s| s.get("package-dir"))
            .and_then(|p| p.as_table())
        {
            for (package_name, dir) in package_dir {
                if let Some(dir_str) = dir.as_str() {
                    if package_name.is_empty() || package_name == "\"\"" {
                        config.source_roots.push(SourceRoot {
                            path: PathBuf::from(dir_str),
                            is_test: false,
                        });
                    } else {
                        config.path_mappings.push(PathMapping {
                            prefix: package_name.clone(),
                            target: PathBuf::from(dir_str),
                        });
                    }
                }
            }
        }

        // If no source roots found, check for src/ layout
        if config.source_roots.is_empty() && project_root.join("src").is_dir() {
            config.source_roots.push(SourceRoot {
                path: PathBuf::from("src"),
                is_test: false,
            });
        }

        Some(config)
    }

    fn detect_layout(&self, project_root: &Path) -> Option<ProjectConfig> {
        let mut config = ProjectConfig::default();

        // Detect src layout
        if project_root.join("src").is_dir() {
            config.source_roots.push(SourceRoot {
                path: PathBuf::from("src"),
                is_test: false,
            });
        }

        // Detect flat layout (module in root)
        let has_setup_py = project_root.join("setup.py").exists();
        let has_init = project_root.join("__init__.py").exists();

        if has_setup_py || has_init {
            config.source_roots.push(SourceRoot {
                path: PathBuf::from("."),
                is_test: false,
            });
        }

        // Add tests dir
        if project_root.join("tests").is_dir() {
            config.source_roots.push(SourceRoot {
                path: PathBuf::from("tests"),
                is_test: true,
            });
        }

        if config.source_roots.is_empty() {
            return None;
        }

        Some(config)
    }
}

fn try_resolve_python_module(base: &Path) -> Option<PathBuf> {
    // Package (directory with __init__.py)
    let init = base.join("__init__.py");
    if init.is_file() {
        return Some(init);
    }

    // Module file
    let py_file = base.with_extension("py");
    if py_file.is_file() {
        return Some(py_file);
    }

    // Stub file
    let pyi_file = base.with_extension("pyi");
    if pyi_file.is_file() {
        return Some(pyi_file);
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_pyproject_toml() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("pyproject.toml"),
            r#"
[project]
name = "mypackage"

[tool.setuptools.packages.find]
where = ["src"]
"#,
        )
        .unwrap();

        let resolver = PythonResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.module_name.as_deref(), Some("mypackage"));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src")));
    }

    #[test]
    fn test_resolve_python_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/mypackage/utils")).unwrap();
        fs::write(root.join("src/mypackage/__init__.py"), "").unwrap();
        fs::write(root.join("src/mypackage/utils/__init__.py"), "").unwrap();
        fs::write(root.join("src/mypackage/utils/helpers.py"), "def helper(): pass").unwrap();

        let config = ProjectConfig {
            source_roots: vec![SourceRoot {
                path: PathBuf::from("src"),
                is_test: false,
            }],
            ..Default::default()
        };

        let resolver = PythonResolver;
        let resolved = resolver.resolve_import("mypackage.utils.helpers", &config, root);
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("helpers.py"));
    }

    #[test]
    fn test_detect_src_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src")).unwrap();

        let resolver = PythonResolver;
        let config = resolver.detect_layout(root).unwrap();
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src")));
    }

    #[test]
    fn test_no_python_project() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = PythonResolver;
        assert!(resolver.resolve(dir.path()).is_none());
    }
}
