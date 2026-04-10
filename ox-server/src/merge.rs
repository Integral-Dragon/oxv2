use anyhow::Result;
use git2::{BranchType, MergeOptions, Repository, Signature};
use std::path::Path;

/// Result of a successful merge operation.
#[derive(Debug)]
pub enum MergeResult {
    FastForward {
        prev_head: String,
        new_head: String,
    },
    MergeCommit {
        prev_head: String,
        new_head: String,
    },
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
    #[error("main branch not found")]
    MainNotFound,
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
}

/// Merge a branch into main. Returns the merge result or an error.
///
/// Supports two strategies:
/// - Fast-forward when main hasn't diverged
/// - Merge commit when main has diverged (rejects on conflicts)
pub fn merge_to_main(repo_path: &Path, branch: &str) -> Result<MergeResult, MergeError> {
    // Try to rebase the branch onto main so we get a clean fast-forward.
    // This handles the case where main moved forward while the branch was
    // being reviewed (e.g. another task merged). If the rebase has
    // conflicts, abort it cleanly and report the conflict — never leave
    // the repo in a broken state.
    try_rebase_onto_main(repo_path, branch)?;

    let repo = Repository::open(repo_path).map_err(MergeError::Git)?;

    // Precondition: worktree must be clean (excluding gitignored files)
    let mut status_opts = git2::StatusOptions::new();
    status_opts.include_ignored(false);
    status_opts.include_untracked(true);
    let statuses = repo.statuses(Some(&mut status_opts)).map_err(MergeError::Git)?;
    if !statuses.is_empty() {
        return Err(MergeError::DirtyWorktree);
    }

    let main_branch = repo
        .find_branch("main", BranchType::Local)
        .map_err(|_| MergeError::MainNotFound)?;
    let main_commit = main_branch
        .get()
        .peel_to_commit()
        .map_err(MergeError::Git)?;

    let feature_branch = repo
        .find_branch(branch, BranchType::Local)
        .map_err(|_| MergeError::BranchNotFound(branch.to_string()))?;
    let branch_commit = feature_branch
        .get()
        .peel_to_commit()
        .map_err(MergeError::Git)?;

    // Precondition: branch must have commits ahead of merge base
    let merge_base = repo
        .merge_base(main_commit.id(), branch_commit.id())
        .map_err(MergeError::Git)?;

    if branch_commit.id() == merge_base {
        return Err(MergeError::EmptyBranch);
    }

    let prev_head = main_commit.id().to_string();

    // After rebase, this should be a fast-forward. But handle merge commits
    // as a fallback in case the rebase was a no-op.
    if main_commit.id() == merge_base {
        repo.reference(
            "refs/heads/main",
            branch_commit.id(),
            true,
            &format!("fast-forward merge of {branch}"),
        )
        .map_err(MergeError::Git)?;

        // Update HEAD if it points to main
        if repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().map(|s| s == "main"))
            .unwrap_or(false)
        {
            repo.checkout_head(Some(git2::build::CheckoutBuilder::new().force()))
                .map_err(MergeError::Git)?;
        }

        return Ok(MergeResult::FastForward {
            prev_head,
            new_head: branch_commit.id().to_string(),
        });
    }

    // Strategy 2: merge commit
    let merge_base_commit = repo.find_commit(merge_base).map_err(MergeError::Git)?;
    let main_tree = main_commit.tree().map_err(MergeError::Git)?;
    let branch_tree = branch_commit.tree().map_err(MergeError::Git)?;
    let ancestor_tree = merge_base_commit.tree().map_err(MergeError::Git)?;

    let mut index = repo
        .merge_trees(&ancestor_tree, &main_tree, &branch_tree, Some(&MergeOptions::new()))
        .map_err(MergeError::Git)?;

    if index.has_conflicts() {
        return Err(MergeError::Conflicts {
            branch: branch.to_string(),
        });
    }

    let tree_oid = index.write_tree_to(&repo).map_err(MergeError::Git)?;
    let tree = repo.find_tree(tree_oid).map_err(MergeError::Git)?;
    let sig = Signature::now("ox-server", "ox@localhost").map_err(MergeError::Git)?;

    let merge_oid = repo
        .commit(
            Some("refs/heads/main"),
            &sig,
            &sig,
            &format!("Merge branch '{branch}' into main"),
            &tree,
            &[&main_commit, &branch_commit],
        )
        .map_err(MergeError::Git)?;

    // Checkout the merged tree so the working directory matches the new HEAD
    repo.checkout_tree(
        tree.as_object(),
        Some(git2::build::CheckoutBuilder::new().force()),
    )
    .map_err(MergeError::Git)?;

    Ok(MergeResult::MergeCommit {
        prev_head,
        new_head: merge_oid.to_string(),
    })
}

/// Try to rebase a branch onto main. If there are conflicts, abort the
/// rebase and return a Conflicts error. The repo is always left clean.
fn try_rebase_onto_main(repo_path: &Path, branch: &str) -> Result<(), MergeError> {
    use std::process::Command;

    // Check if rebase is needed (branch behind main)
    let merge_base = Command::new("git")
        .args(["merge-base", "main", branch])
        .current_dir(repo_path)
        .output()
        .map_err(|e| MergeError::Git(git2::Error::from_str(&e.to_string())))?;

    let main_head = Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(repo_path)
        .output()
        .map_err(|e| MergeError::Git(git2::Error::from_str(&e.to_string())))?;

    let base = String::from_utf8_lossy(&merge_base.stdout).trim().to_string();
    let main = String::from_utf8_lossy(&main_head.stdout).trim().to_string();

    if base == main {
        // Branch is already up to date with main, no rebase needed
        return Ok(());
    }

    // Attempt the rebase
    let result = Command::new("git")
        .args(["rebase", "main", branch])
        .current_dir(repo_path)
        .output()
        .map_err(|e| MergeError::Git(git2::Error::from_str(&e.to_string())))?;

    if result.status.success() {
        // Rebase succeeded — branch is now ahead of main, ready for fast-forward
        // Switch back to main
        let _ = Command::new("git")
            .args(["checkout", "main"])
            .current_dir(repo_path)
            .output();
        return Ok(());
    }

    // Rebase failed (conflicts) — abort and report
    let _ = Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(repo_path)
        .output();

    // Make sure we're back on main
    let _ = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(repo_path)
        .output();

    Err(MergeError::Conflicts {
        branch: branch.to_string(),
    })
}

