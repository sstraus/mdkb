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

/// Walk up from `start` (inclusive) looking for a directory that already
/// contains a `.mdkb/` store. Returns the nearest such directory, or `None`.
///
/// This is the primary anchor mechanism: a hook firing in a drifted sub-path
/// re-discovers the project's existing store instead of creating a new one,
/// and concurrent agents on the same project converge on the same store with
/// no shared state. Host-agnostic — depends only on the filesystem.
pub fn find_existing_store(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(".mdkb").is_dir() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Walk up from `start` (inclusive) looking for a git repository root — a
/// directory containing `.git` (a directory for a normal repo, a file for a
/// secondary worktree). Returns the nearest such directory, or `None`.
pub fn find_git_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

/// Walk up from `start` to `boundary` (both inclusive) looking for a directory
/// that contains a `.mdkb/` store. Never ascends above `boundary`, so the search
/// cannot escape the current git repository and attach to a parent directory's
/// store.
fn find_store_within(start: &Path, boundary: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if dir.join(".mdkb").is_dir() {
            return Some(dir.to_path_buf());
        }
        if dir == boundary {
            break;
        }
        current = dir.parent();
    }
    None
}

/// The user's home directory. See `crate::home` for the rule; this module had
/// the only correct copy of it, and now shares it rather than owning it.
fn home_dir() -> Option<PathBuf> {
    crate::home::dir()
}

/// True if `dir` holds git repositories among its immediate children — that is,
/// it is a container OF projects, not a project. Worktrees count: a secondary
/// worktree's `.git` is a file, not a directory, hence `exists()`.
///
/// Adopting a store here anchors every repo underneath it. Measured on the real
/// tree: `~/Gits` holds 4 such children and `~/Gits/LS/agent2__wt` holds 17.
fn holds_git_repos(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| e.path().join(".git").exists())
}

/// True if `dir` is the user's home directory, or an ancestor of it.
///
/// This is NOT covered by [`holds_git_repos`], and assuming it was is how the
/// second half of this bug survives: on a real machine `$HOME` has **zero**
/// repos as immediate children — they live under `~/Gits/...` — so the container
/// test passes it, while `~/.mdkb` was measured at 1.2 GB indexing the whole
/// home. A store at or above `$HOME` anchors everything the user owns.
fn is_home_or_above(dir: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|home| home.starts_with(dir))
}

/// Walk up from `start` (inclusive) to the nearest ancestor `.mdkb/` store that
/// belongs to a *project*, so a drifted sub-path of a non-git project still
/// re-discovers its own store instead of creating a second one beside it.
///
/// The walk stops without a result when the only thing above is a store that
/// would over-anchor: a container of repos, or `$HOME` and above. Stopping
/// rather than continuing is deliberate — everything further up is a broader
/// container still.
fn find_project_store_above(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        if is_home_or_above(dir, home) {
            return None;
        }
        if dir.join(".mdkb").is_dir() {
            return if holds_git_repos(dir) {
                None
            } else {
                Some(dir.to_path_buf())
            };
        }
        current = dir.parent();
    }
    None
}

/// Resolve the directory to anchor a `.mdkb/` store at, host-agnostically.
///
/// Priority:
///  1. inside a git repo — the nearest existing `.mdkb/` at or below the repo
///     root, else the repo root itself. The upward search is bounded by the git
///     root so a store is NEVER borrowed from a parent directory above the repo
///     (which would anchor the whole parent tree and reindex thousands of
///     unrelated files — including sibling repos and vendored dependencies);
///  2. outside a git repo — the nearest existing `.mdkb/` ancestor that is a
///     project rather than a container of projects (see
///     [`find_project_store_above`]). A non-git project keeps working from any
///     sub-path; a stray store on `~/Gits` or `$HOME` is never adopted;
///  3. `project_hint` (e.g. `CLAUDE_PROJECT_DIR`, the stable launch dir) when set;
///  4. `cwd` itself.
///
/// The chosen directory is then collapsed to the main worktree so that all
/// worktrees of a repo share a single store. This makes the anchor immune to
/// working-directory drift within the repo: `cwd` only matters as a starting
/// point for the bounded upward walk and as the last-resort fallback.
pub fn resolve_project_root(cwd: &Path, project_hint: Option<&Path>) -> PathBuf {
    let chosen = match find_git_root(cwd) {
        // Inside a repo: a repo owns exactly one store, at (or below) its own
        // root. Rediscover a drifted sub-path's store, but never climb past the
        // repo boundary.
        Some(git_root) => find_store_within(cwd, &git_root).unwrap_or(git_root),
        // Not a git repo, so there is no repo boundary — but "no boundary" must
        // not mean "adopt anything". The walk still climbs, because a non-git
        // project must keep working from a sub-path; it just refuses stores that
        // would anchor far more than the project (`~/Gits`, `$HOME`).
        None => find_project_store_above(cwd, home_dir().as_deref())
            .or_else(|| project_hint.map(Path::to_path_buf))
            .unwrap_or_else(|| cwd.to_path_buf()),
    };
    resolve_main_worktree(&chosen)
}

