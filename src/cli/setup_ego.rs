//! `mdkb setup hooks ego` — register mdkb as ego's prompt-context source.

use std::path::PathBuf;

use crate::error::Result;

/// What `mdkb setup hooks ego` did to the `mdkb_prompt_command` key.
#[derive(Debug, PartialEq, Eq)]
pub enum EgoSetupOutcome {
    /// The key was absent, or replaced under `--force`.
    Written,
    /// The key already held this binary path.
    Unchanged,
    /// The key held another value and `--force` was not given.
    KeptExisting(String),
}

/// Result of `mdkb setup hooks ego`.
#[derive(Debug)]
pub struct EgoSetupResult {
    pub config_path: PathBuf,
    pub binary_path: String,
    pub outcome: EgoSetupOutcome,
    pub dry_run: bool,
    pub merged_toml: String,
}

pub fn handle_setup_hooks_ego(_force: bool, _dry_run: bool) -> Result<EgoSetupResult> {
    unimplemented!()
}
