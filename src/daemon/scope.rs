//! Which scope (`home`, `work`, ...) a path belongs to.
//!
//! Two sources declare it, both hand-owned: `[[scopes]]` prefix rules in
//! `daemon.toml`, and a `scope` on a `repos.json` entry. The nearest
//! declaration above a path wins, and at one path the entry beats the rule, so
//! an entry overrides the prefix that would otherwise decide.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::config::DaemonConfig;
use super::repo_map::{canonical_key, clean_scope, read_scope_overrides};

#[derive(Debug, Clone, Default)]
pub struct ScopePolicy {
    rules: BTreeMap<PathBuf, String>,
    overrides: BTreeMap<PathBuf, String>,
}

impl ScopePolicy {
    pub fn new(rules: Vec<(PathBuf, String)>, overrides: BTreeMap<PathBuf, String>) -> Self {
        Self {
            rules: rules.into_iter().collect(),
            overrides,
        }
    }

    /// The policy `config` and the repo map beside it declare. A `scope` on a
    /// `[[repos]]` seed counts as an entry; the map file wins over a seed.
    pub fn load(config: &DaemonConfig) -> Self {
        let mut overrides: BTreeMap<PathBuf, String> = config
            .repos
            .iter()
            .filter_map(|r| {
                Some((
                    canonical_key(Path::new(&r.root)),
                    clean_scope(r.scope.as_deref()?)?,
                ))
            })
            .collect();
        if let Some(map) = config.repo_map_path() {
            overrides.extend(read_scope_overrides(&map));
        }
        Self::new(config.scope_rules(), overrides)
    }

    /// The scope `path` belongs to, or `None` when nothing declares one.
    pub fn scope_of(&self, path: &Path) -> Option<&str> {
        path.ancestors()
            .find_map(|a| self.overrides.get(a).or_else(|| self.rules.get(a)))
            .map(String::as_str)
    }

    /// The scope the caller works in: the first of its `paths` (MCP roots, or
    /// the working directory) that has one.
    pub fn caller_scope(&self, paths: &[PathBuf]) -> Option<&str> {
        paths.iter().find_map(|p| self.scope_of(p))
    }
}
