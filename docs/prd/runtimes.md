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

A runtime definition declares what fields it accepts, how to construct
the command line, what files to place in the workspace, and what
environment variables to set. All values support string interpolation
of declared fields.

```toml
# .ox/runtimes/claude.toml
[runtime]
name = "claude"

[runtime.fields]
model   = { type = "string", required = false }
persona = { type = "file",   required = false }
prompt  = { type = "string", required = false, default = "" }

[runtime.command]
cmd = ["claude", "-p", "{prompt}"]
interactive_cmd = ["claude"]

[[runtime.command.optional]]
when = "model"
args = ["--model", "{model}"]

[[runtime.files]]
from = "{persona}"
to   = "{workspace}/CLAUDE.md"

[runtime.env]
CLAUDE_MODEL = "{model}"

[[runtime.proxy]]
env      = "ANTHROPIC_BASE_URL"
provider = "anthropic"
target   = "https://api.anthropic.com"

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

[[runtime.metrics]]
name   = "model"
type   = "label"
source = "proxy"
```

```toml
# .ox/runtimes/codex.toml
[runtime]
name = "codex"

[runtime.fields]
model   = { type = "string", required = false }
persona = { type = "file",   required = false }
prompt  = { type = "string", required = false, default = "" }

[runtime.command]
cmd = ["codex", "{prompt}"]

[[runtime.command.optional]]
when = "model"
args = ["--model", "{model}"]

[[runtime.files]]
from = "{persona}"
to   = "{workspace}/.codex/system-prompt.md"

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

[runtime.fields]
prompt = { type = "string", required = false, default = "" }

[runtime.command]
cmd = ["openclaw", "--task", "{prompt_file}"]
```

---

## Interpolation

Any string value in a runtime definition can reference declared fields
and built-in variables using `{name}` syntax. Interpolation applies to
command templates, file mappings, and environment variables.

### Built-in Variables

These are always available, provided by ox-runner:

| Variable | Value |
|----------|-------|
| `{workspace}` | Path to the provisioned workspace |
| `{prompt_file}` | Path to a temp file containing the rendered prompt |

### Secret References

Secrets are referenced using `{secret:NAME}` syntax. This is distinct
from field interpolation (`{name}`) and built-in variables.

```toml
[runtime.env]
ANTHROPIC_API_KEY = "{secret:anthropic_api_key}"
GITHUB_TOKEN      = "{secret:github_token}"
```

Secret references are resolved at dispatch time by ox-server — not by
the interpolation engine directly. The interpolation engine recognises
`{secret:NAME}` patterns and collects the referenced names. ox-server
resolves them from its `SecretsState` projection and includes the
resolved values in the dispatch payload sent to the runner.

See [secrets.md](secrets.md) for the full secrets model.

### Field Variables

Any declared field can be referenced by name. If a field has
`type = "file"`, its value is resolved to an absolute path.

| Type | Interpolation value |
|------|---------------------|
| `string` | The field value as-is |
| `file` | Absolute path to the resolved file |
| `bool` | `"true"` or `"false"` |
| `int` | String representation of the integer |

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

**`runtime.fields`** — declares what parameters the runtime accepts.
Each field has a `type` (`string`, `file`, `bool`, `int`), optionality
(`required`), and an optional `default`. When a workflow step passes a
field not declared by the runtime definition, it is an error.

### Command

**`runtime.command.cmd`** — the base command template. An array of
strings with interpolation.

**`runtime.command.interactive_cmd`** — optional alternate command used
when `tty = true`. If absent, `cmd` is used for both modes.

**`runtime.command.optional`** — conditional argument blocks. Each entry
has a `when` field (the field name to check) and `args` (an array of
strings with interpolation). When the field has a value, `args` are
appended to the command. When absent, the block is skipped.

```toml
[[runtime.command.optional]]
when = "model"
args = ["--model", "{model}"]

[[runtime.command.optional]]
when = "verbose"
args = ["--verbose"]
```

### Files

**`runtime.files`** — file placement rules. Each entry copies a file
into the workspace before the runtime starts.

```toml
[[runtime.files]]
from = "{persona}"
to   = "{workspace}/CLAUDE.md"
```

`from` is resolved via the configuration search path for `file` type fields.
`to` is typically relative to `{workspace}`. Both support interpolation.
If `from` references an absent optional field, the mapping is skipped.

Alternatively, `content` provides inline content instead of copying
from a file. This is used with secrets to write credential files:

```toml
[[runtime.files]]
content = "{secret:ssh_private_key}"
to      = "{workspace}/.ssh/id_ed25519"
mode    = "0600"
```

A file entry has either `from` or `content`, not both. `mode` is an
optional POSIX permission string (default: `"0644"`).

This is how persona loading works — it is not special-cased by
ox-runner. A runtime that needs a persona file placed in the workspace
declares a `file` field and a file mapping. A runtime that doesn't use
personas simply doesn't declare one.

### Environment

**`runtime.env`** — environment variables set on the runtime process.
Keys are variable names; values are strings with interpolation.

```toml
[runtime.env]
CLAUDE_MODEL = "{model}"
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
| *(any declared field)* | — | Fields defined by the runtime definition (e.g. `model`, `persona`, `prompt`) |

`type`, `tty`, `env`, and `timeout` are handled by ox-runner directly.
All other fields are passed to the runtime definition for interpolation.
A field not declared by the definition is an error.

```toml
[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "inspired/software-engineer"
prompt  = "Implement the task following the approved proposal."
```

There are no "common" or "special" fields beyond `type`, `tty`, `env`,
and `timeout`. Whether a runtime uses `persona`, `prompt`, `model`, or
any other field is entirely up to the runtime definition.

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
2. Validates the step's fields against the definition
3. Resolves all `{name}` interpolations and collects `{secret:NAME}` refs
4. Reads file content for `runtime.files` mappings (persona files, etc.)
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

1. Places files from the spec (persona content, secret-derived files)
2. Starts API proxies declared in the spec
3. Sets environment variables (runtime env, secret env, proxy overrides)
4. Spawns the process from the resolved command
5. Attaches stdout/stderr to the `log` artifact
6. On exit: reads proxy metrics, computes derived metrics, stops proxies

The runtime communicates with ox-runner through the runtime interface —
it reports completion (`done` with an output value), writes artifact
content, and reports metrics. If the runtime exits without calling
`done`, the runner detects `exited_silent` and fails the step.

If `tty = true`, ox-runner allocates a PTY and uses the interactive
command. The runtime interface works identically inside interactive
sessions. Signals, artifacts, and two-phase confirmation are the same
regardless of whether a TTY is present.

ox-runner has no concept of "human step" vs "agent step" — only whether
a TTY is needed. The distinction is purely in the runtime definition.
