# ox

Ox is a distributed workflow engine for agentic code tasks. It orchestrates
multi-step workflows where each step can be executed by an AI agent (Claude,
Codex) or a shell command, with automatic retry, review loops, and merge.

## Architecture

```
ox-server          HTTP API + SQLite event log + SSE broadcast
  |
  +-- ox-herder    Subscribes to events, dispatches steps to idle runners
  |
  +-- ox-runner    Registers with server, executes steps, reports results
  +-- ox-runner    (one or more)
  +-- ox-runner
  |
  +-- ox-ctl       CLI for operators (status, exec, secrets, events)
```

All coordination happens through events. The server stores an append-only event
log in SQLite. The herder and runners connect via SSE and replay from any
sequence number on reconnect.

## Building

```bash
cargo build            # debug
cargo build --release  # release
```

Produces four binaries in `target/{debug,release}/`:
`ox-server`, `ox-herder`, `ox-runner`, `ox-ctl`.

## Project directory layout

Ox loads workflow and runtime definitions from a config search path. For a
project, create a `.ox/` directory at the repo root:

```
your-project/
  .ox/
    workflows/        # Workflow definitions (TOML)
      code-task.toml
      my-workflow.toml
    runtimes/         # Runtime definitions (TOML)
      claude.toml
      shell.toml
    personas/         # Persona files copied into step workspaces
      software-engineer.md
      reviewer.md
  src/
  ...
```

Ox searches for config in this order (first match wins):

1. `{repo}/.ox/` -- the repo the server is pointed at
2. Each directory in `$OX_HOME` (colon-separated, left to right)
3. Built-in defaults (`defaults/` in the ox source tree)

The built-in defaults ship with three runtimes (`claude`, `codex`, `shell`) and
one workflow (`code-task`).

## Running the ensemble

You need three processes: the server, one herder, and one or more runners.

### 1. Start the server

```bash
ox-server --port 4840 --repo /path/to/your-project
```

| Flag     | Default                | Description                        |
|----------|------------------------|------------------------------------|
| `--port` | `4840`                 | HTTP listen port                   |
| `--db`   | `ox.db` (in repo dir)  | SQLite database path               |
| `--repo` | current directory      | Path to the git repo being managed |

### 2. Start the herder

```bash
ox-herder --server http://localhost:4840
```

| Flag                | Default                    | Description                          |
|---------------------|----------------------------|--------------------------------------|
| `--server`          | `http://localhost:4840`    | Server URL                           |
| `--pool-target`     | `2`                        | Desired number of idle runners       |
| `--heartbeat-grace` | `30s`                      | Time before a runner is marked dead  |
| `--tick-interval`   | `5s`                       | Scheduling loop interval             |

### 3. Start runners

Start runners individually:

```bash
ox-runner --server http://localhost:4840 --workspace-dir /tmp/ox-work
```

| Flag              | Default                    | Description                     |
|-------------------|----------------------------|---------------------------------|
| `--server`        | `http://localhost:4840`    | Server URL                      |
| `--environment`   | `local`                    | Environment label               |
| `--workspace-dir` | `/tmp/ox-work`             | Base directory for step workspaces |

Or use the pool manager to start several at once:

```bash
# Start 3 runners as local processes
ox-pool start 3 --local

# Start 3 runners in seguro VMs
OX_SERVER=http://localhost:4840 ox-pool start 3

# Check status / stop
ox-pool status
ox-pool stop
```

### 4. Set secrets

Runtimes that need credentials use secrets. For Claude Code, inject your
OAuth credentials:

```bash
ox-ctl secrets set claude_credentials --value "$(cat ~/.claude/.credentials.json)"
```

For runtimes that use API keys directly:

```bash
ox-ctl secrets set openai_api_key --value sk-...
```

Secrets are referenced in runtime definitions as `{secret:name}` and injected
into step environments or written to files (e.g. credentials).

## Using ox-ctl

```bash
ox-ctl status                              # Server health and pool summary
ox-ctl workflows                           # List loaded workflows
ox-ctl runners list                        # Show registered runners

ox-ctl exec list                           # List executions
ox-ctl exec show aJuO-e1                   # Show execution detail
ox-ctl exec cancel aJuO-e1                 # Cancel an execution
ox-ctl exec logs aJuO-e1 propose           # Show step logs
ox-ctl exec logs aJuO-e1 propose -f        # Follow logs (like tail -f)
ox-ctl exec logs aJuO-e1 propose -n 50     # Last 50 lines
ox-ctl exec logs aJuO-e1 propose --attempt 2  # Specific attempt

ox-ctl events                              # Stream all events (SSE)
ox-ctl events --type step.done             # Filter by event type
ox-ctl events --since 42                   # Replay from sequence 42

ox-ctl trigger node-123                    # Evaluate triggers for a cx node

ox-ctl secrets list                        # List secret names
ox-ctl secrets delete old_key              # Delete a secret
```

Add `--json` to any command for machine-readable output.
Add `--server URL` to point at a non-default server.

## Writing workflows

Workflows are TOML files in `.ox/workflows/`. A workflow is a list of steps
with transitions between them.

```toml
[workflow]
name        = "my-workflow"
description = "A simple two-step workflow"

[[step]]
name   = "generate"
output = "diff"

[step.workspace]
git_clone = true
branch    = "{task_id}"
push      = true

[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "software-engineer"
prompt  = "Implement the feature described in the task."

[[step.transition]]
match = "pass"
goto  = "verify"

[[step.transition]]
match = "fail"
goto  = "generate"           # retry

[[step]]
name        = "verify"
output      = "verdict"
max_retries = 2

[step.runtime]
type   = "shell"
prompt = "cd {workspace} && cargo test && ox-rt done pass"

[[step.transition]]
match = "pass"
goto  = "merge"

[[step.transition]]
match = "*"
goto  = "generate"

[[step]]
name   = "merge"
action = "merge_to_main"

[step.workspace]
branch = "{task_id}"
```

