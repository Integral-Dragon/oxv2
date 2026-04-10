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

/// Merge a branch into main. Never leaves main's HEAD.
///
/// Strategy:
/// - 1 commit ahead → fast-forward (preserves the commit as-is)
/// - >1 commits ahead + squash → `git merge --squash` (single commit on main)
/// - >1 commits ahead + no squash → `git merge --no-ff` (merge commit)
///
/// All paths stay on main. No branch checkout. No rebase.
pub fn merge_to_main(repo_path: &Path, branch: &str, squash: bool) -> Result<MergeResult, MergeError> {
    // Ensure we're on main with clean worktree.
    // Abort any stale rebase/merge from a previous failed attempt.
    let _ = git(repo_path, &["rebase", "--abort"]);
    let _ = git(repo_path, &["merge", "--abort"]);
    let current = git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if current != "main" {
        git(repo_path, &["checkout", "main"])
            .map_err(|e| MergeError::Git(format!("failed to checkout main: {e}")))?;
    }

    let status = git(repo_path, &["status", "--porcelain", "--ignore-submodules"])?;
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
    let ahead_count: u32 = ahead.trim().parse().unwrap_or(0);
    if ahead_count == 0 {
        return Err(MergeError::EmptyBranch);
    }

    // 1 commit ahead → fast-forward (preserves agent's commit message)
    if ahead_count == 1 {
        let result = Command::new("git")
            .args(["merge", "--ff-only", branch])
            .current_dir(repo_path)
            .output()
            .map_err(|e| MergeError::Git(e.to_string()))?;

        if !result.status.success() {
            // ff-only failed — branch diverged from main
            let stderr = String::from_utf8_lossy(&result.stderr);
            return Err(MergeError::Conflicts {
                branch: format!("{branch} (ff-only failed: {stderr})"),
            });
        }

        let new_head = git(repo_path, &["rev-parse", "main"])?;
        return Ok(MergeResult { prev_head, new_head });
    }

    // >1 commits ahead + squash → squash merge (single commit on main)
    if squash {
        // Collect all commit messages for the squash commit
        let messages = git(
            repo_path,
            &["log", "--reverse", "--format=%B", &format!("main..{branch}")],
        )?;

        let merge_result = Command::new("git")
            .args(["merge", "--squash", branch])
            .current_dir(repo_path)
            .output()
            .map_err(|e| MergeError::Git(e.to_string()))?;

        if !merge_result.status.success() {
            // Squash merge had conflicts — abort
            let _ = git(repo_path, &["reset", "--hard", "HEAD"]);
            return Err(MergeError::Conflicts {
                branch: branch.to_string(),
            });
        }

        // Commit the squashed changes with concatenated messages
        let commit_result = Command::new("git")
            .args(["commit", "-m", messages.trim()])
            .current_dir(repo_path)
            .output()
            .map_err(|e| MergeError::Git(e.to_string()))?;

        if !commit_result.status.success() {
            let stderr = String::from_utf8_lossy(&commit_result.stderr);
            let _ = git(repo_path, &["reset", "--hard", "HEAD"]);
            return Err(MergeError::Git(format!("squash commit failed: {stderr}")));
        }

        let new_head = git(repo_path, &["rev-parse", "main"])?;
        return Ok(MergeResult { prev_head, new_head });
    }

    // >1 commits ahead + no squash → merge commit
    let merge_result = Command::new("git")
        .args(["merge", "--no-ff", "--no-edit", branch])
        .current_dir(repo_path)
        .output()
        .map_err(|e| MergeError::Git(e.to_string()))?;

    if !merge_result.status.success() {
        let _ = git(repo_path, &["merge", "--abort"]);
        return Err(MergeError::Conflicts {
            branch: branch.to_string(),
        });
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
