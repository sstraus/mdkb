use std::fs;
use std::process::Command;

use super::common::McpTestHarness;

pub trait McpFixtureSupport {
    fn create_file(&self, relative_path: &str, content: &str);
    fn add_collection(&self, name: &str, path: &str, pattern: &str);
    fn update_index(&self);
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
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .args(["collection", "add", name, path, "--pattern", pattern])
            .current_dir(&self.root)
            .status()
            .expect("Failed to run mdkb collection add");
        assert!(status.success(), "mdkb collection add failed");
    }

    fn update_index(&self) {
        let status = Command::new(env!("CARGO_BIN_EXE_mdkb"))
            .arg("update")
            .current_dir(&self.root)
            .status()
            .expect("Failed to run mdkb update");
        assert!(status.success(), "mdkb update failed");
    }
}
