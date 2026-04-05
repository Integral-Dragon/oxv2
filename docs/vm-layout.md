# VM Filesystem Layout

This document describes the filesystem inside a seguro VM running
ox-runner. The same layout applies to any execution environment — the
VM is just the reference case.

---

## Shares

The VM has two virtiofs mounts from the host:

### `/ox` (read-only)

Ox binaries and tools. Mounted read-only from a host directory
containing the ox installation and agent CLIs.

```
/ox/
  bin/
    ox-runner         # the runner binary
    ox-rt             # runtime interface helper script
    cx                # cx CLI (file-native issue tracker)
    claude            # Claude Code CLI
    codex             # Codex CLI
    git               # or use the system git
```

`/ox/bin` is prepended to `$PATH`. The VM image does not need to
include these — they are mounted from the host and shared across VMs.

### `/work` (read-write)

Workspace storage. Mounted read-write from a host directory, one per
runner. This is where git clones go. A host-backed share is required
because workspaces can be large (Rust builds produce GBs of artifacts).

```
/work/
  current/            # active step workspace (git clone)
    .git/
    src/
    .complex/
    CLAUDE.md          # persona file placed by ox-runner
    .ssh/id_ed25519    # secret-derived file (if any)
  tmp/                 # temp files for the current step
    ox-run-4a2f-aJuO-e1-implement-1.sock   # runtime interface socket
    prompt.md          # assembled prompt file
```

ox-runner creates `/work/current/` at step start (git clone) and
removes it at step end (cleanup). Between steps, `/work` is empty.

`/work/tmp/` holds ephemeral files for the step: the unix socket,
the assembled prompt file, and any other temp files. Cleaned up with
the workspace.

---

## Environment Variables

Set by the provisioning script when starting ox-runner:

| Variable | Value | Description |
|----------|-------|-------------|
| `OX_SERVER` | `http://host:4840` | ox-server URL — runner registers here on startup |
| `OX_ENVIRONMENT` | `seguro` | Reported on registration for labeling |
| `OX_WORKSPACE_DIR` | `/work` | Where ox-runner creates step workspaces |

Set by ox-runner per step:

| Variable | Value | Description |
|----------|-------|-------------|
| `OX_SOCKET` | `/work/tmp/ox-run-....sock` | Runtime interface socket path |
| *(secrets)* | *(from dispatch)* | Secret-derived env vars (e.g. `ANTHROPIC_API_KEY`) |
| *(runtime env)* | *(from dispatch)* | Runtime-defined env vars (e.g. `CLAUDE_MODEL`) |
| *(proxy overrides)* | *(from dispatch)* | API proxy addresses (e.g. `ANTHROPIC_BASE_URL=http://127.0.0.1:...`) |

---

## What the VM Does NOT Contain

- **`.ox/` configuration directory** — ox-runner does not need local
  config. Runtime definitions, persona files, and workflow definitions
  are resolved by ox-server and included in the dispatch payload. The
  runner receives a fully-resolved step spec: the command to run, env
  vars to set, files to place (with content inline), and proxy
  declarations.

- **ox-server, ox-herder, or ox-ctl** — these run on the host, not
  inside runner VMs.

- **Persistent state** — the VM is ephemeral from ox's perspective.
  Nothing persists between runner lifetimes. Within a runner's lifetime,
  nothing persists between steps (workspace is cleaned between steps).

---

## Step Lifecycle Inside the VM

```
1. ox-runner starts (mounted from /ox/bin)
2. Registers with $OX_SERVER → receives runner_id
3. Subscribes to SSE, starts heartbeat loop
4. Waits for step.dispatched...

On step.dispatched:
  5. mkdir /work/current && git clone $OX_SERVER/git/ /work/current --branch <branch>
  6. Place files from dispatch payload:
     - Persona content → /work/current/CLAUDE.md
     - Secret files → /work/current/.ssh/id_ed25519 (mode 0600)
  7. Write prompt → /work/tmp/prompt.md
  8. Create socket → /work/tmp/ox-run-*.sock
  9. Start API proxies (if declared)
  10. Spawn runtime process in /work/current/
      (command, env, proxy overrides all from dispatch payload)
  11. Stream stdout/stderr as log artifact
  12. Wait for exit...

On runtime exit:
  13. Collect signals, push branch, confirm (or fail)
  14. rm -rf /work/current /work/tmp/*
  15. Return to idle → goto 4

On drain or SIGTERM:
  16. Finish current step if running
  17. Exit — VM can be destroyed
```

---

## Provisioning Script

The provisioning script (outside ox) is responsible for:

1. Creating a seguro VM with the two virtiofs mounts
2. Starting ox-runner inside the VM with the right env vars
3. Optionally: destroying the VM when ox-runner exits

A minimal example:

```bash
#!/usr/bin/env bash
# start-runner.sh — start an ox-runner in a seguro VM

RUNNER_WORK=$(mktemp -d /var/ox/runners/XXXXXX)

seguro run \
  --share /opt/ox/bin:/ox:ro \
  --share "$RUNNER_WORK:/work:rw" \
  --env OX_SERVER="$OX_SERVER" \
  --env OX_ENVIRONMENT=seguro \
  --env OX_WORKSPACE_DIR=/work \
  --env PATH="/ox/bin:\$PATH" \
  -- /ox/bin/ox-runner

# cleanup when runner exits
rm -rf "$RUNNER_WORK"
```

To scale the pool, run this script N times. Each invocation creates
one runner. The herder drains surplus runners when the pool exceeds
the target.
