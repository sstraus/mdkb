use std::fs;
use std::process::Command;

use super::common::McpTestHarness;

pub trait McpFixtureSupport {
    fn create_file(&self, relative_path: &str, content: &str);
    fn add_collection(&self, name: &str, path: &str, pattern: &str);
    fn update_index(&self);
}

/// An `mdkb` child scoped to the fixture's own store.
///
/// `MDKB_NO_DAEMON` keeps the command in-process. Without it the CLI forwards the
/// work to whatever daemon is running on the developer's machine — a different
/// build, holding a different schema version — so the fixture set up a store the
/// tests then read through someone else's binary.
fn mdkb_child(root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mdkb"));
    cmd.current_dir(root).env("MDKB_NO_DAEMON", "1");
    cmd
}

impl McpFixtureSupport for McpTestHarness {
    fn create_file(&self, relative_path: &str, content: &str) {
        let full_path = self.root.join(relative_path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).expect("Failed to create parent dirs");
        }
        fs::write(&full_path, content).expect("Failed to write file");
    }

    fn add_collection(&self, name: &str, path: &str, pattern: &str) {
        let status = mdkb_child(&self.root)
            .args(["collection", "add", name, path, "--pattern", pattern])
            .status()
            .expect("Failed to run mdkb collection add");
        assert!(status.success(), "mdkb collection add failed");
    }

    fn update_index(&self) {
        let status = mdkb_child(&self.root)
            .arg("update")
            .status()
            .expect("Failed to run mdkb update");
        assert!(status.success(), "mdkb update failed");
    }
}
