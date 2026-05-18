//! Git repository utilities (worktree detection, path resolution).

use std::io::Read as _;
use std::path::{Path, PathBuf};

/// Maximum bytes to read from a `.git` file. A valid `gitdir:` line is a
/// short prefix plus a PATH_MAX-bounded path — 512 bytes is generous.
const MAX_DOT_GIT_SIZE: u64 = 512;

/// If `root` is a git worktree (`.git` is a file, not a directory), follow
/// the `gitdir:` pointer back to the main worktree root.  For normal repos
/// (`.git` is a directory) or non-git directories, returns `root` unchanged.
///
/// The expected `.git` file format is `gitdir: <main>/.git/worktrees/<name>`,
/// stable since git 2.5 (2015). See `git help worktree`.
pub fn resolve_main_worktree(root: &Path) -> PathBuf {
    fn try_resolve(root: &Path) -> Option<PathBuf> {
        let dot_git = root.join(".git");
        if !dot_git.is_file() {
            return None;
        }

        let contents = match std::fs::File::open(&dot_git) {
            Ok(f) => {
                let mut buf = String::new();
                f.take(MAX_DOT_GIT_SIZE)
                    .read_to_string(&mut buf)
                    .map_err(|e| {
                        tracing::warn!(
                            path = %dot_git.display(),
                            error = %e,
                            "Failed to read .git file; treating as non-worktree"
                        );
                    })
                    .ok()?;
                buf
            }
            Err(e) => {
                tracing::warn!(
                    path = %dot_git.display(),
                    error = %e,
                    "Failed to open .git file; treating as non-worktree"
                );
                return None;
            }
        };

        let suffix = contents.trim().strip_prefix("gitdir: ")?;
        let gitdir = PathBuf::from(suffix);
        let gitdir = if gitdir.is_relative() {
            root.join(gitdir)
        } else {
            gitdir
        };

        let worktrees_dir = gitdir.parent()?;
        if worktrees_dir.file_name()?.to_str()? != "worktrees" {
            tracing::debug!(
                gitdir = %gitdir.display(),
                "gitdir path does not match worktree layout (<main>/.git/worktrees/<name>); using root as-is"
            );
            return None;
        }
        let dot_git_dir = worktrees_dir.parent()?;
        if dot_git_dir.file_name()?.to_str()? != ".git" {
            tracing::debug!(
                gitdir = %gitdir.display(),
                "gitdir path does not match worktree layout (<main>/.git/worktrees/<name>); using root as-is"
            );
            return None;
        }
        let main_root = dot_git_dir.parent()?;

        // Sanity check: the resolved main root should have a .git directory.
        if !main_root.join(".git").is_dir() {
            tracing::debug!(
                main_root = %main_root.display(),
                "Resolved main worktree root has no .git directory; using root as-is"
            );
            return None;
        }

        Some(main_root.to_path_buf())
    }

    try_resolve(root).unwrap_or_else(|| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn normal_repo_unchanged() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let resolved = resolve_main_worktree(root);
        assert_eq!(resolved, root.to_path_buf());
    }

    #[test]
    fn no_git_unchanged() {
        let tmp = TempDir::new().unwrap();
        let resolved = resolve_main_worktree(tmp.path());
        assert_eq!(resolved, tmp.path().to_path_buf());
    }

    #[test]
    fn secondary_worktree_resolves_to_main() {
        let tmp = TempDir::new().unwrap();
        let main_root = tmp.path().join("main-repo");
        let wt_root = tmp.path().join("worktree-release");

        // Main repo: real .git directory + worktrees subdir
        std::fs::create_dir_all(main_root.join(".git/worktrees/release")).unwrap();
        // Worktree: .git file pointing to main
        std::fs::create_dir_all(&wt_root).unwrap();
        std::fs::write(
            wt_root.join(".git"),
            format!(
                "gitdir: {}\n",
                main_root.join(".git/worktrees/release").display()
            ),
        )
        .unwrap();

        let resolved = resolve_main_worktree(&wt_root);
        assert_eq!(resolved, main_root);
    }

    #[test]
    fn relative_gitdir_resolves_after_canonicalize() {
        let tmp = TempDir::new().unwrap();
        let main_root = tmp.path().join("main-repo");
        let wt_root = tmp.path().join("worktree-feat");

        std::fs::create_dir_all(main_root.join(".git/worktrees/feat")).unwrap();
        std::fs::create_dir_all(&wt_root).unwrap();
        std::fs::write(
            wt_root.join(".git"),
            "gitdir: ../main-repo/.git/worktrees/feat\n",
        )
        .unwrap();

        let resolved = resolve_main_worktree(&wt_root);
        // Relative path — raw result contains ".." but canonicalizes correctly.
        assert!(resolved.ends_with("main-repo"));
        let canonical = resolved.canonicalize().unwrap();
        assert_eq!(canonical, main_root.canonicalize().unwrap());
    }

    #[test]
    fn malformed_git_file_unchanged() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".git"), "not a gitdir line\n").unwrap();

        let resolved = resolve_main_worktree(tmp.path());
        assert_eq!(resolved, tmp.path().to_path_buf());
    }

    #[test]
    fn empty_git_file_unchanged() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join(".git"), "").unwrap();

        let resolved = resolve_main_worktree(tmp.path());
        assert_eq!(resolved, tmp.path().to_path_buf());
    }

    #[test]
    fn gitdir_not_matching_worktree_structure_unchanged() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        // gitdir points to a valid path but not <main>/.git/worktrees/<name>
        std::fs::write(root.join(".git"), "gitdir: /some/bare-repo.git\n").unwrap();

        let resolved = resolve_main_worktree(root);
        assert_eq!(resolved, root.to_path_buf());
    }

    #[test]
    fn gitdir_to_submodule_path_unchanged() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        let modules_dir = tmp.path().join("parent/.git/modules/sub");
        std::fs::create_dir_all(&modules_dir).unwrap();
        std::fs::write(
            root.join(".git"),
            format!("gitdir: {}\n", modules_dir.display()),
        )
        .unwrap();

        let resolved = resolve_main_worktree(root);
        assert_eq!(resolved, root.to_path_buf());
    }

    #[test]
    fn main_root_without_git_dir_unchanged() {
        let tmp = TempDir::new().unwrap();
        let main_root = tmp.path().join("main-repo");
        let wt_root = tmp.path().join("worktree-fix");

        // Set up worktree structure but main_root has NO .git directory
        std::fs::create_dir_all(main_root.join(".git/worktrees/fix")).unwrap();
        // Remove the .git dir from main_root and replace with a file (not a dir)
        std::fs::remove_dir_all(main_root.join(".git")).unwrap();
        // Re-create just the worktrees path (without .git being a directory at main_root)
        // This simulates a broken state where the structure exists but main_root/.git is not a dir
        let fake_git = main_root.join(".git_fake/worktrees/fix");
        std::fs::create_dir_all(&fake_git).unwrap();

        std::fs::create_dir_all(&wt_root).unwrap();
        std::fs::write(
            wt_root.join(".git"),
            format!("gitdir: {}\n", fake_git.display()),
        )
        .unwrap();

        // .git_fake doesn't match the ".git" component name check, so falls back
        let resolved = resolve_main_worktree(&wt_root);
        assert_eq!(resolved, wt_root);
    }
}
