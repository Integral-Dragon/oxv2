# Git Integration Design

ox-server manages a non-bare git repository. Agents clone from it,
work on branches, push back, and the merge step integrates branches
into main. This document captures the design decisions and pitfalls
that shape the implementation.

---

## Repository Model

ox-server's `--repo` flag points to a working git repository (not
bare). This is the source of truth for code and `.complex/` state.

- **ox-server** hosts a git smart HTTP endpoint at `/git/`
- **Agents** clone from this endpoint into isolated workspaces
- **Agents** push branches back to this endpoint
- **ox-server** merges branches to main via the `merge_to_main` action

The repo must stay on the `main` branch. ox-server never checks out
other branches — it only updates refs and performs merges.

---

## Agent Isolation (Seguro)

Agents run inside seguro VMs. They cannot access the host filesystem.
This is critical — without isolation, agents escape to the host repo:

**What goes wrong without isolation:**
- Claude Code detects the host project directory and `cd`s there
- `cx` commands and `git` operations hit the host repo directly
- The host repo's HEAD changes, branches get checked out, working
  tree gets dirty
- Merge operations fail because the host worktree is out of sync

**With seguro:**
- The agent only sees `/work/current` (the cloned workspace)
- `cx` operates on `.complex/` inside the clone
- `git push` goes to ox-server's HTTP endpoint, not the host filesystem
- The host repo stays clean on main

Never use `--local` runners for real work. Local runners are only
useful for debugging ox-runner itself.

---

## Branch Lifecycle

1. **Clone**: Runner does a full clone from ox-server (not
   `--single-branch`) so `origin/main` is always available for
   diffing and rebasing. If the branch doesn't exist yet, it clones
   main and creates the branch locally.

2. **Work**: Agent makes changes, runs `cx comment`, edits code,
   commits to the branch.

3. **Push**: Agent pushes the branch to ox-server's git endpoint.
   This updates the branch ref on the host repo without touching
   the working tree or HEAD.

4. **Merge**: The herder's `merge_to_main` action merges the branch
   into main using git2. This updates the main ref AND checks out
   the merged tree.

---

## Merge Implementation (merge.rs)

The merge uses git2 (libgit2) to perform merges without shelling
out to git. Two strategies:

### Fast-forward

When main hasn't diverged from the merge base:
- Update `refs/heads/main` to the branch commit
- Checkout HEAD if it points to main

### Merge commit

When main has diverged:
- Three-way merge of ancestor, main, and branch trees
- Check for conflicts (reject if any)
- Write merged tree, create merge commit on main
- **Checkout the merged tree** ← critical

### Working Tree Checkout

The merge commit path always checks out the merged tree after creating
the merge commit. Updating the ref without updating the working tree
leaves the files on disk out of sync with `main`, which causes the
dirty-worktree guard to reject subsequent merges.

### Dirty Worktree Check

The pre-merge check uses `repo.statuses()` to verify the worktree
is clean. This must exclude gitignored files:

```rust
let mut status_opts = git2::StatusOptions::new();
status_opts.include_ignored(false);
status_opts.include_untracked(true);
```

Without `include_ignored(false)`, directories like `.ox/run/` and
`target/` show up as dirty even though they're gitignored.

---

## cx on Branches

All cx mutations happen on branches, not main. This is enforced by
seguro isolation — the agent can only write to its workspace clone.

When the branch merges to main, cx state changes (comments, state
transitions, integrations) come with it. `ox-cx-watcher` observes
the new commits on main on its next tick and posts the corresponding
source events to `/api/events/ingest` — ox-server itself has no cx
polling logic.

**Important:** cx commands work on the local `.complex/` directory.
Inside the seguro VM, this is the clone's `.complex/`, not the
host's. The agent must commit and push `.complex/` changes for
them to reach main via merge.

---

## Git Push from Agents

The agent is responsible for pushing its work. The workflow prompts
include explicit `git push origin {branch}` instructions.

**Why agents push (not the runner):**
- The agent knows when its work is complete
- The agent may make multiple commits before pushing
- The runner doesn't know which files changed
- Keeping push in the agent's control matches the seguro model:
  the agent owns its workspace

**Why push must be in the code block, not just prose:**
Agents (Claude Code) follow code blocks literally. If the prompt
says "commit, push, and report done" but the code block only shows
`git commit` and `ox-rt done`, the agent will skip the push. Every
`ox-rt done` call must have a `git push` immediately before it in
the same code block.

---

## First-Boot State Recovery

On first boot (no cursor stored server-side for the `cx` source),
`ox-cx-watcher` snapshots the current cx state using `cx list --json`
instead of replaying the full `cx log` history. This prevents:

- Re-triggering workflows for integrated/shadowed nodes
- Firing stale `node.ready` events from replayed source transitions
- Creating duplicate executions for completed work

Subsequent ticks use `cx log --json --since <cursor>` to catch
incremental changes. See [prd/cx.md](prd/cx.md) for the watcher
lifecycle.

---

## Known Limitations

**Non-bare repo:** ox-server manages a working tree, which means
merge operations affect the filesystem. A bare repo would avoid
this but would break `cx` commands (which need a working tree to
read `.complex/` files). The workaround is careful checkout
management in merge.rs.

**Single writer:** Only ox-server writes to main (via merge). If a
human commits directly to main while ox is running, the cx watcher's
cursor and merge state may become inconsistent. Stop ox before
making manual changes to main.

**Branch cleanup:** Merged branches are not automatically deleted.
They accumulate as refs. This is harmless but messy.
