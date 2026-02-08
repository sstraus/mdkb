//! TypeScript project resolver - parses tsconfig.json for path aliases.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    PathMapping, ProjectConfig, ProjectResolver, SourceRoot, find_config_file, read_json_file,
};

#[derive(Debug)]
pub struct TypeScriptResolver;

impl ProjectResolver for TypeScriptResolver {
    fn language(&self) -> Language {
        Language::TypeScript
    }

    fn config_files(&self) -> &[&str] {
        &["tsconfig.json"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        let config_path = find_config_file(project_root, "tsconfig.json")?;
        let json = read_json_file(&config_path)?;

        let mut config = ProjectConfig::default();
        let compiler_opts = &json["compilerOptions"];

        // Extract baseUrl
        let base_url = compiler_opts["baseUrl"]
            .as_str()
            .unwrap_or(".");

        // Extract paths
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

        // Extract rootDir/rootDirs as source roots
        if let Some(root_dir) = compiler_opts["rootDir"].as_str() {
            config.source_roots.push(SourceRoot {
                path: PathBuf::from(root_dir),
                is_test: false,
            });
        }
        if let Some(root_dirs) = compiler_opts["rootDirs"].as_array() {
            for dir in root_dirs {
                if let Some(dir_str) = dir.as_str() {
                    config.source_roots.push(SourceRoot {
                        path: PathBuf::from(dir_str),
                        is_test: false,
                    });
                }
            }
        }

        // Extract outDir for metadata
        if let Some(out_dir) = compiler_opts["outDir"].as_str() {
            config.metadata.insert("outDir".to_string(), out_dir.to_string());
        }

        Some(config)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        // Check path aliases
        for mapping in &config.path_mappings {
            if import_path == mapping.prefix
                || import_path.starts_with(&format!("{}/", mapping.prefix))
            {
                let remainder = import_path
                    .strip_prefix(&mapping.prefix)
                    .unwrap_or("")
                    .trim_start_matches('/');
                let resolved = project_root.join(&mapping.target).join(remainder);
                return try_resolve_ts_file(&resolved);
            }
        }

        // Relative imports are already resolved
        if import_path.starts_with('.') {
            return None;
        }

        // Try source roots
        for root in &config.source_roots {
            let candidate = project_root.join(&root.path).join(import_path);
            if let Some(resolved) = try_resolve_ts_file(&candidate) {
                return Some(resolved);
            }
        }

        None
    }
}

/// Try to resolve a TypeScript/JavaScript file path with extension probing.
fn try_resolve_ts_file(base: &Path) -> Option<PathBuf> {
    let extensions = [".ts", ".tsx", ".js", ".jsx", ".mts", ".mjs"];

    // Direct file exists
    if base.is_file() {
        return Some(base.to_path_buf());
    }

    // Try with extensions
    for ext in &extensions {
        let with_ext = base.with_extension(ext.trim_start_matches('.'));
        if with_ext.is_file() {
            return Some(with_ext);
        }
    }

    // Try index file in directory
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
    fn test_resolve_tsconfig_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("tsconfig.json"),
            r#"{
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@components/*": ["src/components/*"],
      "@utils/*": ["src/utils/*"]
    },
    "rootDir": "src",
    "outDir": "dist"
  }
}"#,
        )
        .unwrap();

        let resolver = TypeScriptResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.path_mappings.len(), 2);
        assert!(config.path_mappings.iter().any(|m| m.prefix == "@components"
            && m.target == PathBuf::from("./src/components")));
        assert!(config.path_mappings.iter().any(|m| m.prefix == "@utils"
            && m.target == PathBuf::from("./src/utils")));
        assert_eq!(config.source_roots.len(), 1);
        assert_eq!(config.source_roots[0].path, PathBuf::from("src"));
        assert_eq!(config.metadata.get("outDir").unwrap(), "dist");
    }

    #[test]
    fn test_resolve_tsconfig_with_comments() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("tsconfig.json"),
            r#"{
  // Compiler options
  "compilerOptions": {
    "baseUrl": ".",
    "paths": {
      "@lib/*": ["lib/*"], // Library path
    }
  }
}"#,
        )
        .unwrap();

        let resolver = TypeScriptResolver;
        let config = resolver.resolve(root).unwrap();
        assert_eq!(config.path_mappings.len(), 1);
        assert_eq!(config.path_mappings[0].prefix, "@lib");
    }

    #[test]
    fn test_resolve_import_with_alias() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("src/components")).unwrap();
        fs::write(root.join("src/components/Button.ts"), "export class Button {}").unwrap();

        let config = ProjectConfig {
            path_mappings: vec![PathMapping {
                prefix: "@components".to_string(),
                target: PathBuf::from("src/components"),
            }],
            ..Default::default()
        };

        let resolver = TypeScriptResolver;
        let resolved = resolver.resolve_import("@components/Button", &config, root);
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("Button.ts"));
    }

    #[test]
    fn test_no_tsconfig() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = TypeScriptResolver;
        assert!(resolver.resolve(dir.path()).is_none());
    }
}
