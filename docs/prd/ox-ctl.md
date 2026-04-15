# ox-ctl

ox-ctl is the operator CLI for ox-server. It is used by humans at a
terminal to inspect, manage, and control the ox system. It is a thin
wrapper around the ox-server HTTP API with consistent output formatting.

ox-ctl is not used by agents. Runtimes communicate with ox-runner
through the runtime interface (see [runtimes.md](runtimes.md)).

---

## Global Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--server <url>` | `$OX_SERVER` | ox-server base URL |
| `--json` | false | Output as JSON. All commands support this. |

`$OX_SERVER` defaults to `http://localhost:4840` if not set.

---

## Executions

### `ox-ctl exec list`

List executions.

```
ox-ctl exec list [--status <status>] [--workflow <name>] [--subject <id>]
```

| Flag | Description |
|------|-------------|
| `--status <status>` | Filter by status: `running`, `completed`, `escalated`, `cancelled` |
| `--workflow <name>` | Filter by workflow name |
| `--subject <id>` | Filter by source subject ID (e.g. a cx node id) |

Output:

```
ID          ORIGIN          WORKFLOW    STEP        STATUS     AGE
aJuO-e1     cx:aJuO         code-task   implement   running    4m
bX3k-e2     cx:bX3k         code-task   merge       completed  12m
cR9p-e1     cx:cR9p         triage      assess      running    1m
```

The `ORIGIN` column shows the source and subject of the event that
created the execution — `cx:<node_id>` for cx-sourced work,
`linear:<issue_id>` for Linear, `manual` for API calls without an
originating event, and so on.

### `ox-ctl exec show <id>`

Show the full execution as a linear sequence of step attempts. This is
the primary view of an execution — every attempt in order, with its
output, transition, metrics, and artifacts.

```
ox-ctl exec show <id>
```

Output:

```
Execution: aJuO-e1
Origin:    cx:aJuO — "Add rate limiting to the API"
Workflow:  code-task
Status:    running

#  STEP          ATTEMPT  STATUS   RUNNER     DURATION  TOKENS     OUTPUT     TRANSITION
1  propose       1        ✓ done   run-a3f2   3m 12s    14k/4k     proposed   → review-plan
2  review-plan   1        ✓ done   run-b1c4   1m 45s     8k/2k     fail       fail → propose
3  propose       2        ✓ done   run-c7e1   4m 01s    16k/5k     proposed   → review-plan
4  review-plan   2        ✓ done   run-b1c4   1m 12s     7k/2k     pass:7     pass → implement
5  implement     1        ↻ run    run-d2f3   4m 02s     —          —          —
```

Each row is a step attempt in execution order. The transition column
shows the output match and where it routed — making review loops and
retries visible.

### `ox-ctl exec show <id> <step> [attempt]`

Show detail for a specific step attempt: full metrics, signals,
artifacts, and output.

```
ox-ctl exec show aJuO-e1 propose 2
```

Output:

```
Execution: aJuO-e1
Step:      propose (attempt 2)
Runner:    run-c7e1
Status:    done
Output:    proposed
Transition: → review-plan

Metrics:
  duration       4m 01s
  input_tokens   16,230
  output_tokens  5,102
  api_calls      8
  cpu            12.4s
  memory_peak    480 MB
  commits        2
  lines_added    187
  lines_removed  43
  files_changed  6

Signals: (none)

Artifacts:
  NAME       STATUS   SIZE
  log        closed   842 KB
  commits    closed   2 commits
  cx-diff    closed   1 comment
  proposal   closed   3.4 KB
```

### `ox-ctl exec cancel <id>`

Cancel a running execution.

```
ox-ctl exec cancel <id>
```

Emits `execution.cancelled`. The currently assigned runner completes its
signal collection and abandons the step without confirming.

### `ox-ctl exec logs <id> <step>`

Show step logs (stdout/stderr from the runtime process).

```
ox-ctl exec logs <id> <step>                  # full log, latest attempt
ox-ctl exec logs <id> <step> -n 50            # last 50 lines
ox-ctl exec logs <id> <step> -f               # follow (like tail -f)
ox-ctl exec logs <id> <step> --attempt 2      # specific attempt
```

| Flag | Description |
|------|-------------|
| `-n <lines>` | Show last N lines |
| `-f`, `--follow` | Follow log output, polling every 2 seconds |
| `--attempt <n>` | Read a specific attempt (defaults to most recent) |

Logs are pushed by the runner to ox-server during execution. The
`--follow` flag polls the server for new content — it works while
the step is running or after completion.

---

## Triggers

### `ox-ctl trigger`

Synthesize a source event and evaluate triggers against it. Fires any
matching workflow. Used to manually start a workflow on a subject
that is already in the correct state, or to re-run a workflow after
manual intervention.

```
ox-ctl trigger --source <name> --kind <kind> --subject <id> [--force]
```

| Flag | Description |
|------|-------------|
| `--source <name>` | Watcher namespace (e.g. `cx`, `linear`, `github`) |
| `--kind <kind>` | Source-native event kind (e.g. `node.ready`, `issue.labeled`) |
| `--subject <id>` | Source-native correlation key — the `subject_id` the event is about |
| `--force` | Fire even if a matching execution is already live (bypass dedup) |

A convenience alias `ox-ctl trigger cx-node <node_id>` expands to
`--source cx --kind node.ready --subject <node_id>` for the common
cx case.

---

## Offices

### `ox-ctl runners list`

List registered runners.

```
ox-ctl runners list
```

Output:

```
ID          ENVIRONMENT   STATUS      STEP           AGE
run-4a2f    seguro        executing   aJuO-e1/implement   4m
run-7b3c    seguro        idle        —              12m
run-2d9e    gcp           executing   cR9p-e1/assess      1m
```

