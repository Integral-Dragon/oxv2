# Wire Protocols & IPC

This document specifies the communication protocols between components:
the runtime interface (runtime ↔ ox-runner), the runner protocol
(ox-runner ↔ ox-server), and prompt assembly.

For the API endpoint specifications, see [api.md](api.md). For the
event model, see [prd/events.md](prd/events.md).

---

## Runtime Interface

The runtime interface is how a spawned process (agent or human session)
communicates with ox-runner. It provides three operations: `done`,
`artifact`, and `metric`.

### Transport

Unix domain socket. ox-runner creates the socket before spawning the
runtime and passes its path via the `OX_SOCKET` environment variable.

```
$OX_SOCKET=/tmp/ox-run-4a2f-aJuO-e1-implement-1.sock
```

The socket path encodes runner ID, execution ID, step name, and attempt
for debugging. The runtime does not need to parse it — it is an opaque
path.

ox-runner listens on the socket. The runtime connects as a client. The
connection is persistent for the lifetime of the runtime process. If
the runtime disconnects and reconnects, ox-runner accepts the new
connection (only one active connection at a time).

### Line Protocol

The protocol is newline-delimited text. Each line is a command. The
runtime writes commands; ox-runner reads them. ox-runner writes
acknowledgements; the runtime reads them.

```
→  command\n
←  ok\n
```

or

```
→  command\n
←  error: reason\n
```

All commands and responses are UTF-8 text terminated by `\n`. No
framing beyond newline delimitation.

### Commands

#### `done <output>`

Report that the step is complete. `<output>` is the output value used
for transition matching (e.g. `pass`, `fail`, `pass:7`). Everything
after the first space is the output value, including any spaces within
it.

```
→  done pass:7\n
←  ok\n
```

After `done`, the runtime should exit. ox-runner ignores further
commands after `done` except `metric` (which may arrive from cleanup
code before exit).

If the runtime exits without sending `done`, ox-runner detects
`exited_silent` and fails the step.

#### `artifact <name> <base64-data>`

Write a chunk of content to a named artifact. `<name>` is the artifact
name (must match a declared artifact in the step spec). `<base64-data>`
is base64-encoded binary content.

```
→  artifact proposal SGVsbG8gd29ybGQ=\n
←  ok\n
```

Multiple `artifact` commands for the same name append content in order.
ox-runner forwards each chunk to the ox-server artifact API.

For text content, base64 encoding is required to avoid newline
conflicts in the line protocol. The overhead is acceptable — artifact
chunks are typically KB-sized, not GB-sized.

To signal that an artifact is complete, send:

```
→  artifact-done <name>\n
←  ok\n
```

ox-runner calls the artifact close endpoint on ox-server.

#### `metric <name> <value>`

Report a metric. `<name>` is the metric name. `<value>` is a string
that ox-runner interprets based on the metric's declared type.

```
→  metric input_tokens 14523\n
←  ok\n
→  metric model sonnet\n
←  ok\n
```

For counters and gauges, `<value>` is a numeric string. For labels,
it is a string. For histograms, each `metric` call appends an
observation.

Metrics can be reported at any time during execution, including after
`done` but before exit. Undeclared metric names are accepted.

### Helper Script

Runtimes should not need to implement the socket protocol directly. A
shell helper simplifies the interface:

```bash
#!/usr/bin/env bash
# ox-rt — runtime interface helper
# Usage: ox-rt done pass
#        ox-rt metric input_tokens 14523
#        echo "content" | ox-rt artifact proposal

set -euo pipefail

cmd="$1"; shift

case "$cmd" in
  done)
    echo "done $*" | socat - UNIX-CONNECT:"$OX_SOCKET"
    ;;
  metric)
    echo "metric $*" | socat - UNIX-CONNECT:"$OX_SOCKET"
    ;;
  artifact)
    name="$1"; shift
    if [ $# -gt 0 ]; then
      data=$(echo -n "$*" | base64 -w0)
    else
      data=$(base64 -w0)
    fi
    echo "artifact $name $data" | socat - UNIX-CONNECT:"$OX_SOCKET"
    ;;
  artifact-done)
    echo "artifact-done $1" | socat - UNIX-CONNECT:"$OX_SOCKET"
    ;;
  *)
    echo "unknown command: $cmd" >&2
    exit 1
    ;;
esac
```

This is placed on `$PATH` inside the runtime environment. Agents and
scripts call `ox-rt done pass` rather than speaking the socket protocol
directly.

For a persistent connection (avoiding reconnection per command), the
helper can maintain a connection via a background `socat` process or
use a small compiled binary.

### Socket Lifecycle

1. ox-runner creates the Unix socket at `$OX_SOCKET`
2. ox-runner starts listening (accept loop)
3. ox-runner spawns the runtime process with `OX_SOCKET` in its env
4. Runtime connects to the socket
5. Runtime sends commands, receives acknowledgements
6. Runtime sends `done <output>` and exits
7. ox-runner detects process exit, closes the socket, cleans up the file

If the runtime exits without connecting (e.g. immediate crash),
ox-runner detects `exited_silent` via process exit without a prior
`done` command.

---

## Runner ↔ Server Protocol

