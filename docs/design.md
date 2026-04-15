# Design

This document describes how ox is built. It covers crate structure, key
types, dependency relationships, configuration loading, and cross-cutting
concerns. Subsystem details are in companion documents:

- [storage.md](storage.md) — event log persistence, SQLite schema, projections
- [api.md](api.md) — HTTP API, SSE, git smart HTTP, artifact endpoints
- [protocols.md](protocols.md) — runtime interface, runner↔server protocol, prompt assembly
- [workflows-design.md](workflows-design.md) — workflow engine, triggers, transitions, merge
- [event-sources-design.md](event-sources-design.md) — watcher plugins, ingest endpoint, `SourceEvent`

For what ox does (as opposed to how), see [prd/README.md](prd/README.md).

---

## Crate Layout

Ox is a Cargo workspace with five binary crates and one library crate.

```
ox/
  Cargo.toml          workspace root
  ox-core/            shared library
  ox-server/          HTTP server + event log + git endpoint
  ox-herder/          event-driven orchestration loop
  ox-runner/          step executor
  ox-ctl/             operator CLI
  ox-cx-watcher/      cx source watcher (reference event source)
```

Watchers live in their own crates — one per source system. Only the
cx watcher exists today. A deployment that uses Linear or GitHub
instead of cx would install `ox-linear-watcher` or `ox-github-watcher`
alongside the server; ox-server itself has no knowledge of any
specific source system.

### ox-core

Shared types, event definitions, configuration parsing, and the API
client. Every other crate depends on ox-core. It contains no I/O beyond
file reads for configuration loading.

Modules:

| Module | Contents |
|--------|----------|
| `types` | `ExecutionId`, `StepAttempt`, `RunnerId`, `Seq`, newtypes |
| `events` | `EventEnvelope`, all event type enums, event data structs |
| `config` | Search path resolution, TOML parsing for workflows/runtimes/personas |
| `workflow` | `WorkflowDef`, `StepDef`, `TransitionDef`, `WorkspaceDef` |
| `runtime` | `RuntimeDef`, `CommandDef`, `ProxyDef`, `MetricDef` |
| `interpolation` | `{name}` and `{secret.name}` template engine for runtime command/env/file rendering |
| `client` | HTTP client for ox-server API (used by ox-herder, ox-runner, ox-ctl) |

### ox-server

The hub. Accepts API requests, appends events to the log, maintains
projections, serves SSE, hosts the git endpoint, and stores artifacts.
Passive — it never initiates action.

Modules:

| Module | Contents |
|--------|----------|
| `main` | CLI arg parsing, server startup, graceful shutdown |
| `db` | SQLite connection pool, event log append/read, schema migrations |
| `events` | Event bus — append, broadcast to SSE subscribers, replay |
| `projections` | In-memory state rebuilt from the event log (pool, executions, cx) |
| `pool` | Runner pool management — registration, heartbeats, drain, staleness/mismatch detection |
| `api` | Axum router, REST handlers, request/response types |
| `sse` | SSE endpoint, subscriber management, `Last-Event-ID` resume, secret redaction |
| `secrets` | Secrets projection, CRUD API handlers |
| `git` | Git smart HTTP protocol handlers (`/git/*`) |
| `artifacts` | Artifact storage, chunk writes, fetch, streaming reads |
| `pty_relay` | WebSocket relay for interactive PTY sessions (bridges runner ↔ client) |
| `merge` | `merge_to_main` implementation, cx diff extraction |
| `ingest` | Watcher batch ingest handler, cursor CAS, idempotency dedup |
| `cx` | cx state projection, diff parsing, cx event derivation *(removed in the event-sources migration — relocates to `ox-cx-watcher`)* |

### ox-herder

The active loop. Subscribes to the ox-server SSE stream and reacts to
events. Makes decisions and acts by calling ox-server API endpoints.
Never mutates state directly.

Modules:

| Module | Contents |
|--------|----------|
| `main` | CLI arg parsing, SSE subscription, tick loop startup |
| `triggers` | Trigger evaluation, dedup tracking, execution creation |
| `dispatch` | Idle runner selection, step dispatch |
| `advance` | Transition matching, step advancement after confirmation |
| `pool` | Pool size monitoring, surplus runner drain decisions |
| `liveness` | Heartbeat staleness detection, re-dispatch |
| `tick` | Periodic checks that cannot be expressed as event reactions |

### ox-runner

The executor. Registers with ox-server, receives step assignments via
SSE, executes them, and reports results.

Modules:

