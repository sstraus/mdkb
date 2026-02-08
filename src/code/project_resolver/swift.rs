//! Swift project resolver - parses Package.swift for module resolution.

use std::path::{Path, PathBuf};

use crate::code::parsing::language::Language;
use crate::code::project_resolver::{
    ProjectConfig, ProjectResolver, SourceRoot, find_config_file,
};

#[derive(Debug)]
pub struct SwiftResolver;

impl ProjectResolver for SwiftResolver {
    fn language(&self) -> Language {
        Language::Swift
    }

    fn config_files(&self) -> &[&str] {
        &["Package.swift"]
    }

    fn resolve(&self, project_root: &Path) -> Option<ProjectConfig> {
        // Try Package.swift (SPM)
        if let Some(config) = self.resolve_spm(project_root) {
            return Some(config);
        }
        // Detect Xcode project layout
        self.detect_layout(project_root)
    }

    fn resolve_import(
        &self,
        import_path: &str,
        config: &ProjectConfig,
        project_root: &Path,
    ) -> Option<PathBuf> {
        // Swift imports are module-level, not file-level
        // Check if the module corresponds to a local source root
        for root in &config.source_roots {
            // Module name typically matches the directory name
            let dir_name = root.path.file_name()?.to_str()?;
            if dir_name == import_path {
                let full_path = project_root.join(&root.path);
                if full_path.is_dir() {
                    return Some(full_path);
                }
            }
        }

        // Check module_name match
        if config.module_name.as_deref() == Some(import_path) {
            for root in &config.source_roots {
                if !root.is_test {
                    return Some(project_root.join(&root.path));
                }
            }
        }

        None
    }
}

impl SwiftResolver {
    fn resolve_spm(&self, project_root: &Path) -> Option<ProjectConfig> {
        let _config_path = find_config_file(project_root, "Package.swift")?;

        let mut config = ProjectConfig::default();

        // Read Package.swift to extract package name
        if let Ok(content) = std::fs::read_to_string(project_root.join("Package.swift")) {
            for line in content.lines() {
                let line = line.trim();
                // Match: name: "PackageName"
                if let Some(after_name) = line.strip_prefix("name:") {
                    let name = after_name
                        .trim()
                        .trim_start_matches('"')
                        .split('"')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    if !name.is_empty() {
                        config.module_name = Some(name);
                    }
                    break;
                }
            }
        }

        // SPM standard layout: Sources/<Target>/ and Tests/<Target>Tests/
        let sources_dir = project_root.join("Sources");
        if sources_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&sources_dir) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                        config.source_roots.push(SourceRoot {
                            path: PathBuf::from("Sources").join(entry.file_name()),
                            is_test: false,
                        });
                    }
                }
            }

            // If no subdirectories, Sources itself is the source root
            if config.source_roots.is_empty() {
                config.source_roots.push(SourceRoot {
                    path: PathBuf::from("Sources"),
                    is_test: false,
                });
            }
        }

        let tests_dir = project_root.join("Tests");
        if tests_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&tests_dir) {
                for entry in entries.flatten() {
                    if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false) {
                        config.source_roots.push(SourceRoot {
                            path: PathBuf::from("Tests").join(entry.file_name()),
                            is_test: true,
                        });
                    }
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

        // Check for common Xcode project layouts
        if project_root.join("Sources").is_dir() {
            config.source_roots.push(SourceRoot {
                path: PathBuf::from("Sources"),
                is_test: false,
            });
        }

        // Check for .xcodeproj
        if let Ok(entries) = std::fs::read_dir(project_root) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.ends_with(".xcodeproj") || name_str.ends_with(".xcworkspace") {
                    // Xcode project found - project root is likely the source root
                    config.source_roots.push(SourceRoot {
                        path: PathBuf::from("."),
                        is_test: false,
                    });
                    break;
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
    fn test_resolve_spm_package() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("Sources/MyLib")).unwrap();
        fs::create_dir_all(root.join("Tests/MyLibTests")).unwrap();
        fs::write(
            root.join("Package.swift"),
            r#"
// swift-tools-version: 5.9
import PackageDescription
let package = Package(
    name: "MyLib",
    targets: [
        .target(name: "MyLib"),
        .testTarget(name: "MyLibTests", dependencies: ["MyLib"]),
    ]
)
"#,
        )
        .unwrap();

        let resolver = SwiftResolver;
        let config = resolver.resolve(root).unwrap();

        assert_eq!(config.module_name.as_deref(), Some("MyLib"));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("Sources/MyLib") && !r.is_test));
        assert!(config.source_roots.iter().any(|r| r.path == PathBuf::from("Tests/MyLibTests") && r.is_test));
    }

    #[test]
    fn test_resolve_swift_import() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        fs::create_dir_all(root.join("Sources/MyLib")).unwrap();

        let config = ProjectConfig {
            source_roots: vec![SourceRoot {
                path: PathBuf::from("Sources/MyLib"),
                is_test: false,
            }],
            ..Default::default()
        };

        let resolver = SwiftResolver;
        let resolved = resolver.resolve_import("MyLib", &config, root);
        assert!(resolved.is_some());
    }

    #[test]
    fn test_no_package_swift() {
        let dir = tempfile::tempdir().unwrap();
        let resolver = SwiftResolver;
        assert!(resolver.resolve(dir.path()).is_none());
    }
}
