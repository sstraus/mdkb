//! Which scope (`home`, `work`, ...) a path belongs to.
//!
//! Two sources declare it, both hand-owned: `[[scopes]]` prefix rules in
//! `daemon.toml`, and a `scope` on a `repos.json` entry. The nearest
//! declaration above a path wins, and at one path the entry beats the rule, so
//! an entry overrides the prefix that would otherwise decide.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::config::DaemonConfig;
use super::repo_map::{canonical_key, clean_scope, try_read_scope_overrides};

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

    /// The policy `config` and the repo map beside it declare, read together
    /// and read fresh: `daemon.toml` again when `config` knows where it lives,
    /// so a rule edited while a daemon runs applies like a `repos.json` edit.
    /// A `scope` on a `[[repos]]` seed counts as an entry; the map file wins
    /// over a seed.
    ///
    /// `Err` carries why `repos.json` could not be read and the policy without
    /// its overrides, for a caller with nothing better to fall back to.
    pub fn try_load(config: &DaemonConfig) -> Result<Self, (Self, String)> {
        let fresh = config
            .state_dir
            .as_ref()
            .map(|d| d.join("daemon.toml"))
            .filter(|p| p.is_file())
            .and_then(|p| match DaemonConfig::load_or_default(&p) {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!(path = %p.display(), "daemon.toml unreadable, scopes from the loaded config: {e}");
                    None
                }
            });
        let config = fresh.as_ref().unwrap_or(config);
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
        let rules = config.scope_rules();
        let Some(map) = config.repo_map_path() else {
            return Ok(Self::new(rules, overrides));
        };
        match try_read_scope_overrides(&map) {
            Ok(file) => {
                overrides.extend(file);
                Ok(Self::new(rules, overrides))
            }
            Err(why) => Err((Self::new(rules, overrides), why)),
        }
    }

    /// [`try_load`](Self::try_load) for a caller that lives one call: an
    /// unreadable `repos.json` is warned about and counts as having no scopes.
    pub fn load(config: &DaemonConfig) -> Self {
        Self::try_load(config).unwrap_or_else(|(policy, why)| {
            tracing::warn!("repos.json unreadable, its scopes are ignored: {why}");
            policy
        })
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
