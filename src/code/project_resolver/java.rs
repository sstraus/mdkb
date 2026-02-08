//! Java project resolver - parses build.gradle and pom.xml for source roots.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    ProjectConfig, ProjectResolver, SourceRoot, find_config_file,
};

#[derive(Debug)]
pub struct JavaResolver;

impl ProjectResolver for JavaResolver {
    fn language(&self) -> Language {
        Language::Java
    }

    fn config_files(&self) -> &[&str] {
        &["build.gradle", "pom.xml"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        // Try Gradle first
        if let Some(config) = self.resolve_gradle(project_root) {
            return Some(config);
        }
        // Then Maven
        if let Some(config) = self.resolve_maven(project_root) {
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
        // Convert Java package path to filesystem path
        // "com.example.utils.Helper" -> "com/example/utils/Helper.java"
        let file_path = import_path.replace('.', "/");

        for root in &config.source_roots {
            let candidate = project_root.join(&root.path).join(&file_path);
            let java_file = candidate.with_extension("java");
            if java_file.is_file() {
                return Some(java_file);
            }
        }

        None
    }
}

impl JavaResolver {
    fn resolve_gradle(&self, project_root: &Path) -> Option<ProjectConfig> {
        let _config_path = find_config_file(project_root, "build.gradle")?;

        let mut config = ProjectConfig::default();

        // Standard Gradle Java source sets
        let standard_roots = [
            ("src/main/java", false),
            ("src/main/groovy", false),
            ("src/test/java", true),
            ("src/test/groovy", true),
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

        // Extract group/artifact from build.gradle (simple regex)
        if let Ok(content) = std::fs::read_to_string(project_root.join("build.gradle")) {
            for line in content.lines() {
                let line = line.trim();
                if let Some(group) = line.strip_prefix("group") {
                    let group = group
                        .trim_start_matches(|c: char| c == ' ' || c == '=' || c == '\'' || c == '"')
                        .trim_end_matches(|c: char| c == '\'' || c == '"')
                        .trim();
                    if !group.is_empty() {
                        config.module_name = Some(group.to_string());
                    }
                }
            }
        }

        if config.source_roots.is_empty() {
            return None;
        }

        Some(config)
    }

    fn resolve_maven(&self, project_root: &Path) -> Option<ProjectConfig> {
        let _config_path = find_config_file(project_root, "pom.xml")?;

        let mut config = ProjectConfig::default();

        // Standard Maven source directories
        let standard_roots = [
            ("src/main/java", false),
            ("src/test/java", true),
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

        // Extract groupId from pom.xml (basic parsing)
        if let Ok(content) = std::fs::read_to_string(project_root.join("pom.xml")) {
            if let Some(start) = content.find("<groupId>") {
                let start = start + "<groupId>".len();
                if let Some(end) = content[start..].find("</groupId>") {
                    config.module_name = Some(content[start..start + end].trim().to_string());
                }
            }
        }

        if config.source_roots.is_empty() {
            return None;
        }

        Some(config)
    }

    fn detect_layout(&self, project_root: &Path) -> Option<ProjectConfig> {
        let mut config = ProjectConfig::default();

        // Check for Maven/Gradle standard layout without build files
        let candidates = [
            ("src/main/java", false),
            ("src/test/java", true),
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
    fn test_resolve_gradle_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/main/java")).unwrap();
        fs::create_dir_all(root.join("src/test/java")).unwrap();
        fs::write(root.join("build.gradle"), "group = 'com.example'\nversion = '1.0'").unwrap();

        let resolver = JavaResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.module_name.as_deref(), Some("com.example"));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src/main/java") && !r.is_test));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("src/test/java") && r.is_test));
    }

    #[test]
    fn test_resolve_maven_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/main/java")).unwrap();
        fs::write(
            root.join("pom.xml"),
            "<project><groupId>com.example</groupId></project>",
        )
        .unwrap();

        let resolver = JavaResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.module_name.as_deref(), Some("com.example"));
    }

    #[test]
    fn test_resolve_java_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/main/java/com/example/utils")).unwrap();
        fs::write(
            root.join("src/main/java/com/example/utils/Helper.java"),
            "package com.example.utils;\npublic class Helper {}",
        )
        .unwrap();

        let config = ProjectConfig {
            source_roots: vec![SourceRoot {
                path: PathBuf::from("src/main/java"),
                is_test: false,
            }],
            ..Default::default()
        };

        let resolver = JavaResolver;
        let resolved =
            resolver.resolve_import("com.example.utils.Helper", &config, root);
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("Helper.java"));
    }
}