### Step fields

| Field             | Description                                            |
|-------------------|--------------------------------------------------------|
| `name`            | Unique step identifier                                 |
| `output`          | What this step produces (`diff`, `verdict`, etc.)      |
| `max_retries`     | Times to retry on failure (default: 3)                 |
| `max_visits`      | Max times the workflow can enter this step              |
| `max_visits_goto` | Step to jump to when max_visits exceeded               |
| `action`          | Built-in action instead of a runtime (`merge_to_main`) |
| `on_fail`         | Step to jump to on failure                             |

### Workspace provisioning

```toml
[step.workspace]
git_clone = true          # Clone the repo into the step workspace
branch    = "{task_id}"   # Create or checkout this branch
push      = true          # Push changes after step completes
read_only = false
```

### Transitions

Transitions match on the step's output string. Matches are checked in order;
`*` is a catch-all. A prefix match uses `:` (e.g. `"fail:"` matches
`"fail:lint"`).

```toml
[[step.transition]]
match = "pass"
goto  = "next-step"
```

### Triggers

Triggers start a workflow execution when an external event arrives:

```toml
[[trigger]]
on       = "cx.task_ready"       # Event type to watch
tag      = "workflow:code-task"  # Optional tag filter
workflow = "code-task"           # Workflow to execute
```

## Writing runtimes

Runtimes define how a step process is spawned. They live in `.ox/runtimes/`.

```toml
[runtime]
name = "my-runtime"

[runtime.fields]
prompt = { type = "string", required = false, default = "" }
model  = { type = "string", required = false }

[runtime.command]
cmd = ["my-tool", "--prompt", "{prompt_file}"]
interactive_cmd = ["my-tool", "--interactive"]

[[runtime.command.optional]]
when = "model"
args = ["--model", "{model}"]

[runtime.env]
MY_API_KEY = "{secret:my_api_key}"
```

### Runtime features

**Files** -- place files before execution using path placeholders:

```toml
# Write secret content to the runner's home directory
[[runtime.files]]
content = "{secret:claude_credentials}"
to      = "{home}/.claude/.credentials.json"

# Copy a file into the workspace
[[runtime.files]]
from = "{persona}"
to   = "{workspace}/config.md"
```

Path placeholders: `{workspace}` (git work dir), `{tmp_dir}` (outside git),
`{home}` (runner HOME). Bare relative paths go to `{tmp_dir}`.

**Persona + prompt assembly** -- if a runtime declares a `persona` field
(type `file`), the persona content is loaded from `personas/` on the search
path and prepended to the step prompt. The combined content is written to
`{tmp_dir}/ox-prompt` and referenced via `{prompt_file}` in the command.

**Proxy** -- ox-runner can proxy API calls for metrics and rate limiting:

```toml
[[runtime.proxy]]
env      = "ANTHROPIC_BASE_URL"  # Env var pointing the tool at the proxy
provider = "anthropic"           # Provider type
target   = "https://api.anthropic.com"
```

**Metrics** -- collected from proxied requests:

```toml
[[runtime.metrics]]
name   = "input_tokens"
type   = "counter"               # counter, histogram, or label
source = "proxy"
```

## ox-rt: step-to-runner communication

Steps communicate back to the runner through `ox-rt`, a helper that talks over
a Unix socket (`$OX_SOCKET`, set automatically by the runner). Uses Python for
socket communication (no `socat` dependency).

```bash
ox-rt done pass                  # Complete the step with output "pass"
ox-rt done "fail:lint errors"    # Complete with a failure output
ox-rt metric input_tokens 14523  # Report a metric
echo "content" | ox-rt artifact proposal   # Stream artifact data
ox-rt artifact-done proposal     # Close an artifact stream
```

## Environment variables

| Variable          | Used by    | Description                              |
|-------------------|------------|------------------------------------------|
| `OX_SERVER`       | all        | Server URL (default: http://localhost:4840) |
| `OX_HOME`         | server     | Colon-separated config search path       |
| `OX_ENVIRONMENT`  | runner     | Environment label (default: local)       |
| `OX_WORKSPACE_DIR`| runner     | Base dir for step workspaces             |
| `OX_SOCKET`       | step procs | Unix socket for ox-rt (set by runner)    |
| `RUST_LOG`        | all        | Log level (default: info)                |

## Quick start

```bash
# Build
cargo build

# Terminal 1: server (set OX_HOME so it finds default runtimes/workflows)
OX_HOME=/path/to/oxv2/defaults ./target/debug/ox-server --repo /path/to/project

# Terminal 2: herder
OX_HOME=/path/to/oxv2/defaults ./target/debug/ox-herder

# Terminal 3: runners
./bin/ox-pool start 2 --local

# Terminal 4: set secrets and go
./target/debug/ox-ctl secrets set claude_credentials --value "$(cat ~/.claude/.credentials.json)"
./target/debug/ox-ctl status
```

The server polls `cx log` every 10 seconds. When a cx node tagged
`workflow:code-task` is surfaced to `ready`, the workflow starts
automatically. Monitor with:

```bash
./target/debug/ox-ctl events                      # stream all events
./target/debug/ox-ctl exec list                    # list executions
./target/debug/ox-ctl exec logs <id> <step> -f     # follow step logs
```
