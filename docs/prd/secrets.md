# Secrets

Secrets are named values that provide credentials, API keys, tokens, and
other sensitive configuration to runtimes and workspaces. They are
global, managed through the ox-server API, and delivered to runners at
dispatch time.

---

## Model

A secret is a named string value. Names are unique. Secrets are
event-sourced — `secret.set` and `secret.deleted` events are appended
to the event log, and the current secret state is a projection rebuilt
from those events.

Secret values are stored in the event log in SQLite. Disk encryption
handles at-rest protection — ox does not encrypt secret values itself.

---

## SSE Redaction

Secret events are **redacted** before broadcast on the SSE stream. The
event log stores the full event (including the value for `secret.set`),
but SSE subscribers receive only the secret name — never the value.

```
Event log (SQLite):
  { "type": "secret.set", "data": { "name": "anthropic_api_key", "value": "sk-ant-..." } }

SSE broadcast:
  { "type": "secret.set", "data": { "name": "anthropic_api_key" } }
```

This is the only event type where the SSE payload differs from the
stored payload. The redaction happens in the SSE broadcast path —
the event bus strips the `value` field before sending to subscribers.

The `secret.deleted` event carries only the name in both the log and
SSE — no redaction needed.

---

## Projection

ox-server maintains a `SecretsState` projection — a `HashMap<String,
String>` of name→value, rebuilt from the event log on startup.

```rust
pub struct SecretsState {
    pub secrets: HashMap<String, String>,
}
```

Applied events:
- `secret.set` — insert or update the name→value mapping
- `secret.deleted` — remove the name from the map

The projection is used internally by ox-server to resolve secret
references at step dispatch time. It is not exposed directly via any
API endpoint — `GET /api/secrets` returns names only, derived from the
projection keys.

---

## Secret References

Secrets are referenced in runtime definitions and workflow step specs
using the `{secret.NAME}` syntax. This is distinct from field
interpolation (`{name}`) and built-in variables (`{workspace}`).

```toml
[runtime.env]
ANTHROPIC_API_KEY = "{secret.anthropic_api_key}"
GITHUB_TOKEN      = "{secret.github_token}"

[[runtime.files]]
content = "{secret.ssh_private_key}"
to      = "{workspace}/.ssh/id_ed25519"
mode    = "0600"
```

The interpolation engine recognises `{secret.NAME}` references but does
not resolve them directly. Instead, it collects them and returns a list
of required secret names. Resolution happens at dispatch time on
ox-server, which reads from the `SecretsState` projection.

---

## Delivery to Runners

When ox-server builds the dispatch payload for a step, it:

1. Scans the runtime spec and workspace spec for `{secret.NAME}` refs
2. Resolves each ref against the `SecretsState` projection
3. Includes the resolved values in a `secrets` field on the dispatch
   response — a map of name→value for only the secrets this step needs
4. If a referenced secret does not exist, the dispatch fails with a
   validation error

The `step.dispatched` event in the event log records `secret_refs` (the
list of secret names) but **not** the resolved values:

```json
{
  "type": "step.dispatched",
  "data": {
    "execution_id": "aJuO-e1",
    "step": "implement",
    "attempt": 1,
    "runner_id": "run-4a2f",
    "secret_refs": ["anthropic_api_key", "github_token"],
    "runtime": { ... }
  }
}
```

The resolved values travel only in the direct HTTP response / SSE
message to the assigned runner. They are never persisted in the event
log or artifacts.

---

## Injection into Runtimes

ox-runner receives the resolved secrets in the dispatch payload and
injects them into the runtime environment via two mechanisms:

### Environment Variables

When a runtime definition's `[runtime.env]` references a secret:

```toml
[runtime.env]
ANTHROPIC_API_KEY = "{secret.anthropic_api_key}"
```

ox-runner resolves the `{secret.NAME}` template using the dispatch
payload's `secrets` map and sets the resulting environment variable on
the spawned process. The secret value is only in process memory — never
written to disk by ox-runner.

### Files

When a runtime definition's `[[runtime.files]]` uses `content` with a
secret:

```toml
[[runtime.files]]
content = "{secret.ssh_private_key}"
to      = "{workspace}/.ssh/id_ed25519"
mode    = "0600"
```

ox-runner writes the resolved content to the target path with the
specified permissions. The `content` field is an alternative to `from`
— it provides inline content rather than copying from a file on the
search path. Secret-derived files are cleaned up when the step workspace
is removed after completion.

---

## Security Invariants

- Secret values are stored in the SQLite event log (protected by disk
  encryption)
- Secret values are **never** broadcast over SSE — events are redacted
- Secret values are **never** stored in artifact content
- Secret values are **never** included in the persisted `step.dispatched`
  event data — only the list of secret names
- `GET /api/secrets` returns names only — no endpoint exposes values
- Secret-derived files are cleaned up with the workspace

---

## Events

```
secret.set       { name, value }     — stored in log; SSE redacted to { name }
secret.deleted   { name }            — same in log and SSE
```

`secret.set` — a secret has been created or updated. The value is
present in the event log but redacted from the SSE broadcast.

`secret.deleted` — a secret has been removed. Subsequent dispatches
referencing this secret will fail validation.

---

## API

See [../api.md](../api.md) for the full endpoint specification.

```
PUT    /api/secrets/{name}    — set a secret
GET    /api/secrets           — list secret names
DELETE /api/secrets/{name}    — delete a secret
```

## CLI

See [ox-ctl.md](ox-ctl.md) for the full CLI reference.

```
ox-ctl secrets set <name>       — set a secret (reads value from stdin)
ox-ctl secrets set <name> --value <value>
ox-ctl secrets list             — list secret names
ox-ctl secrets delete <name>    — delete a secret
```
