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

    // Strategy 1: fast-forward if main hasn't diverged
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

    Ok(MergeResult::MergeCommit {
        prev_head,
        new_head: merge_oid.to_string(),
    })
}

