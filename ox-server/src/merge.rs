use std::path::Path;
use std::process::Command;

/// Result of a successful merge operation.
#[derive(Debug)]
pub struct MergeResult {
    pub prev_head: String,
    pub new_head: String,
}

/// Errors specific to the merge operation.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("worktree is dirty")]
    DirtyWorktree,
    #[error("branch has no commits ahead of main")]
    EmptyBranch,
    #[error("merge conflicts detected on branch '{branch}'")]
    Conflicts { branch: String },
    #[error("branch '{0}' not found")]
    BranchNotFound(String),
    #[allow(dead_code)]
    #[error("main branch not found")]
    MainNotFound,
    #[error("git error: {0}")]
    Git(String),
}

impl From<String> for MergeError {
    fn from(s: String) -> Self {
        MergeError::Git(s)
    }
}

/// Merge a branch into main using rebase + fast-forward.
///
/// 1. Record current main HEAD
/// 2. Rebase the branch onto main (abort on conflicts)
/// 3. Fast-forward main to the rebased branch
///
/// The repo is always left clean on main. If the rebase has conflicts,
/// it is aborted and a Conflicts error is returned.
pub fn merge_to_main(repo_path: &Path, branch: &str) -> Result<MergeResult, MergeError> {
    // Precondition: must be on main with clean worktree
    let current = git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current != "main" {
        let _ = git(repo_path, &["checkout", "main"]);
    }

    let status = git(repo_path, &["status", "--porcelain", "--ignore-submodules"])?;
    // Filter out ignored files (lines starting with !!)
    let dirty: Vec<&str> = status.lines().filter(|l| !l.starts_with("!!")).collect();
    if !dirty.is_empty() {
        return Err(MergeError::DirtyWorktree);
    }

    // Check branch exists
    if git(repo_path, &["rev-parse", "--verify", branch]).is_err() {
        return Err(MergeError::BranchNotFound(branch.to_string()));
    }

    let prev_head = git(repo_path, &["rev-parse", "main"])?;

    // Check branch has commits ahead of main
    let ahead = git(repo_path, &["rev-list", "--count", &format!("main..{branch}")])?;
    if ahead.trim() == "0" {
        return Err(MergeError::EmptyBranch);
    }

    // Squash: if branch has >1 commit ahead, collapse into one
    let ahead_count: u32 = ahead.trim().parse().unwrap_or(0);
    if ahead_count > 1 {
        // Collect all commit messages (oldest first)
        let messages = git(
            repo_path,
            &["log", "--reverse", "--format=%B", &format!("main..{branch}")],
        )?;

        // Checkout the branch, soft-reset to merge base, recommit
        git(repo_path, &["checkout", branch])
            .map_err(|e| MergeError::Git(format!("checkout branch for squash: {e}")))?;
        let merge_base = git(repo_path, &["merge-base", "main", "HEAD"])?;
        git(repo_path, &["reset", "--soft", &merge_base])
            .map_err(|e| MergeError::Git(format!("soft reset for squash: {e}")))?;

        let squash_commit = Command::new("git")
            .args(["commit", "-m", messages.trim()])
            .current_dir(repo_path)
            .output()
            .map_err(|e| MergeError::Git(e.to_string()))?;

        if !squash_commit.status.success() {
            let stderr = String::from_utf8_lossy(&squash_commit.stderr);
            return Err(MergeError::Git(format!("squash commit failed: {stderr}")));
        }

        // Back to main for rebase
        git(repo_path, &["checkout", "main"])
            .map_err(|e| MergeError::Git(format!("checkout main after squash: {e}")))?;
    }

    // Rebase branch onto main
    let rebase = Command::new("git")
        .args(["rebase", "main", branch])
        .current_dir(repo_path)
        .output()
        .map_err(|e| MergeError::Git(e.to_string()))?;

    if !rebase.status.success() {
        // Abort the failed rebase, return to main
        let _ = Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(repo_path)
            .output();
        let _ = Command::new("git")
            .args(["checkout", "main"])
            .current_dir(repo_path)
            .output();
        return Err(MergeError::Conflicts {
            branch: branch.to_string(),
        });
    }

    // Back to main
    git(repo_path, &["checkout", "main"])
        .map_err(|e| MergeError::Git(format!("checkout main after rebase: {e}")))?;

    // Fast-forward merge — guaranteed to work after a successful rebase
    let merge = Command::new("git")
        .args(["merge", "--ff-only", branch])
        .current_dir(repo_path)
        .output()
        .map_err(|e| MergeError::Git(e.to_string()))?;

    if !merge.status.success() {
        let stderr = String::from_utf8_lossy(&merge.stderr);
        return Err(MergeError::Git(format!("ff-only merge failed: {stderr}")));
    }

    let new_head = git(repo_path, &["rev-parse", "main"])?;

    Ok(MergeResult { prev_head, new_head })
}

/// Run a git command and return trimmed stdout.
fn git(repo_path: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_path)
        .output()
        .map_err(|e| e.to_string())?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(stderr.trim().to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