/// Discover ancestor `.mdkb/` stores **above** `primary`, for read-only
/// layering (e.g. a nested git repo inheriting its parent repo's knowledge).
///
/// Walks up from `primary`'s parent, collecting directories that ALREADY
/// contain a `.mdkb/`, nearest-first. It NEVER creates anything and never
/// includes `primary` itself — so layering can add read provenance without
/// reintroducing store proliferation: only stores a human/`init` already
/// created are surfaced. Returns at most `max_depth` ancestors (a small cap
/// keeps fan-out bounded); pass `usize::MAX` for unbounded.
pub fn discover_ancestor_stores(primary: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut ancestors = Vec::new();
    let mut current = primary.parent();
    while let Some(dir) = current {
        if ancestors.len() >= max_depth {
            break;
        }
        if dir.join(".mdkb").is_dir() {
            ancestors.push(dir.to_path_buf());
        }
        current = dir.parent();
    }
    ancestors
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

    // ── anchor resolution ────────────────────────────────────────────────────

    #[test]
    fn find_existing_store_walks_up_to_nearest() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".mdkb")).unwrap();
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_existing_store(&deep), Some(root.to_path_buf()));
        // inclusive of start
        assert_eq!(find_existing_store(root), Some(root.to_path_buf()));
    }

    #[test]
    fn find_existing_store_prefers_nearest() {
        let tmp = TempDir::new().unwrap();
        let outer = tmp.path();
        let inner = outer.join("project");
        std::fs::create_dir_all(outer.join(".mdkb")).unwrap();
        std::fs::create_dir_all(inner.join(".mdkb")).unwrap();
        let deep = inner.join("src");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_existing_store(&deep), Some(inner));
    }

    #[test]
    fn find_existing_store_none_when_absent() {
        let tmp = TempDir::new().unwrap();
        let deep = tmp.path().join("x/y");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_existing_store(&deep), None);
    }

    #[test]
    fn find_git_root_walks_up() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("sub/dir");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_git_root(&deep), Some(root.to_path_buf()));
    }

    #[test]
    fn resolve_project_root_existing_store_wins() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        // repo is a git root AND has a store one level down (legacy): existing wins
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let proj = repo.join("proj");
        std::fs::create_dir_all(proj.join(".mdkb")).unwrap();
        let deep = proj.join("src");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(resolve_project_root(&deep, None), proj);
    }

    #[test]
    fn resolve_project_root_falls_back_to_git_root() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        // No .mdkb anywhere → create anchor is the git root.
        assert_eq!(resolve_project_root(&deep, None), repo.to_path_buf());
    }

    #[test]
    fn discover_ancestor_stores_existing_only_never_creates() {
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path();
        let nested = parent.join("nested-repo");
        // parent has a store; nested (the primary) has its own.
        std::fs::create_dir_all(parent.join(".mdkb")).unwrap();
        std::fs::create_dir_all(nested.join(".mdkb")).unwrap();
        let no_store_mid = parent.join("nested-repo/mid");
        std::fs::create_dir_all(&no_store_mid).unwrap();

        // From the nested primary: parent surfaces as a read layer; the
        // primary itself is excluded.
        let got = discover_ancestor_stores(&nested, usize::MAX);
        assert_eq!(got, vec![parent.to_path_buf()]);

        // It must not have created any store along the way.
        assert!(!no_store_mid.join(".mdkb").exists());

        // No ancestor store → empty (and still nothing created).
        let lonely = TempDir::new().unwrap();
        let leaf = lonely.path().join("a/b");
        std::fs::create_dir_all(&leaf).unwrap();
        assert!(discover_ancestor_stores(&leaf, usize::MAX).is_empty());
        assert!(!leaf.join(".mdkb").exists());
    }

    #[test]
    fn discover_ancestor_stores_respects_depth_cap() {
        let tmp = TempDir::new().unwrap();
        let l0 = tmp.path();
        let l1 = l0.join("l1");
        let l2 = l1.join("l2");
        let primary = l2.join("primary");
        for d in [l0, &l1, &l2, &primary] {
            std::fs::create_dir_all(d.join(".mdkb")).unwrap();
        }
        // cap at 2 → only the two nearest ancestors (l2, l1), not l0.
        let got = discover_ancestor_stores(&primary, 2);
        assert_eq!(got, vec![l2.clone(), l1.clone()]);
    }

    #[test]
    fn resolve_project_root_never_climbs_above_git_root() {
        // Regression: a parent directory with a stray `.mdkb/` (e.g. ~/Gits)
        // must NOT hijack a repo below it. Anchoring to the parent makes the
        // daemon index the entire parent tree (sibling repos, vendored deps) —
        // the 100% CPU spike root cause.
        let tmp = TempDir::new().unwrap();
        let parent = tmp.path();
        std::fs::create_dir_all(parent.join(".mdkb")).unwrap(); // stray parent store
        let repo = parent.join("myrepo");
        std::fs::create_dir_all(repo.join(".git")).unwrap(); // real repo, no own store
        let deep = repo.join("src/inner");
        std::fs::create_dir_all(&deep).unwrap();

        // Must anchor to the repo's own git root, not the parent's store.
        assert_eq!(resolve_project_root(&deep, None), repo);
        // Sanity: the unbounded walk WOULD have returned the parent.
        assert_eq!(find_existing_store(&deep), Some(parent.to_path_buf()));
    }

    #[test]
    fn find_project_store_above_still_finds_a_non_git_project_from_a_sub_path() {
        // The behaviour the container guard must NOT cost us: a plain non-git
        // project keeps re-discovering its own store from any sub-path, with no
        // hint and no git root to bound the walk. Breaking this would silently
        // create a second store beside the first on every `mdkb` run from a
        // subfolder.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let project = home.join("work/notes");
        std::fs::create_dir_all(project.join(".mdkb")).unwrap();
        let deep = project.join("chapters/one");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(
            find_project_store_above(&deep, Some(&home)),
            Some(project.clone())
        );
        // And through the public entry point, with no hint at all.
        assert_eq!(resolve_project_root(&deep, None), project);
    }

    #[test]
    fn find_project_store_above_refuses_a_store_at_home_or_above() {
        // `holds_git_repos` does NOT catch this: on a real machine $HOME has no
        // repo as an immediate child (they live under ~/Gits/...), so only the
        // explicit home guard stops `~/.mdkb` — measured at 1.2 GB — from
        // anchoring the entire home directory.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join(".mdkb")).unwrap();
        let deep = home.join("Downloads/scratch");
        std::fs::create_dir_all(&deep).unwrap();

        // Sanity: the home store is invisible to the container test.
        assert!(!holds_git_repos(&home), "no repo as an immediate child");
        assert_eq!(find_project_store_above(&deep, Some(&home)), None);
    }

    #[test]
    fn holds_git_repos_sees_both_normal_repos_and_worktrees() {
        let tmp = TempDir::new().unwrap();
        let container = tmp.path();
        assert!(!holds_git_repos(container), "empty container holds nothing");

        // A normal repo: .git is a directory.
        std::fs::create_dir_all(container.join("repo/.git")).unwrap();
        assert!(holds_git_repos(container));

        // A worktree container: .git is a FILE, so `is_dir` would miss it.
        let wt = TempDir::new().unwrap();
        std::fs::create_dir_all(wt.path().join("feat-x")).unwrap();
        std::fs::write(wt.path().join("feat-x/.git"), "gitdir: /elsewhere").unwrap();
        assert!(holds_git_repos(wt.path()));
    }

    #[test]
    fn resolve_project_root_never_adopts_a_container_store_outside_a_repo() {
        // Regression (2026-08-14): the sibling of the test above, for the case
        // it never covered. A directory that merely HOLDS repos is not itself a
        // repo, so `find_git_root` returns None — true for `~/Gits`, `~/Gits/LS`
        // and worktree containers like `~/Gits/LS/agent2__wt`. The unbounded
        // walk then adopted the stray `~/Gits/.mdkb` and anchored the whole
        // tree: 3.99 GB of code.sqlite in 15 minutes, every core pegged.
        let tmp = TempDir::new().unwrap();
        let container = tmp.path(); // ~/Gits
        std::fs::create_dir_all(container.join(".mdkb")).unwrap(); // stray store
        // What makes it a container: it holds repos. On the real tree ~/Gits
        // held 4 as immediate children.
        std::fs::create_dir_all(container.join("sibling-repo/.git")).unwrap();
        // The cwd: a worktree container, itself not a repo (17 worktrees on the
        // real tree), so find_git_root returns None and the walk starts here.
        let worktree_container = container.join("LS/agent2__wt");
        std::fs::create_dir_all(&worktree_container).unwrap();
        std::fs::write(
            {
                let wt = worktree_container.join("feat-x");
                std::fs::create_dir_all(&wt).unwrap();
                wt.join(".git")
            },
            "gitdir: /elsewhere",
        )
        .unwrap();

        // No hint: anchor at cwd itself, never the container above it.
        assert_eq!(
            resolve_project_root(&worktree_container, None),
            worktree_container
        );
        // Sanity: the unbounded walk WOULD have returned the container — this is
        // the exact step the guard removes.
        assert_eq!(
            find_existing_store(&worktree_container),
            Some(container.to_path_buf())
        );
    }

    #[test]
    fn resolve_project_root_no_git_uses_hint_then_cwd() {
        let tmp = TempDir::new().unwrap();
        let launch = tmp.path().join("launch");
        let drifted = tmp.path().join("launch/deep/sub");
        std::fs::create_dir_all(&drifted).unwrap();

        // No store, no git: hint (launch dir) wins over the drifted cwd.
        assert_eq!(resolve_project_root(&drifted, Some(&launch)), launch);
        // No hint either: fall back to cwd as-is.
        assert_eq!(resolve_project_root(&drifted, None), drifted);
    }
}
