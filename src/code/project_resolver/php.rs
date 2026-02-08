//! PHP project resolver - parses composer.json for PSR-4 namespace mappings.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    PathMapping, ProjectConfig, ProjectResolver, SourceRoot, find_config_file, read_json_file,
};

#[derive(Debug)]
pub struct PhpResolver;

impl ProjectResolver for PhpResolver {
    fn language(&self) -> Language {
        Language::Php
    }

    fn config_files(&self) -> &[&str] {
        &["composer.json"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        let config_path = find_config_file(project_root, "composer.json")?;
        let json = read_json_file(&config_path)?;

        let mut config = ProjectConfig::default();

        // Extract package name
        if let Some(name) = json["name"].as_str() {
            config.module_name = Some(name.to_string());
        }

        // Extract PSR-4 autoload mappings
        if let Some(psr4) = json["autoload"]["psr-4"].as_object() {
            for (namespace, path) in psr4 {
                let prefix = namespace.trim_end_matches('\\').to_string();
                let target = match path {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(arr) => {
                        arr.first()
                            .and_then(|v| v.as_str())
                            .unwrap_or("src")
                            .to_string()
                    }
                    _ => continue,
                };

                config.path_mappings.push(PathMapping {
                    prefix: prefix.clone(),
                    target: PathBuf::from(&target),
                });
                config.source_roots.push(SourceRoot {
                    path: PathBuf::from(&target),
                    is_test: false,
                });
            }
        }

        // Extract PSR-4 autoload-dev mappings (tests)
        if let Some(psr4_dev) = json["autoload-dev"]["psr-4"].as_object() {
            for (namespace, path) in psr4_dev {
                let prefix = namespace.trim_end_matches('\\').to_string();
                let target = match path {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(arr) => {
                        arr.first()
                            .and_then(|v| v.as_str())
                            .unwrap_or("tests")
                            .to_string()
                    }
                    _ => continue,
                };

                config.path_mappings.push(PathMapping {
                    prefix,
                    target: PathBuf::from(&target),
                });
                config.source_roots.push(SourceRoot {
                    path: PathBuf::from(&target),
                    is_test: true,
                });
            }
        }

        if config.source_roots.is_empty() && config.path_mappings.is_empty() {
            return None;
        }

        Some(config)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        // PHP namespaces use backslash: "App\Models\User"
        let normalized = import_path.replace('\\', "/");

        for mapping in &config.path_mappings {
            let prefix_path = mapping.prefix.replace('\\', "/");
            if normalized == prefix_path || normalized.starts_with(&format!("{prefix_path}/")) {
                let remainder = normalized
                    .strip_prefix(&prefix_path)
                    .unwrap_or("")
                    .trim_start_matches('/');
                let candidate = project_root.join(&mapping.target).join(remainder);
                let php_file = candidate.with_extension("php");
                if php_file.is_file() {
                    return Some(php_file);
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_composer_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("composer.json"),
            r#"{
  "name": "vendor/mypackage",
  "autoload": {
    "psr-4": {
      "App\\": "src/",
      "App\\Models\\": "src/Models/"
    }
  },
  "autoload-dev": {
    "psr-4": {
      "Tests\\": "tests/"
    }
  }
}"#,
        )
        .unwrap();

        let resolver = PhpResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.module_name.as_deref(), Some("vendor/mypackage"));
        assert!(config.path_mappings.iter().any(|m| m.prefix == "App"
            && m.target == PathBuf::from("src/")));
        assert!(config.path_mappings.iter().any(|m| m.prefix == "Tests"
            && m.target == PathBuf::from("tests/")));
        assert!(config.source_roots.iter().any(|r| r.is_test && r.path == PathBuf::from("tests/")));
    }

    #[test]
    fn test_resolve_php_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/Models")).unwrap();
        fs::write(root.join("src/Models/User.php"), "<?php class User {}").unwrap();

        let config = ProjectConfig {
            path_mappings: vec![PathMapping {
                prefix: "App".to_string(),
                target: PathBuf::from("src"),
            }],
            ..Default::default()
        };

        let resolver = PhpResolver;
        let resolved = resolver.resolve_import(r"App\Models\User", &config, root);
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("User.php"));
    }

    #[test]
    fn test_no_composer_json() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = PhpResolver;
        assert!(resolver.resolve(dir.path()).is_none());
    }
}