### `ox-ctl runners drain <id>`

Instruct an runner to stop accepting new assignments and exit after its
current step completes.

```
ox-ctl runners drain <id>
```

Emits `runner.drained`. Used for manual pool management or graceful
shutdown of a specific runner.

---

## Artifacts

### `ox-ctl artifacts list <execution-id> <step>`

List artifacts for a step.

```
ox-ctl artifacts list <execution-id> <step>
```

Output:

```
NAME          SOURCE        STATUS     SIZE
log           log           closed     1.2 MB
commits       git-commits   closed     3 commits
cx-diff       cx-diff       closed     1 comment
proposal      file          closed     3.4 KB
```

### `ox-ctl artifacts show <execution-id> <step> <name>`

Print the full content of a closed artifact.

```
ox-ctl artifacts show <execution-id> <step> <name>
```

### `ox-ctl artifacts tail <execution-id> <step> <name>`

Stream a live artifact. Blocks and prints chunks as they arrive.
Exits when the artifact closes.

```
ox-ctl artifacts tail <execution-id> <step> <name>
```

Useful for watching an agent log in real-time:

```
ox-ctl artifacts tail aJuO-e1 implement log
```

---

## Events

### `ox-ctl events`

Tail the ox event stream. Prints events as they arrive.

```
ox-ctl events [--since <seq>] [--type <prefix>]
```

| Flag | Description |
|------|-------------|
| `--since <seq>` | Start from this sequence number |
| `--type <prefix>` | Filter to events whose type matches this prefix (e.g. `step.`, `source`) |

Output (default):

```
42  2026-04-04T12:01:03Z  step.dispatched    aJuO-e1/implement → run-4a2f
43  2026-04-04T12:01:04Z  artifact.declared  aJuO-e1/implement/log
44  2026-04-04T12:05:15Z  step.done          aJuO-e1/implement  output=diff
45  2026-04-04T12:05:18Z  step.signals       aJuO-e1/implement  []
46  2026-04-04T12:05:22Z  step.confirmed     aJuO-e1/implement
47  2026-04-04T12:05:22Z  step.advanced      aJuO-e1  implement→review-code
```

With `--json`, each line is the raw event envelope.

---

## Workflows

### `ox-ctl workflows list`

List loaded workflow definitions.

```
ox-ctl workflows list
```

Output:

```
NAME                  STEPS   DESCRIPTION
code-task             7       Propose → review plan → implement → review code → merge
triage                3       Diagnose failure, re-dispatch or descope
phase-review          2       Review completed phase, merge branches, integrate
pm-discovery          2       Discover objectives from plan
task-decomposition    3       Decompose objectives into phases and tasks
```

---

## Secrets

### `ox-ctl secrets list`

List secret names. Never shows values.

```
ox-ctl secrets list
```

Output:

```
NAME
anthropic_api_key
github_token
ssh_private_key
```

### `ox-ctl secrets set <name>`

Set a secret. Reads value from stdin (for piping) or `--value` flag.

```
ox-ctl secrets set anthropic_api_key --value sk-ant-api03-...
echo "sk-ant-api03-..." | ox-ctl secrets set anthropic_api_key
```

### `ox-ctl secrets delete <name>`

Delete a secret.

```
ox-ctl secrets delete anthropic_api_key
```

---

## Status

### `ox-ctl status`

Show server health, pool size, active execution count, and the state
of each configured watcher.

```
ox-ctl status
```

Output:

```
ox-server   healthy   uptime 2d 4h
pool        3 runners (2 executing, 1 idle)
executions  2 running, 0 escalated
workflows   8 loaded

watchers
  SOURCE   LAST INGEST           CURSOR            STATUS
  cx       2026-04-15T12:00:03Z  d59b010abc12…     alive
```

The watchers section is rendered from `GET /api/watchers`. Each row
is one entry in the `watcher_cursors` table:

- **SOURCE** — watcher identifier.
- **LAST INGEST** — timestamp of the last successful ingest call
  (including empty-batch liveness pings).
- **CURSOR** — the opaque cursor string the watcher last committed,
  truncated for display. The cursor is a git sha for cx, a delivery
  id for a webhook watcher, or a timestamp for a polling API watcher.
- **STATUS** — `alive` if the last call committed cleanly, otherwise
  the short error from `last_error`.

If `last_error` is set the status column turns into the error text —
for example, `cas:expected X got Y` when two concurrent watchers
fight over the same source row.

---

## Config

### `ox-ctl reload`

Reload configuration from disk. Reads all workflow, runtime, persona,
and trigger files from the search path, validates them, and swaps the
live config atomically. If validation fails, the old config is kept.

```
ox-ctl reload
```

Output:

```
Config reloaded:
  workflows: 5
  runtimes:  3
  personas:  3
  triggers:  1
```

On failure, prints validation errors and exits non-zero.

### `ox-ctl config check`

Validate configuration files without applying. Reports errors and
shows what would change compared to the currently loaded config.

```
ox-ctl config check
```

Output:

```
Config valid.
  personas: +1 -0
```

On validation failure:

```
Config invalid:
  persona 'test/eng': sets var 'modle' which runtime 'claude' does not declare
```

---

## Output Format

All commands default to human-readable tabular output. Pass `--json` for
machine-readable output. With `--json`, every command prints a JSON object
or array to stdout.

```sh
ox-ctl exec show aJuO-e1 --json | jq -r '.status'
```

Exit codes follow unix conventions: `0` on success, non-zero on error.
Error messages are written to stderr; structured output is always on
stdout.
