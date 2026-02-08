//! JavaScript project resolver - parses jsconfig.json for path aliases.
//!
//! Very similar to TypeScript resolver but reads jsconfig.json.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    PathMapping, ProjectConfig, ProjectResolver, SourceRoot, find_config_file, read_json_file,
};

#[derive(Debug)]
pub struct JavaScriptResolver;

impl ProjectResolver for JavaScriptResolver {
    fn language(&self) -> Language {
        Language::JavaScript
    }

    fn config_files(&self) -> &[&str] {
        &["jsconfig.json"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        let config_path = find_config_file(project_root, "jsconfig.json")?;
        let json = read_json_file(&config_path)?;

        let mut config = ProjectConfig::default();
        let compiler_opts = &json["compilerOptions"];

        let base_url = compiler_opts["baseUrl"].as_str().unwrap_or(".");

        // Extract paths (same format as tsconfig)
        if let Some(paths) = compiler_opts["paths"].as_object() {
            for (alias, targets) in paths {
                if let Some(first_target) = targets.as_array().and_then(|a| a.first()) {
                    if let Some(target_str) = first_target.as_str() {
                        let prefix = alias.trim_end_matches("/*").to_string();
                        let target = target_str.trim_end_matches("/*");
                        let resolved = PathBuf::from(base_url).join(target);
                        config.path_mappings.push(PathMapping {
                            prefix,
                            target: resolved,
                        });
                    }
                }
            }
        }

        if let Some(root_dir) = compiler_opts["rootDir"].as_str() {
            config.source_roots.push(SourceRoot {
                path: PathBuf::from(root_dir),
                is_test: false,
            });
        }

        Some(config)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        for mapping in &config.path_mappings {
            if import_path == mapping.prefix
                || import_path.starts_with(&format!("{}/", mapping.prefix))
            {
                let remainder = import_path
                    .strip_prefix(&mapping.prefix)
                    .unwrap_or("")
                    .trim_start_matches('/');
                let resolved = project_root.join(&mapping.target).join(remainder);
                return try_resolve_js_file(&resolved);
            }
        }

        if import_path.starts_with('.') {
            return None;
        }

        for root in &config.source_roots {
            let candidate = project_root.join(&root.path).join(import_path);
            if let Some(resolved) = try_resolve_js_file(&candidate) {
                return Some(resolved);
            }
        }

        None
    }
}

fn try_resolve_js_file(base: &Path) -> Option<PathBuf> {
    let extensions = [".js", ".jsx", ".mjs", ".cjs"];

    if base.is_file() {
        return Some(base.to_path_buf());
    }

    for ext in &extensions {
        let with_ext = base.with_extension(ext.trim_start_matches('.'));
        if with_ext.is_file() {
            return Some(with_ext);
        }
    }

    if base.is_dir() {
        for ext in &extensions {
            let index = base.join(format!("index{ext}"));
            if index.is_file() {
                return Some(index);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_resolve_jsconfig_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("jsconfig.json"),
            r#"{
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@lib/*": ["lib/*"]
    }
  }
}"#,
        )
        .unwrap();

        let resolver = JavaScriptResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.path_mappings.len(), 1);
        assert_eq!(config.path_mappings[0].prefix, "@lib");
        assert_eq!(config.path_mappings[0].target, PathBuf::from("./lib"));
    }

    #[test]
    fn test_no_jsconfig() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = JavaScriptResolver;
        assert!(resolver.resolve(dir.path()).is_none());
    }
}
