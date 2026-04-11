# Runtimes

A runtime defines what process ox-runner spawns to execute a workflow
step. Runtime definitions are TOML files found via the configuration
search path (see [README.md](README.md#configuration-search)) under
`runtimes/`. ox-runner has no hardcoded knowledge of any agent CLI —
`claude`, `codex`, and any other runtime are all defined the same way,
as configuration.

Adding a new runtime means adding a TOML file to any directory on the
search path.

---

## Runtime Definitions

A runtime definition declares its vars, how to construct the command
line, what files to place, and what environment variables to set. All
values support interpolation.

```toml
# .ox/runtimes/claude.toml
[runtime]
name = "claude"

[runtime.vars]
model  = { type = "string" }
prompt = { type = "string", default = "" }

[runtime.command]
cmd = [
    "claude",
    "-p", "{tmp_dir}/ox-prompt",
    "--dangerously-skip-permissions",
    "--verbose",
    "--output-format", "stream-json",
]
interactive_cmd = ["claude"]

[[runtime.command.optional]]
when = "model"
args = ["--model", "{var.model}"]

# Assemble persona + prompt into a single prompt file.
# {workflow.persona} is a file-typed workflow var — resolved to content at dispatch.
[[runtime.files]]
content = "{workflow.persona}\n\n---\n\n{var.prompt}"
to      = "{tmp_dir}/ox-prompt"

# Inject OAuth credentials from secrets
[[runtime.files]]
content = "{secret.claude_credentials}"
to      = "{home}/.claude/.credentials.json"
```

Claude Code manages its own authentication via OAuth credentials
stored in `~/.claude/.credentials.json`. The credentials JSON is
injected as a secret — no API key environment variable or proxy
needed:

```bash
ox-ctl secrets set claude_credentials --value "$(cat ~/.claude/.credentials.json)"
```

```toml
# .ox/runtimes/codex.toml
[runtime]
name = "codex"

[runtime.vars]
model  = { type = "string" }
prompt = { type = "string", default = "" }

[runtime.command]
cmd = ["codex", "{tmp_dir}/ox-prompt"]

[[runtime.command.optional]]
when = "model"
args = ["--model", "{var.model}"]

# Assemble persona + prompt
[[runtime.files]]
content = "{workflow.persona}\n\n---\n\n{var.prompt}"
to      = "{tmp_dir}/ox-prompt"

# Place persona as system prompt for codex
[[runtime.files]]
content = "{workflow.persona}"
to      = "{workspace}/.codex/system-prompt.md"

[[runtime.proxy]]
env      = "OPENAI_BASE_URL"
provider = "openai"
target   = "https://api.openai.com"

[[runtime.metrics]]
name   = "input_tokens"
type   = "counter"
source = "proxy"

[[runtime.metrics]]
name   = "output_tokens"
type   = "counter"
source = "proxy"

[[runtime.metrics]]
name   = "api_calls"
type   = "counter"
source = "proxy"

[[runtime.metrics]]
name   = "response_latency_ms"
type   = "histogram"
source = "proxy"
```

```toml
# .ox/runtimes/openclaw.toml
[runtime]
name = "openclaw"

[runtime.vars]
prompt = { type = "string", default = "" }

[runtime.command]
cmd = ["openclaw", "--task", "{tmp_dir}/ox-prompt"]

[[runtime.files]]
content = "{var.prompt}"
to      = "{tmp_dir}/ox-prompt"
```

---

## Interpolation

All string values in a runtime definition support `{name}` interpolation.
There are four namespaces, distinguished by prefix:

| Pattern | Source |
|---------|--------|
| `{var.name}` | Runtime vars (e.g. `{var.prompt}`, `{var.model}`) |
| `{workflow.name}` | Workflow/execution vars (e.g. `{workflow.persona}`, `{workflow.task_id}`) |
| `{secret.name}` | Secrets (e.g. `{secret.claude_credentials}`) |
| `{name}` | Builtins: `{workspace}`, `{tmp_dir}`, `{home}` |

Interpolation applies to command templates, file content, file paths,
and environment variable values.

### Secret References

Secrets are referenced using `{secret.name}` syntax.

```toml
[runtime.env]
ANTHROPIC_API_KEY = "{secret.anthropic_api_key}"
GITHUB_TOKEN      = "{secret.github_token}"
```

Secret references are resolved at dispatch time by ox-server. The
interpolation engine collects `{secret.name}` references; ox-server
resolves them from its `SecretsState` projection and includes the
values in the dispatch payload sent to the runner.

See [secrets.md](secrets.md) for the full secrets model.

### Runtime Variables

Runtime vars declared in `[runtime.vars]` are referenced with the
`var.` prefix:

```toml
[runtime.vars]
model  = { type = "string" }
prompt = { type = "string", default = "" }

[runtime.command]
cmd = ["my-tool", "-p", "{tmp_dir}/ox-prompt"]

[[runtime.command.optional]]
when = "model"
args = ["--model", "{var.model}"]
```

### Workflow Variables

Workflow vars (from the execution's `vars` map) are available with
the `workflow.` prefix. File-typed workflow vars are resolved to
content at dispatch time:

```toml
# In runtime files — reference workflow vars
[[runtime.files]]
content = "{workflow.persona}\n\n---\n\n{var.prompt}"
to      = "{tmp_dir}/ox-prompt"
```

### Absent Fields

When an optional field has no value:

- **`cmd`** — absent fields interpolate to empty string. Use
  `optional` for args that should be omitted entirely.
- **`optional`** — the entire args block is skipped when the `when`
  field is absent.
- **`files`** — the file mapping is skipped when `from` or `to`
  references an absent field.
- **`env`** — the variable is not set when any referenced field is
  absent.

This means optional configuration is cleanly omitted rather than set
to empty values.

---

## Definition Fields

**`runtime.name`** — the name referenced by `type` in workflow steps.

**`runtime.vars`** — declares what parameters the runtime accepts.
Each var has a `type` (`string`, `bool`, `int`), optionality
(`required`), and an optional `default`. Runtime vars are referenced
with the `{var.name}` prefix in templates.

### Command

**`runtime.command.cmd`** — the base command template. An array of
strings with interpolation.

**`runtime.command.interactive_cmd`** — optional alternate command used
when `tty = true`. If absent, `cmd` is used for both modes.

**`runtime.command.optional`** — conditional argument blocks. Each entry
has a `when` field (the runtime var name to check) and `args` (an array
of strings with interpolation). When the var has a value, `args` are
appended to the command. When absent, the block is skipped.

```toml
[[runtime.command.optional]]
when = "model"
args = ["--model", "{var.model}"]

[[runtime.command.optional]]
when = "verbose"
args = ["--verbose"]
```

### Files

**`runtime.files`** — file placement rules. Each entry writes a file
before the runtime starts. Files are how runtimes assemble prompts,
inject credentials, and place configuration.

A file entry has either `from` (copy from search path) or `content`
(inline content with interpolation), not both. `mode` is an optional
POSIX permission string (default: `"0644"`).

Path placeholders resolved by the runner:

| Placeholder | Resolves to |
|-------------|-------------|
| `{workspace}` | The step workspace (git work_dir) |
| `{tmp_dir}` | Runner's temp directory (outside git, cleaned after step) |
| `{home}` | Runner's HOME directory |

Bare relative paths (no placeholder prefix) are placed in `{tmp_dir}`.

```toml
# Assemble persona + prompt into a prompt file
[[runtime.files]]
content = "{workflow.persona}\n\n---\n\n{var.prompt}"
to      = "{tmp_dir}/ox-prompt"

# Write secret content to the runner's home directory
[[runtime.files]]
content = "{secret.claude_credentials}"
to      = "{home}/.claude/.credentials.json"

# Write a secret key with restricted permissions
[[runtime.files]]
content = "{secret.ssh_private_key}"
to      = "{home}/.ssh/id_ed25519"
mode    = "0600"
```

### Environment

**`runtime.env`** — environment variables set on the runtime process.
Keys are variable names; values are strings with interpolation.

```toml
[runtime.env]
CLAUDE_MODEL = "{var.model}"
MY_CONFIG    = "{workspace}/.config"
```

Variables referencing absent optional fields are not set. These are
merged with `env` from the step spec — step-level env takes precedence.

### Proxy

**`runtime.proxy`** — API proxy declarations. Each entry tells ox-runner
to start a local proxy that intercepts API traffic from the runtime
process. The proxy extracts metrics (tokens, latency, model) from
request/response payloads.

```toml
[[runtime.proxy]]
env      = "ANTHROPIC_BASE_URL"
provider = "anthropic"
target   = "https://api.anthropic.com"
```

| Field | Required | Description |
|-------|----------|-------------|
| `env` | yes | Environment variable to override with the proxy address |
| `provider` | yes | API provider format — tells the proxy how to parse responses |
| `target` | yes | Upstream URL the proxy forwards requests to |

ox-runner starts the proxy before spawning the runtime, sets the env
var to point to it, and reads accumulated metrics from the proxy after
the runtime exits. The runtime process makes API calls normally — it
does not know it is being proxied.

A runtime can declare multiple proxies (e.g. a runtime that calls both
Anthropic and OpenAI APIs). Each gets its own local listener and env
var override.

Supported providers are built into ox-runner. Each provider knows how
to extract token counts, model identifiers, and latency from that API's
response format. The set is small and changes rarely:

| Provider | Extracts |
|----------|----------|
| `anthropic` | input_tokens, output_tokens, model, latency |
| `openai` | prompt_tokens, completion_tokens, model, latency |

Runtimes with no API calls omit the `proxy` section entirely.

### Metrics

**`runtime.metrics`** — declares metrics the runtime will produce.
See [metrics.md](metrics.md) for the full metrics model.

```toml
[[runtime.metrics]]
name        = "input_tokens"
type        = "counter"
source      = "proxy"
description = "Total input tokens consumed"
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Metric name |
| `type` | yes | Metric type: `gauge`, `counter`, `histogram`, `label` |
| `source` | no | Where the metric comes from: `proxy` or `runtime` (default) |
| `description` | no | Human-readable description for schema discovery |

**`source = "proxy"`** — the metric is collected by the API proxy.
ox-runner maps provider-specific fields to the declared metric names.
The runtime process does not need to report these — the proxy does it
on its behalf.

**`source = "runtime"`** (default) — the runtime process reports the
metric via the runtime interface. The runtime calls `metric <name>
<value>` during execution.

A runtime definition can mix sources — some metrics from the proxy,
others reported directly by the runtime. Undeclared metrics reported
via the interface are accepted and stored but have no type or
description metadata.

---

## Step Runtime Fields

The step's `[step.runtime]` block passes parameters to the runtime
definition. See [workflows.md](workflows.md) for how steps are defined.

| Field | Required | Description |
|-------|----------|-------------|
| `type` | yes | Name of the runtime definition |
| `tty` | no | Allocate a TTY for the process |
| `env` | no | Extra environment variables passed to the process |
| `timeout` | no | Maximum wall-clock time before the runner kills the process |
| *(any declared var)* | — | Vars defined by the runtime (e.g. `model`, `prompt`) |
| *(any workflow var)* | — | Overrides for workflow vars (e.g. `persona` per step) |

`type`, `tty`, `env`, and `timeout` are handled by ox-runner directly.
Runtime vars are passed to the runtime definition for interpolation.
Workflow var overrides (like `persona`) are resolved at dispatch time
before reaching the runtime.

```toml
[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "inspired/software-engineer"
prompt  = "Implement the task following the approved proposal."
```

---

## Runtime Interface

ox-runner exposes a local interface to the spawned process. This is
the only way a runtime communicates with the ox system — it never
talks to ox-server directly. ox-runner mediates all communication
with ox-server on the runtime's behalf.

The runtime interface has three capabilities:

**done** — report that the step is complete, with an output value.
The output value is used for transition matching (e.g. `pass`, `fail`,
`pass:7`). ox-runner forwards this to ox-server as `step.done`.

**artifact** — write content to a named artifact. The runtime streams
chunks of content to a declared artifact name. ox-runner forwards
these to the ox-server artifact API. This is how a runtime produces
streaming artifacts beyond the implicit `log` (stdout/stderr).

**metric** — report a named metric with a value. Used for tokens,
API calls, latency, and any other runtime-observable measurement.
See [metrics.md](metrics.md) for metric types and the full metrics
model.

The runtime does not need to know about ox-server, executions,
workflows, or the event stream. Its world is: a workspace, its
configured files, and these three operations.

Agents call `cx` directly on their branch for issue graph operations
— this is expected and correct. cx is a local file tool and does not
go through the runtime interface.

---

## Runtime Resolution

Runtime definitions are resolved by **ox-server**, not by ox-runner.
When the herder dispatches a step, ox-server:

1. Finds the runtime definition via the configuration search path
2. Merges step runtime field overrides with workflow vars
3. Resolves file-typed workflow vars to content from the search path
4. Resolves all `{var.name}`, `{workflow.name}`, `{secret.name}` interpolations
5. Resolves secrets from the `SecretsState` projection
6. Builds the command from `cmd` (or `interactive_cmd`) plus `optional` blocks
7. Assembles the complete resolved step spec

The resolved spec is included in the `step.dispatched` event data (with
secret values excluded from the persisted event — see
[secrets.md](secrets.md)). The runner receives everything it needs to
execute the step without access to runtime definitions or the
configuration search path.

## Runner Execution

ox-runner receives a fully-resolved step spec and executes it:

1. Places files from the spec, resolving `{workspace}`, `{tmp_dir}`,
   `{home}` placeholders to actual paths
2. Starts API proxies declared in the spec
3. Resolves placeholders in command args (e.g. `{tmp_dir}/ox-prompt`)
4. Sets environment variables (runtime env, secret env, proxy overrides)
5. Spawns the process with stdin null, stdout/stderr to a log file
6. Ships log chunks to ox-server every 5 seconds via
   `POST /api/executions/{id}/steps/{step}/log/chunk`
7. On exit: final log flush, reads proxy metrics, stops proxies

### Completion signaling

The runtime communicates with ox-runner through the runtime interface
(ox-rt / unix socket). It reports completion with `ox-rt done <output>`,
where the output value is used for transition matching (e.g. `pass`,
`fail`, `pass:7`).

If the runtime exits with code 0 without calling `ox-rt done`, the
runner infers `done ""` (empty output). The workflow engine will
advance to the next step by declaration order since no transition
pattern matches the empty string. This allows runtimes that don't
know about ox-rt to still complete successfully.

If the runtime exits with a non-zero code without calling `ox-rt done`,
the `exited_silent` signal fires and the step fails.

### Log viewing

Step logs are stored on the server and viewable via:

```bash
ox-ctl exec logs <execution-id> <step>           # full log
ox-ctl exec logs <execution-id> <step> -n 50     # last 50 lines
ox-ctl exec logs <execution-id> <step> -f         # follow (like tail -f)
ox-ctl exec logs <execution-id> <step> --attempt 2  # specific attempt
```

### TTY mode

If `tty = true`, ox-runner allocates a PTY (via `openpty`) and spawns
the interactive command attached to it. A TCP bridge listens on a
random port and the address is reported in the `step.running` event's
`connect_addr` field.

Clients connect via TCP:

```
socat - TCP:<runner-host>:<port>
```

PTY output is teed to the step log file so the log pusher works
identically to standard mode. The unix socket for `ox-rt done` /
metrics / artifacts is also available — the process can signal
completion the same way an agent would.

ox-runner has no concept of "human step" vs "agent step" — only whether
a TTY is needed. The distinction is purely in the runtime definition.