| Module | Contents |
|--------|----------|
| `main` | CLI arg parsing, registration, SSE subscription, idle loop |
| `workspace` | Git clone, branch checkout, workspace provisioning |
| `runtime` | Resolved step spec execution — file placement, env assembly, command spawning |
| `pty` | PTY allocation (openpty), process spawning, websocket relay to server for interactive sessions |
| `socket` | Unix domain socket server for the runtime interface |
| `proxy` | API proxy — local listener, request/response interception, metric extraction |
| `signals` | Post-exit signal collection (no_commits, dirty_workspace, etc.) |
| `artifacts` | Implicit artifact collection (commits, cx-diff), declared artifact forwarding |
| `confirm` | Branch push, confirm API call, two-phase completion |

### ox-ctl

Thin CLI wrapper around the ox-core API client.

Modules:

| Module | Contents |
|--------|----------|
| `main` | Clap command tree, global flags |
| `exec` | `exec list`, `exec show`, `exec cancel` |
| `runners` | `runners list`, `runners drain` |
| `artifacts` | `artifacts list`, `artifacts show`, `artifacts tail` |
| `events` | `events` (tail SSE stream) |
| `workflows` | `workflows list` |
| `status` | `status` (server health) |
| `secrets` | `secrets list`, `secrets set`, `secrets delete` |
| `trigger` | `trigger <node-id>` |
| `output` | Table formatter, JSON output mode |

---

## Dependency Graph

```
ox-ctl ──→ ox-core
ox-herder ──→ ox-core
ox-runner ──→ ox-core
ox-server ──→ ox-core
```

All binary crates depend on ox-core. No binary crate depends on another
binary crate. ox-core depends on no ox crate.

### External Dependencies

| Crate | Used by | Purpose |
|-------|---------|---------|
| `axum` | ox-server | HTTP framework |
| `rusqlite` | ox-server | SQLite |
| `tokio` | all | Async runtime |
| `reqwest` | ox-core (client) | HTTP client |
| `clap` | ox-server, ox-herder, ox-runner, ox-ctl | CLI parsing |
| `serde` / `serde_json` | all | Serialization |
| `toml` | ox-core | Config parsing |
| `git2` | ox-server, ox-runner | Git operations |
| `tokio-tungstenite` | ox-runner, ox-ctl | WebSocket client for PTY relay |
| `libc` | ox-runner | PTY allocation (openpty/fork), process control |
| `hyper` | ox-runner (proxy) | Low-level HTTP for API proxy |
| `tracing` | all | Structured logging |

---

## Key Types

All defined in ox-core. Shown as Rust structs for precision.

### Identifiers

```rust
/// Monotonically increasing event sequence number.
pub struct Seq(pub u64);

/// Runner identifier. Assigned by ox-server on registration.
/// Format: "run-{4hex}" e.g. "run-4a2f"
pub struct RunnerId(pub String);

/// Execution identifier. Server-generated: "e-{epoch}-{seq}"
pub struct ExecutionId(pub String);

/// Addresses a specific step attempt within an execution.
/// Format: "{execution_id}/{step_name}/{attempt}"
/// e.g. "aJuO-e1/propose/2"
pub struct StepAttemptId {
    pub execution_id: ExecutionId,
    pub step: String,
    pub attempt: u32,
}
```

### Events

```rust
pub struct EventEnvelope {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub event_type: EventType,
    pub data: serde_json::Value,
}

pub enum EventType {
    // Runner
    RunnerRegistered,
    RunnerDrained,
    RunnerHeartbeatMissed,
    // Execution
    ExecutionCreated,
    ExecutionCompleted,
    ExecutionEscalated,
    ExecutionCancelled,
    // Step
    StepDispatched,
    StepRunning,
    StepDone,
    StepSignals,
    StepConfirmed,
    StepFailed,
    StepTimeout,
    StepAdvanced,
    StepRetrying,
    // Artifact
    ArtifactDeclared,
    ArtifactClosed,
    // Source event (from watcher ingest)
    Source,
    // cx (deprecated — removed in the event-sources migration)
    CxTaskReady,
    CxTaskClaimed,
    CxTaskIntegrated,
    CxTaskShadowed,
    CxCommentAdded,
    CxPhaseComplete,
    // Git
    GitBranchPushed,
    GitMerged,
    GitMergeFailed,
    // Secrets
    SecretSet,
    SecretDeleted,
}
```

Each variant's `data` payload is a typed struct (e.g.
`StepDispatchedData`, `RunnerRegisteredData`) that serialises to/from
the `serde_json::Value` in the envelope. Type safety at the edges,
JSON in storage and on the wire.

### Configuration

