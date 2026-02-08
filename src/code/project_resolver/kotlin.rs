//! Kotlin project resolver - parses build.gradle.kts for source roots and KMP config.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    ProjectConfig, ProjectResolver, SourceRoot, find_config_file,
};

#[derive(Debug)]
pub struct KotlinResolver;

impl ProjectResolver for KotlinResolver {
    fn language(&self) -> Language {
        Language::Kotlin
    }

    fn config_files(&self) -> &[&str] {
        &["build.gradle.kts", "build.gradle"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        // Try Kotlin DSL first
        if let Some(config) = self.resolve_gradle_kts(project_root) {
            return Some(config);
        }
        // Fallback to Groovy Gradle
        if let Some(config) = self.resolve_gradle(project_root) {
            return Some(config);
        }
        // Detect standard layout
        self.detect_layout(project_root)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        let file_path = import_path.replace('.', "/");

        for root in &config.source_roots {
            let candidate = project_root.join(&root.path).join(&file_path);
            let kt_file = candidate.with_extension("kt");
            if kt_file.is_file() {
                return Some(kt_file);
            }
            let kts_file = candidate.with_extension("kts");
            if kts_file.is_file() {
                return Some(kts_file);
            }
        }

        None
    }
}

impl KotlinResolver {
    fn resolve_gradle_kts(&self, project_root: &Path) -> Option<ProjectConfig> {
        let _config_path = find_config_file(project_root, "build.gradle.kts")?;

        let mut config = ProjectConfig::default();

        // Standard Kotlin source sets
        let standard_roots = [
            ("src/main/kotlin", false),
            ("src/main/java", false),
            ("src/test/kotlin", true),
            ("src/test/java", true),
            // KMP common source sets
            ("src/commonMain/kotlin", false),
            ("src/commonTest/kotlin", true),
            ("src/jvmMain/kotlin", false),
            ("src/jvmTest/kotlin", true),
        ];

        for (path, is_test) in &standard_roots {
            let full_path = project_root.join(path);
            if full_path.is_dir() {
                config.source_roots.push(SourceRoot {
                    path: PathBuf::from(path),
                    is_test: *is_test,
                });
            }
        }

        // Extract group from build.gradle.kts
        if let Ok(content) = std::fs::read_to_string(project_root.join("build.gradle.kts")) {
            for line in content.lines() {
                let line = line.trim();
                if let Some(group) = line.strip_prefix("group") {
                    let group = group
                        .trim_start_matches(|c: char| c == ' ' || c == '=')
                        .trim()
                        .trim_matches('"');
                    if !group.is_empty() {
                        config.module_name = Some(group.to_string());
                    }
                    break;
                }
            }
        }

        if config.source_roots.is_empty() {
            return None;
        }

        Some(config)
    }

    fn resolve_gradle(&self, project_root: &Path) -> Option<ProjectConfig> {
        let _config_path = find_config_file(project_root, "build.gradle")?;

        let mut config = ProjectConfig::default();

        let standard_roots = [
            ("src/main/kotlin", false),
            ("src/main/java", false),
            ("src/test/kotlin", true),
        ];

        for (path, is_test) in &standard_roots {
            if project_root.join(path).is_dir() {
                config.source_roots.push(SourceRoot {
                    path: PathBuf::from(path),
                    is_test: *is_test,
                });
            }
        }

        if config.source_roots.is_empty() {
            return None;
        }

        Some(config)
    }

    fn detect_layout(&self, project_root: &Path) -> Option<ProjectConfig> {
        let mut config = ProjectConfig::default();

        let candidates = [
            ("src/main/kotlin", false),
            ("src/test/kotlin", true),
            ("src", false),
        ];

        for (path, is_test) in &candidates {
            if project_root.join(path).is_dir() {
                config.source_roots.push(SourceRoot {
                    path: PathBuf::from(path),
                    is_test: *is_test,
                });
            }
        }

        if config.source_roots.is_empty() {
            return None;
        }

        Some(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_kotlin_gradle_kts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/main/kotlin")).unwrap();
        fs::create_dir_all(root.join("src/test/kotlin")).unwrap();
        fs::write(
            root.join("build.gradle.kts"),
            "group = \"com.example\"\nversion = \"1.0\"",
        )
        .unwrap();

        let resolver = KotlinResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.module_name.as_deref(), Some("com.example"));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src/main/kotlin")));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src/test/kotlin") && r.is_test));
    }

    #[test]
    fn test_resolve_kmp_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/commonMain/kotlin")).unwrap();
        fs::create_dir_all(root.join("src/jvmMain/kotlin")).unwrap();
        fs::write(root.join("build.gradle.kts"), "").unwrap();

        let resolver = KotlinResolver;
        let config = resolver.resolve(root).unwrap();

        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src/commonMain/kotlin")));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src/jvmMain/kotlin")));
    }

    #[test]
    fn test_resolve_kotlin_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/main/kotlin/com/example")).unwrap();
        fs::write(
            root.join("src/main/kotlin/com/example/App.kt"),
            "package com.example\nclass App",
        )
        .unwrap();

        let config = ProjectConfig {
            source_roots: vec![SourceRoot {
                path: PathBuf::from("src/main/kotlin"),
                is_test: false,
            }],
            ..Default::default()
        };

        let resolver = KotlinResolver;
        let resolved = resolver.resolve_import("com.example.App", &config, root);
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("App.kt"));
    }
}