ox-runner communicates with ox-server via HTTP API calls and the SSE
event stream. This section describes the protocol flow, not the
endpoint specs (which are in [api.md](api.md)).

### Registration

On startup, ox-runner registers with ox-server:

```
POST /api/runners/register
{ "environment": "seguro", "labels": { ... } }

→ 201 { "runner_id": "run-4a2f" }
```

The runner stores its assigned `runner_id` for all subsequent calls.

### SSE Subscription

After registration, the runner subscribes to the event stream:

```
GET /api/events/stream
Last-Event-ID: <last_seen_seq>
```

The runner filters events locally for those relevant to it:

- `step.dispatched` where `runner_id` matches → begin step execution
- `runner.drained` where `runner_id` matches → stop after current step
- `execution.cancelled` where `execution_id` matches current → abort

### Heartbeat Loop

A background task sends heartbeats at a fixed interval (default: 10s):

```
POST /api/runners/{id}/heartbeat
```

The heartbeat continues during step execution. If ox-server does not
receive a heartbeat within the grace period (default: 30s), the herder
re-dispatches the step.

### Step Execution Flow

When the runner receives `step.dispatched` via SSE:

```
1. Parse the resolved step spec from the event data
2. Provision workspace
   └─ git clone http://<ox-server>/git/ --branch <branch>
3. Place files from the resolved spec (persona content, secret files)
4. Start API proxies (if declared in spec)
5. Create Unix socket at $OX_SOCKET
6. Set env vars (runtime env, secrets, proxy overrides)
7. Spawn runtime process from resolved command
8. Capture stdout/stderr → stream as "log" artifact
    POST /api/executions/{id}/steps/{step}/artifacts/log/chunks

9. Wait for runtime exit

10. On "done <output>" received via socket:
    POST /api/executions/{id}/steps/{step}/done
    { "attempt": N, "output": "<output>" }

11. Collect signals (check workspace state)
    POST /api/executions/{id}/steps/{step}/signals
    { "attempt": N, "signals": [...] }

12. Check signal failure rules:
    - If failure signal → POST .../fail and skip to cleanup
    - Otherwise continue

13. Close streaming artifacts
    POST /api/executions/{id}/steps/{step}/artifacts/{name}/close

14. Collect non-streaming artifacts (commits, cx-diff)
    POST .../artifacts/commits/chunks + close
    POST .../artifacts/cx-diff/chunks + close

15. Collect metrics from proxy, compute derived metrics

16. Push branch
    git push origin <branch>

17. Confirm
    POST /api/executions/{id}/steps/{step}/confirm
    { "attempt": N, "metrics": { ... } }

18. Cleanup: remove workspace, socket, proxy
19. Return to idle
```

If any step in 12–19 fails (e.g. push fails), the runner calls
`POST .../fail` instead of confirm.

---

## Prompt Assembly

The `{prompt_file}` built-in variable points to a temporary file
containing the fully rendered prompt for the runtime. ox-runner
assembles this file before spawning the runtime process.

### Prompt Structure

The prompt file is assembled from multiple sources in a fixed order:

```
## Task

{task_title}

{task_body}

## Previous Step

Step: {prev_step_name}
Output: {prev_output}

## Task Context

{cx_comments}

## Instructions

{step_prompt}
```

### Source Data

**Task title and body** — fetched from the cx node via
`GET /api/state/cx` or by reading `.complex/nodes/{task_id}.json`
from the cloned workspace.

**Previous step output** — the `output` value from the preceding step
attempt, carried in the `step.dispatched` event data as `prev_output`.

**cx comments** — all comments on the cx node, ordered chronologically.
These carry inter-step communication (proposals, review feedback,
verdicts). Read from `.complex/nodes/{task_id}.json` in the workspace.

**Step prompt** — the `prompt` field from the step's runtime spec in
the workflow TOML. This is the step-specific instruction.

### Interpolation

The prompt field in the step spec supports `{name}` interpolation for
all built-in variables and runtime fields:

| Variable | Value |
|----------|-------|
| `{task_id}` | cx node ID |
| `{task_title}` | cx node title |
| `{prev_output}` | Output value from the previous step |
| `{prev_step}` | Name of the previous step |
| `{execution_id}` | Current execution ID |
| `{step}` | Current step name |
| `{attempt}` | Current attempt number |
| `{workspace}` | Workspace path |
| `{prompt_file}` | Path to the assembled prompt file |

### Sections Omitted When Empty

If there is no previous step (first step in the workflow), the
"Previous Step" section is omitted entirely. If there are no cx
comments, the "Task Context" section is omitted. If the step has no
`prompt` field, the "Instructions" section is omitted.

The prompt file always contains at least the "Task" section. An empty
prompt file is an error — it means the task has no title, which
indicates a misconfigured cx node.

### Persona Files

Persona files are not part of the prompt file. They are placed in the
workspace via the runtime definition's `[[runtime.files]]` mapping
(e.g. persona → `CLAUDE.md`). The agent reads them from the workspace
filesystem, not from the prompt.

This separation is intentional: the prompt carries task-specific
context; the persona carries agent-specific instructions. They reach
the agent through different channels.