```rust
pub struct WorkflowDef {
    pub name: String,
    pub description: String,
    pub steps: Vec<StepDef>,
    pub triggers: Vec<TriggerDef>,
}

pub struct StepDef {
    pub name: String,
    pub runtime: Option<RuntimeSpec>,
    pub action: Option<String>,
    pub output: Option<String>,
    pub workspace: WorkspaceDef,
    pub artifacts: Vec<ArtifactDecl>,
    pub transitions: Vec<TransitionDef>,
    pub max_retries: Option<u32>,
    pub max_visits: Option<u32>,
    pub max_visits_goto: Option<String>,
    pub on_fail: Option<String>,
}

pub struct RuntimeSpec {
    pub runtime: String,
    pub tty: bool,
    pub env: HashMap<String, String>,
    pub timeout: Option<Duration>,
    pub fields: HashMap<String, toml::Value>,
}

pub struct RuntimeDef {
    pub name: String,
    pub vars: IndexMap<String, VarDef>,
    pub command: CommandDef,
    pub files: Vec<FileMappingDef>,
    pub env: HashMap<String, String>,
    pub proxy: Vec<ProxyDef>,
    pub metrics: Vec<MetricDef>,
}
```

### Projections

```rust
/// The pool projection — current state of all runners.
pub struct PoolState {
    pub runners: HashMap<RunnerId, RunnerState>,
}

pub struct RunnerState {
    pub id: RunnerId,
    pub environment: String,
    pub labels: HashMap<String, String>,
    pub status: RunnerStatus,
    pub current_step: Option<StepAttemptId>,
    pub last_heartbeat: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
}

pub enum RunnerStatus {
    Idle,
    Assigned,
    Executing,
    Drained,
}

/// The execution projection — active and recent executions.
pub struct ExecutionsState {
    pub executions: HashMap<ExecutionId, ExecutionState>,
}

pub struct ExecutionState {
    pub id: ExecutionId,
    pub workflow: String,
    pub vars: HashMap<String, String>,
    pub status: ExecutionStatus,
    pub attempts: Vec<StepAttemptState>,
    pub current_step: Option<String>,
    pub current_attempt: u32,
    pub visit_counts: HashMap<String, u32>,
    pub created_at: DateTime<Utc>,
}

pub enum ExecutionStatus {
    Running,
    Completed,
    Escalated,
    Cancelled,
}

pub struct StepAttemptState {
    pub step: String,
    pub attempt: u32,
    pub runner_id: Option<RunnerId>,
    pub status: StepStatus,
    pub output: Option<String>,
    pub signals: Vec<String>,
    pub error: Option<String>,
    pub transition: Option<String>,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

pub enum StepStatus {
    Dispatched,
    Running,
    Done,       // pending confirm
    Confirmed,
    Failed,
}

/// The secrets projection — current secret names and values.
pub struct SecretsState {
    pub secrets: HashMap<String, String>,
}
```

---

## Configuration Loading

The search path is resolved once at startup by the process that needs
it (ox-server for workflows/triggers/config, ox-runner for runtime definitions).

```rust
/// Resolve the configuration search path.
/// 1. {repo}/.ox/
/// 2. Each directory in $OX_HOME (colon-separated, left to right)
fn resolve_search_path(repo_root: &Path) -> Vec<PathBuf> {
    let mut path = vec![];
    let repo_ox = repo_root.join(".ox");
    if repo_ox.is_dir() {
        path.push(repo_ox);
    }
    if let Ok(ox_home) = std::env::var("OX_HOME") {
        for dir in ox_home.split(':') {
            let expanded = shellexpand::tilde(dir);
            let p = PathBuf::from(expanded.as_ref());
            if p.is_dir() {
                path.push(p);
            }
        }
    }
    path
}

/// Find a named config file. First match wins.
fn find_config(search_path: &[PathBuf], subdir: &str, name: &str) -> Option<PathBuf> {
    for dir in search_path {
        let candidate = dir.join(subdir).join(format!("{name}.toml"));
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
```

A `config.toml` in each search-path directory controls which trigger
files to load. Trigger file lists are additive across the search path;
scalar values (e.g. `heartbeat_grace`) use first-match-wins. If no
`config.toml` exists, ox falls back to loading `workflows/triggers.toml`
from each search-path directory.

All workflow definitions are loaded eagerly at ox-server startup —
names must be unique across the merged search path. Triggers are loaded
separately from files listed in `config.toml`, decoupled from workflow
definitions. Runtime definitions are loaded by ox-runner when a step is
dispatched — the runner resolves the definition by name at that point.

---

## Event Ingestion

External events enter the system through one endpoint:
`POST /api/events/ingest`. The handler accepts a batch authored by a
watcher process, runs a single SQLite transaction that:

1. Compares `cursor_before` against the current `watcher_cursors[source]`
   row (CAS guard) — on mismatch, returns 409 with no side effects.
2. For each event, attempts `INSERT OR IGNORE INTO ingest_idempotency`.
   Duplicate keys silently drop the event.
3. Appends non-duplicate events as `EventType::Source` rows with a
   `SourceEventData` payload (`source`, `kind`, `subject_id`, `tags`,
   `data`).
