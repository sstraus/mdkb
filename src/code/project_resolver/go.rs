//! Go project resolver - parses go.mod for module paths.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    ProjectConfig, ProjectResolver, SourceRoot, find_config_file,
};

#[derive(Debug)]
pub struct GoResolver;

impl ProjectResolver for GoResolver {
    fn language(&self) -> Language {
        Language::Go
    }

    fn config_files(&self) -> &[&str] {
        &["go.mod"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        let config_path = find_config_file(project_root, "go.mod")?;
        let content = std::fs::read_to_string(&config_path).ok()?;

        let mut config = ProjectConfig::default();

        // Extract module path from first "module" line
        for line in content.lines() {
            let line = line.trim();
            if let Some(module_path) = line.strip_prefix("module ") {
                config.module_name = Some(module_path.trim().to_string());
                break;
            }
        }

        // Go projects have the root as the source root
        config.source_roots.push(SourceRoot {
            path: PathBuf::from("."),
            is_test: false,
        });

        // Extract Go version for metadata
        for line in content.lines() {
            let line = line.trim();
            if let Some(version) = line.strip_prefix("go ") {
                config.metadata.insert("go_version".to_string(), version.trim().to_string());
                break;
            }
        }

        Some(config)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        let module_name = config.module_name.as_deref()?;

        // Check if the import belongs to this module
        if import_path == module_name {
            return Some(project_root.to_path_buf());
        }

        if let Some(sub_path) = import_path.strip_prefix(module_name) {
            let sub_path = sub_path.trim_start_matches('/');
            let resolved = project_root.join(sub_path);
            if resolved.is_dir() {
                return Some(resolved);
            }
        }

        // External package - not resolvable locally
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_go_mod() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("go.mod"),
            "module github.com/user/myproject\n\ngo 1.22\n\nrequire (\n\tgolang.org/x/sync v0.6.0\n)\n",
        )
        .unwrap();

        let resolver = GoResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(
            config.module_name.as_deref(),
            Some("github.com/user/myproject")
        );
        assert_eq!(config.metadata.get("go_version").unwrap(), "1.22");
    }

    #[test]
    fn test_resolve_local_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("internal/handler")).unwrap();
        fs::write(root.join("internal/handler/main.go"), "package handler").unwrap();

        let config = ProjectConfig {
            module_name: Some("github.com/user/myproject".to_string()),
            ..Default::default()
        };

        let resolver = GoResolver;
        let resolved =
            resolver.resolve_import("github.com/user/myproject/internal/handler", &config, root);
        assert!(resolved.is_some());
    }

    #[test]
    fn test_external_import_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let config = ProjectConfig {
            module_name: Some("github.com/user/myproject".to_string()),
            ..Default::default()
        };

        let resolver = GoResolver;
        assert!(resolver
            .resolve_import("golang.org/x/sync", &config, dir.path())
            .is_none());
    }

    #[test]
    fn test_no_go_mod() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = GoResolver;
        assert!(resolver.resolve(dir.path()).is_none());
    }
}
