use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git is not installed or not on PATH")]
    Unavailable,
    #[error("git {args:?} failed: {stderr}")]
    Failed { args: Vec<String>, stderr: String },
    #[error("nothing to undo")]
    NothingToUndo,
}

/// Seeded into the repository's own `info/exclude` (never the vault) so
/// `commit`'s `add --all` does not sweep in Obsidian plugin state or
/// OS/editor cruft.
const VAULT_EXCLUDE: &str = "\
.obsidian/
.trash/
.DS_Store
*.swp
*~
";

/// A per-project git repository whose directory lives OUTSIDE the backend root,
/// so the vault has no `.git` and no sync tool replicates git internals (§6).
pub struct GitNet {
    repo_dir: PathBuf,
    work_tree: PathBuf,
}

impl GitNet {
    pub fn new(repo_dir: PathBuf, work_tree: PathBuf) -> Self {
        Self {
            repo_dir,
            work_tree,
        }
    }

    pub fn is_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.repo_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(args)
            .output()
            .map_err(|_| GitError::Unavailable)?;
        if !out.status.success() {
            return Err(GitError::Failed {
                args: args.iter().map(|s| s.to_string()).collect(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    /// True once HEAD resolves to a commit. False (not an error) on a repo
    /// with zero commits, where HEAD is unborn.
    ///
    /// `git rev-parse --verify -q HEAD` exits non-zero with EMPTY stderr
    /// both for a genuinely unborn HEAD and for a `refs/heads` directory
    /// git can't read (verified empirically: `chmod 000` on `refs/heads`
    /// produces byte-for-byte the same silent exit-1 as a fresh repo on
    /// git 2.50.1 — git's files ref-backend appears to treat a failed
    /// opendir the same as "no refs found" rather than erroring). So an
    /// stderr-emptiness check cannot tell the two apart; check readability
    /// of `refs/heads` ourselves first, before trusting git's silence.
    fn head_exists(&self) -> Result<bool, GitError> {
        match std::fs::read_dir(self.repo_dir.join("refs").join("heads")) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(GitError::Failed {
                    args: vec!["refs/heads".into()],
                    stderr: e.to_string(),
                });
            }
        }

        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.repo_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(["rev-parse", "--verify", "-q", "HEAD"])
            .output()
            .map_err(|_| GitError::Unavailable)?;
        Ok(out.status.success())
    }

    pub fn ensure_init(&self) -> Result<(), GitError> {
        let just_created = !self.repo_dir.join("HEAD").exists();
        if just_created {
            std::fs::create_dir_all(&self.repo_dir).map_err(|_| GitError::Unavailable)?;
            let out = Command::new("git")
                .args(["init", "--bare", "--initial-branch=main"])
                .arg(&self.repo_dir)
                .output()
                .map_err(|_| GitError::Unavailable)?;
            if !out.status.success() {
                return Err(GitError::Failed {
                    args: vec!["init".into()],
                    stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
                });
            }
            // `git init --bare` seeds info/exclude with its own commented-out
            // sample. Replace it with cadet's own so a first commit never
            // sweeps in Obsidian plugin state or OS/editor cruft. Only done
            // on the init that actually created the repo, so a user who
            // edits this file afterwards keeps their changes.
            std::fs::write(self.repo_dir.join("info").join("exclude"), VAULT_EXCLUDE)
                .map_err(|_| GitError::Unavailable)?;
        }
        // A bare repo has core.bare=true, which forbids --work-tree operations.
        // Verify rather than blindly re-apply: a crash between `init --bare`
        // and this point would otherwise leave a repo that looks initialized
        // (HEAD exists) but still rejects every --work-tree operation, with
        // no future ensure_init call ever repairing it.
        if self.run(&["config", "--get", "core.bare"])?.trim() != "false" {
            self.run(&["config", "core.bare", "false"])?;
        }
        Ok(())
    }

    pub fn commit(&self, message: &str) -> Result<(), GitError> {
        self.run(&["add", "--all"])?;
        if self.run(&["status", "--porcelain"])?.trim().is_empty() {
            return Ok(());
        }
        self.run(&[
            "-c",
            "user.name=cadet",
            "-c",
            "user.email=cadet@localhost",
            "commit",
            "--no-gpg-sign",
            "-m",
            message,
        ])?;
        Ok(())
    }

    /// Moves the work tree back to the state before the most recent commit.
    /// Any uncommitted edit present at the time of the call is preserved
    /// first as its own commit, so `undo` can never itself be the thing
    /// that destroys unsaved work — it stays reachable in the repository's
    /// history and reflog even after the reset.
    pub fn undo(&self) -> Result<(), GitError> {
        if !self.head_exists()? {
            return Err(GitError::NothingToUndo);
        }
        let count: u32 = self
            .run(&["rev-list", "--count", "HEAD"])?
            .trim()
            .parse()
            .unwrap_or(0);
        if count < 2 {
            return Err(GitError::NothingToUndo);
        }

        // Resolve the destination BEFORE taking any snapshot, or the
        // snapshot commit shifts what HEAD~1 means.
        let target = self.run(&["rev-parse", "HEAD~1"])?.trim().to_string();

        // Preserve any uncommitted work so `undo` can never be the thing
        // that loses it.
        if !self.run(&["status", "--porcelain"])?.trim().is_empty() {
            self.commit("snapshot before undo")?;
        }

        self.run(&["reset", "--hard", &target])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, tempfile::TempDir, GitNet) {
        let repo = tempfile::tempdir().unwrap();
        let tree = tempfile::tempdir().unwrap();
        let g = GitNet::new(repo.path().to_path_buf(), tree.path().to_path_buf());
        g.ensure_init().unwrap();
        (repo, tree, g)
    }

    /// Inspects the tempdir-scoped repository directly, independent of
    /// `GitNet::run`, so tests can assert on history/reflog state that
    /// `GitNet`'s public API deliberately doesn't expose. Always scoped to
    /// the tempdir paths passed in — never the real repository.
    fn raw_git(repo_dir: &std::path::Path, work_tree: &std::path::Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(repo_dir)
            .arg("--work-tree")
            .arg(work_tree)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "raw git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    #[test]
    fn init_puts_no_git_directory_in_the_work_tree() {
        let (_repo, tree, _g) = setup();
        assert!(
            !tree.path().join(".git").exists(),
            "the vault must stay clean"
        );
    }

    #[test]
    fn commit_then_undo_restores_the_previous_content() {
        let (_repo, tree, g) = setup();
        let f = tree.path().join("a.md");

        std::fs::write(&f, "one\n").unwrap();
        g.commit("first").unwrap();

        std::fs::write(&f, "two\n").unwrap();
        g.commit("second").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "two\n");

        g.undo().unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "one\n");
    }

    #[test]
    fn undo_restores_a_deleted_file() {
        let (_repo, tree, g) = setup();
        let f = tree.path().join("a.md");
        std::fs::write(&f, "one\n").unwrap();
        g.commit("add").unwrap();
        std::fs::remove_file(&f).unwrap();
        g.commit("delete").unwrap();
        g.undo().unwrap();
        assert!(f.exists(), "undo must bring back a deleted task");
    }

    #[test]
    fn committing_with_no_changes_is_not_an_error() {
        let (_repo, _tree, g) = setup();
        g.commit("nothing").unwrap();
        g.commit("still nothing").unwrap();
    }

    #[test]
    fn undo_preserves_an_uncommitted_edit() {
        let (repo, tree, g) = setup();
        let f = tree.path().join("a.md");

        std::fs::write(&f, "one\n").unwrap();
        g.commit("first").unwrap();

        std::fs::write(&f, "two\n").unwrap();
        g.commit("second").unwrap();

        // A hand-edit made in the vault after the last commit, never
        // explicitly committed.
        std::fs::write(&f, "uncommitted\n").unwrap();

        g.undo().unwrap();

        assert_eq!(
            std::fs::read_to_string(&f).unwrap(),
            "one\n",
            "undo must still roll the work tree back to before the previous commit"
        );

        // The hand-edit must not have been silently destroyed: it must be
        // reachable from history via the snapshot commit undo took first.
        let history = raw_git(repo.path(), tree.path(), &["log", "--walk-reflogs", "-p"]);
        assert!(
            history.contains("uncommitted"),
            "the uncommitted edit must remain recoverable from history: {history}"
        );
    }

    #[test]
    fn undo_on_a_repository_with_one_commit_is_nothing_to_undo() {
        let (_repo, tree, g) = setup();
        std::fs::write(tree.path().join("a.md"), "one\n").unwrap();
        g.commit("first").unwrap();
        assert!(matches!(g.undo(), Err(GitError::NothingToUndo)));
    }

    #[test]
    fn undo_on_an_empty_repository_is_nothing_to_undo() {
        let (_repo, _tree, g) = setup();
        assert!(matches!(g.undo(), Err(GitError::NothingToUndo)));
    }

    #[test]
    fn init_puts_the_repository_in_the_repo_dir() {
        let (repo, _tree, _g) = setup();
        assert!(
            repo.path().join("HEAD").exists(),
            "the repository must actually be created in repo_dir"
        );
    }

    #[test]
    fn ensure_init_is_idempotent() {
        let (_repo, tree, g) = setup();
        g.ensure_init().unwrap();
        g.ensure_init().unwrap();
        std::fs::write(tree.path().join("a.md"), "one\n").unwrap();
        g.commit("first").unwrap();
    }

    #[test]
    fn the_obsidian_directory_is_not_committed() {
        let (repo, tree, g) = setup();
        std::fs::create_dir_all(tree.path().join(".obsidian")).unwrap();
        std::fs::write(tree.path().join(".obsidian").join("workspace.json"), "{}\n").unwrap();
        std::fs::write(tree.path().join("a.md"), "task\n").unwrap();

        g.commit("first").unwrap();

        let tracked = raw_git(
            repo.path(),
            tree.path(),
            &["ls-tree", "-r", "--name-only", "HEAD"],
        );
        assert!(
            !tracked.contains(".obsidian"),
            "the vault's .obsidian directory must never be tracked: {tracked}"
        );
        assert!(tracked.contains("a.md"));
    }

    #[cfg(unix)]
    #[test]
    fn undo_propagates_a_real_error_instead_of_reporting_nothing_to_undo() {
        use std::os::unix::fs::PermissionsExt;

        let (repo, tree, g) = setup();
        std::fs::write(tree.path().join("a.md"), "one\n").unwrap();
        g.commit("first").unwrap();
        std::fs::write(tree.path().join("a.md"), "two\n").unwrap();
        g.commit("second").unwrap();

        let refs_heads = repo.path().join("refs").join("heads");
        let original_mode = std::fs::metadata(&refs_heads).unwrap().permissions();
        std::fs::set_permissions(&refs_heads, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = g.undo();

        // Always restore permissions before asserting, so the tempdir can
        // still be cleaned up even if the assertion below panics.
        std::fs::set_permissions(&refs_heads, original_mode).unwrap();

        assert!(
            !matches!(result, Err(GitError::NothingToUndo)),
            "an unreadable refs directory on a repo with real history must not be reported as nothing to undo, got {result:?}"
        );
        assert!(
            matches!(result, Err(GitError::Failed { .. })),
            "expected GitError::Failed, got {result:?}"
        );
    }
}
