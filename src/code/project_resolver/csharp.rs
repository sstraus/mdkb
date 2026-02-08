//! C# project resolver - parses .csproj files for namespace resolution.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    PathMapping, ProjectConfig, ProjectResolver, SourceRoot,
};

#[derive(Debug)]
pub struct CSharpResolver;

impl ProjectResolver for CSharpResolver {
    fn language(&self) -> Language {
        Language::CSharp
    }

    fn config_files(&self) -> &[&str] {
        &["*.csproj", "*.sln"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        // Find .csproj files
        if let Some(config) = self.resolve_csproj(project_root) {
            return Some(config);
        }
        self.detect_layout(project_root)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        // C# uses namespaces: "MyApp.Models.User" -> "Models/User.cs"
        let file_path = import_path.replace('.', "/");

        // Try with namespace prefix stripped
        for mapping in &config.path_mappings {
            if import_path == mapping.prefix
                || import_path.starts_with(&format!("{}.", mapping.prefix))
            {
                let remainder = import_path
                    .strip_prefix(&mapping.prefix)
                    .unwrap_or("")
                    .trim_start_matches('.');
                let remainder_path = remainder.replace('.', "/");
                let candidate = project_root.join(&mapping.target).join(remainder_path);
                let cs_file = candidate.with_extension("cs");
                if cs_file.is_file() {
                    return Some(cs_file);
                }
            }
        }

        // Try source roots
        for root in &config.source_roots {
            let candidate = project_root.join(&root.path).join(&file_path);
            let cs_file = candidate.with_extension("cs");
            if cs_file.is_file() {
                return Some(cs_file);
            }
        }

        None
    }
}

impl CSharpResolver {
    fn resolve_csproj(&self, project_root: &Path) -> Option<ProjectConfig> {
        // Find .csproj files in the root
        let csproj = std::fs::read_dir(project_root)
            .ok()?
            .flatten()
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .ends_with(".csproj")
            })?;

        let content = std::fs::read_to_string(csproj.path()).ok()?;
        let mut config = ProjectConfig::default();

        // Extract RootNamespace
        if let Some(start) = content.find("<RootNamespace>") {
            let start = start + "<RootNamespace>".len();
            if let Some(end) = content[start..].find("</RootNamespace>") {
                let namespace = content[start..start + end].trim().to_string();
                config.module_name = Some(namespace.clone());
                config.path_mappings.push(PathMapping {
                    prefix: namespace,
                    target: PathBuf::from("."),
                });
            }
        }

        // Extract AssemblyName as fallback module name
        if config.module_name.is_none() {
            if let Some(start) = content.find("<AssemblyName>") {
                let start = start + "<AssemblyName>".len();
                if let Some(end) = content[start..].find("</AssemblyName>") {
                    config.module_name = Some(content[start..start + end].trim().to_string());
                }
            }
        }

        // Use project name from filename as last resort
        if config.module_name.is_none() {
            config.module_name = csproj
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string());
        }

        // Standard .NET source roots
        config.source_roots.push(SourceRoot {
            path: PathBuf::from("."),
            is_test: false,
        });

        Some(config)
    }

    fn detect_layout(&self, project_root: &Path) -> Option<ProjectConfig> {
        let mut config = ProjectConfig::default();

        // Check for .sln (solution file) - indicates .NET project
        let has_sln = std::fs::read_dir(project_root)
            .ok()?
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".sln"));

        if has_sln {
            // Look for project directories that contain .csproj
            if let Ok(entries) = std::fs::read_dir(project_root) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                        let dir_path = entry.path();
                        let has_csproj = std::fs::read_dir(&dir_path)
                            .map(|entries| {
                                entries
                                    .flatten()
                                    .any(|e| e.file_name().to_string_lossy().ends_with(".csproj"))
                            })
                            .unwrap_or(false);

                        if has_csproj {
                            let is_test = entry
                                .file_name()
                                .to_string_lossy()
                                .to_lowercase()
                                .contains("test");
                            config.source_roots.push(SourceRoot {
                                path: PathBuf::from(entry.file_name()),
                                is_test,
                            });
                        }
                    }
                }
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
    fn test_resolve_csproj() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(
            root.join("MyApp.csproj"),
            r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <TargetFramework>net8.0</TargetFramework>
    <RootNamespace>MyApp</RootNamespace>
  </PropertyGroup>
</Project>"#,
        )
        .unwrap();

        let resolver = CSharpResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.module_name.as_deref(), Some("MyApp"));
        assert!(config.path_mappings.iter().any(|m| m.prefix == "MyApp"));
    }

    #[test]
    fn test_resolve_csharp_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("Models")).unwrap();
        fs::write(root.join("Models/User.cs"), "namespace MyApp.Models;").unwrap();

        let config = ProjectConfig {
            path_mappings: vec![PathMapping {
                prefix: "MyApp".to_string(),
                target: PathBuf::from("."),
            }],
            ..Default::default()
        };

        let resolver = CSharpResolver;
        let resolved = resolver.resolve_import("MyApp.Models.User", &config, root);
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("User.cs"));
    }

    #[test]
    fn test_detect_sln_layout() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::write(root.join("MySolution.sln"), "").unwrap();
        fs::create_dir_all(root.join("MyApp")).unwrap();
        fs::write(root.join("MyApp/MyApp.csproj"), "<Project/>").unwrap();
        fs::create_dir_all(root.join("MyApp.Tests")).unwrap();
        fs::write(root.join("MyApp.Tests/MyApp.Tests.csproj"), "<Project/>").unwrap();

        let resolver = CSharpResolver;
        let config = resolver.resolve(root).unwrap();

        assert!(config.source_roots.iter().any(|r| !r.is_test));
        assert!(config.source_roots.iter().any(|r| r.is_test));
    }

    #[test]
    fn test_no_csproj() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = CSharpResolver;
        assert!(resolver.resolve(dir.path()).is_none());
    }
}