4. Updates `watcher_cursors[source] = cursor_after` with fresh
   `updated_at` / `updated_seq` / cleared `last_error`.

On commit the handler applies projections and broadcasts the new events
to SSE subscribers. Subscribers never observe events that later roll
back. Batch append is a new primitive on `EventBus` — the existing
single-event `append()` is not reused inside the ingest transaction.

Two companion routes support watcher liveness and operator status:

- `GET /api/watchers/{source}/cursor` — returns the opaque cursor
  string (or `null` on first boot) for a watcher to resume from.
- `GET /api/watchers` — returns one row per known source for
  `ox-ctl status` and future UIs.

ox-server treats cursors as opaque blobs throughout. A cx watcher writes
a git sha; a github watcher writes a delivery id; the server never
parses either. See [event-sources-design.md](event-sources-design.md)
for the full module layout, HTTP shapes, and migration slices.

---

## Error Handling

Errors fall into two categories:

**Operational errors** — network failures, SQLite write errors, git push
failures, process spawn failures. These are retried or surfaced as step
failures. They use `anyhow::Result` for ergonomic propagation with
context.

**Domain errors** — unknown workflow name, undeclared runtime field,
invalid transition target, merge conflicts. These are typed enums
returned from domain functions and mapped to HTTP status codes or
event error fields.

```rust
/// Domain errors returned by ox-server API handlers.
pub enum OxError {
    NotFound(String),
    InvalidState(String),
    MergeConflict { branch: String },
    ValidationError(String),
}
```

ox-runner treats all runtime exits as observable facts — a non-zero exit
code is not a Rust error, it is a signal. Errors in the Rust sense are
reserved for infrastructure failures (socket died, can't write to disk).

---

## Authentication

v1 has no authentication. The ox-server API is unauthenticated.

The extension point is a middleware layer in the Axum router. When auth
is added:

- Runners authenticate on registration and receive a bearer token
- ox-ctl authenticates via a configured token or OAuth flow
- The herder authenticates as a service account
- SSE connections carry the token as a query parameter (SSE does not
  support custom headers in the browser `EventSource` API, but ox
  clients use `reqwest` and can set headers)

The API design does not assume authentication — no endpoints change
shape. Auth is purely additive middleware.

---

## Process Lifecycle

### ox-server

1. Parse CLI args (`--port`, `--db`, `--repo`, `--heartbeat-grace`)
2. Open SQLite database, run migrations
3. Replay event log to rebuild projections
4. Resolve configuration search path, load workflow definitions
5. Initialise bare git repo if not present
6. Start background tasks: cx poll loop, heartbeat checker
7. Start Axum server (API + SSE + git endpoints)
8. On SIGTERM: stop accepting connections, drain SSE, flush WAL, exit

### ox-herder

1. Parse CLI args (`--server`, `--pool-target`, `--heartbeat-grace`)
2. Connect to ox-server SSE stream (`Last-Event-ID: 0` for full replay,
   or from a persisted checkpoint)
3. Rebuild local state from replayed events
4. Enter main loop: process SSE events + periodic tick
5. On SIGTERM: finish current event, exit

The herder is stateless on disk. It rebuilds its understanding of the
world from the event stream on every startup. It may optionally persist
the last processed `seq` to a file for faster reconnection.

The herder does not spawn runners. It monitors pool size and drains
surplus runners, but scaling up is an external concern — runners are
started by provisioning scripts and register themselves with ox-server.

### ox-runner

1. Parse CLI args (`--server`, `--environment`, `--labels`)
2. Call `POST /api/runners/register` → receive `RunnerId`
3. Start heartbeat loop (periodic `POST /api/runners/{id}/heartbeat`)
4. Subscribe to SSE stream from event 0, replay full history
5. During replay: compact the stream to find pending assignment
   (`step.dispatched` to this runner sets it; `step.dispatched` for the
   same step to a different runner clears it; `step.confirmed`/
   `step.failed`/`step.timeout` clears it)
6. After replay: if pending assignment exists, execute it immediately
7. Go live: process new `step.dispatched` events as they arrive
8. Return to idle after step completes
9. On `runner.drained` event: finish current step, exit
10. On SIGTERM: if executing, attempt graceful completion; otherwise exit

### ox-ctl

Stateless. Each invocation parses args, makes one or more API calls,
formats output, and exits. No persistent state, no background processes.

---

## Directory Layout at Runtime

```
$OX_DATA/                          # --data-dir, default ~/.ox-data
  ox.db                            # SQLite database (event log + metadata)
  repo/                            # Bare git repository
    HEAD
    objects/
    refs/
  artifacts/                       # Artifact content (if filesystem-backed)
    {execution_id}/
      {step}/{attempt}/
        {artifact_name}
```

ox-server owns this directory. No other process reads or writes it
directly.
