# Ox

Ox is a deterministic workflow engine for hands-off agentic work.

Agents do judgment-heavy work: propose plans, write code, review changes,
investigate failures, and explain tradeoffs. Ox does coordination:
triggering workflows, dispatching steps, advancing state, retrying failures,
enforcing branch discipline, collecting artifacts, and escalating to humans.

Ox does not use agents to orchestrate agents. Workflow control is explicit,
event-driven, replayable infrastructure.

The operating model is close to CI/CD for agent work: external events start
workflows, isolated runners execute steps, artifacts record what happened,
and deterministic transitions decide what runs next.

For coding workflows, this feels like **GitHub for agents**: issues track
work, branches isolate attempts, merges serialize completed work, runners
are fungible VMs, and humans review escalations instead of sitting in the
happy path.

---

## Product Doctrine

### Agents do cognition

Agents are used where language-model judgment is valuable:

- Reading ambiguous task context
- Proposing implementation plans
- Writing code
- Reviewing code
- Investigating failures
- Summarizing tradeoffs
- Asking humans for clarification

### Ox does coordination

Ox owns the mechanics that should not depend on model judgment:

- Trigger matching
- Workflow state
- Step dispatch
- Retries and visit limits
- Runner liveness
- Timeout handling
- Transition matching
- Artifact collection
- Branch merge discipline
- Escalation routing

### Events are facts

Ox advances only from explicit events: a task became ready, a step was
confirmed, a runner missed heartbeat, a merge failed, an artifact closed,
or a signal was observed.

An agent may recommend what should happen next, but Ox decides whether the
workflow is allowed to advance.

### Workflows are policy

Teams encode their operating policy in workflow files. The workflow graph is
the source of truth for what happens next, not an agent improvising process
at runtime.

---

## Core Layers

The system is designed in layers. Each layer works independently but
amplifies the ones below it:

1. **Workflow engine** — deterministic event-driven orchestration.
2. **Execution layer** — isolated runners, artifacts, metrics, and signals.
3. **Agent interface** — personas, runtimes, and skills.
4. **Integrations** — event sources such as cx, GitHub, Linear, Jira, and
   webhooks.
5. **Product surfaces** — CLI, dashboard, hosted platform, and APIs.
6. **Learning loop** — retro workflows and memory built on execution
   history.

---

## Components

### ox-server

The hub. Owns all shared state and exposes it through an HTTP API and an SSE
event stream. Passive — it responds to requests and never initiates action.

ox-server is the **single writer to main**:

- **cx on main** — cx mutations reach main only via branch merges. Agents
  write cx freely on their branch (calling `cx` directly is fine and
  expected); the user writes cx through interactive workflow steps on a
  branch. ox-server executes the merge. Nothing writes cx to main directly.
- **git main** — merges to main are always an ox-server operation. Agents
  and the user push to branches; only ox-server advances main.

ox-server also owns the event log. Every state change — execution created,
step dispatched, artifact received, branch merged — is an event appended to
the log. Current state is a projection of that log. See
[events.md](events.md).

### ox-herder

The active loop. A separate binary that reads state from ox-server via API,
makes decisions, and acts by calling ox-server API endpoints. It never
mutates state directly.

The herder is not AI. It counts things and fires triggers:

- Tails the ox event stream for external work events. In the reference
  local setup, those events come from cx state changes derived from git log.
  Other event sources can map GitHub, Linear, Jira, cron, or webhook events
  into the same trigger model.
- Monitors running executions for liveness. Stale runner heartbeat →
  re-dispatch. Retries exhausted → shadow the cx task, fire the escalation
  step defined in the workflow.
- Advances workflow steps when confirmed results arrive.
- Drains surplus runners when the pool exceeds the target size.

The herder does not spawn runners. ox-runner processes are started
externally (e.g. via a script that creates seguro VMs) and register
themselves with ox-server on startup. The herder only observes the
pool and drains runners it no longer needs.

Because the herder uses only the public API, it can be restarted
independently, run out-of-process, or replaced entirely without touching
ox-server.

### ox-runner

The executor. Analogous to a GitHub Actions runner: ox-runner registers with
ox-server, subscribes to the SSE event stream for step assignments, executes
them, and reports results.

ox-server does not know or care where ox-runner is running — it is any process
that can reach the ox-server API. This means ox-runner can run inside a
[seguro](../../seguro) VM for local isolated execution, in a GCP or AWS VM,
in a Kubernetes pod, or on bare metal. The execution environment is a
deployment decision, not an architectural one.

Each ox-runner instance is stateless and fungible. It does not know about other
instances, does not share state with them, and carries no identity between
steps. The step assignment contains everything it needs: workflow name, step
workspace spec, runtime spec, and artifact declarations.

An ox-runner process **is** a runner. It registers once with ox-server on
startup and remains registered for its lifetime. Steps are assigned to the
runner, executed, and completed — after which the runner becomes idle and
eligible for the next assignment. The runner is not released between steps.

ox-runner does not distinguish between agent steps and interactive (human)
steps. Both are processes. The only difference is whether the runtime spec
declares `tty = true`, in which case ox-runner allocates a TTY so a human can
interact with the session. Signals, artifacts, and two-phase confirmation
work identically in both cases.

This is the WIP limit mechanism: the number of registered runners is the
maximum number of steps executing concurrently. Pool size is controlled by
starting and stopping ox-runner instances, not by runners checking in and out.

On each assigned step:

1. Receives assignment via the ox-server SSE stream
2. Provisions workspace (git clone, branch checkout)
3. Spawns the runtime process according to the runtime definition
4. Exposes the runtime interface — the runtime uses this to report completion and write artifacts
5. Forwards artifact content to the ox-server artifact API
6. Collects step signals after runtime exits
7. Pushes branch and calls confirm — completing the two-phase handoff
8. Returns to idle — the runner remains registered, awaiting the next assignment

The runner is released only when the ox-runner process exits (VM shutdown or
pool drain). See [execution.md](execution.md) for the full lifecycle.

### ox-ctl

The operator CLI. Used by humans at a terminal to interact with ox-server.
A thin wrapper around the HTTP API with consistent output formatting and
`--json` support for scripting.

ox-ctl is not used by agents. Runtimes communicate with ox-runner through
the runtime interface (see [runtimes.md](runtimes.md)). Steps may mutate
source-specific state as a side effect — e.g. a cx-integrated workflow
shells out to `cx` to claim, comment, integrate, or shadow nodes on its
branch. cx is one reference example; other source systems
(Linear, GitHub, Jira) would be mutated through their own clients.

See [ox-ctl.md](ox-ctl.md) for the full command reference.

---

## Event Sources

Ox workflows start from events. An event source observes an external system,
maps native activity into Ox events, and advances a server-side cursor.

cx is the reference event source for local development. The intended plugin
boundary is watcher processes that ingest events through
`POST /api/events/ingest`.

This keeps the workflow engine independent from any particular work tracker.

---

## External Dependencies

### cx

A file-native hierarchical issue tracker. Nodes have states
(`latent → ready → claimed → integrated`), typed edges (`blocks`,
`waits-for`, `discovered-from`, `related`), tags, comments, and a `meta`
field for arbitrary orchestrator data.

cx has no knowledge of ox. It is a passive read/write tool against JSON
files. ox-server is its only writer in a running ox installation.

cx's own event log (`events/`) is not used. ox-server derives cx events by
diffing `.complex/` changes in git commits — the git log already contains
the full mutation history. See [cx.md](cx.md).

### seguro

A QEMU-based VM sandbox for CLI coding agents. Provides filesystem
isolation (virtiofs shares), network isolation (transparent proxy with
allow/deny), ephemeral SSH keys, TLS inspection, and AI API token metering.

Seguro is the reference local execution environment for ox-runner.
ox-runner is started inside a seguro VM externally — via a provisioning
script, systemd unit, or similar — and registers itself with ox-server
on startup using the `OX_SERVER` environment variable. ox does not
manage seguro sessions directly; launching and destroying VMs is an
operational concern outside ox's scope. Other execution environments
(cloud VMs, Kubernetes pods, bare metal) work the same way — start
ox-runner with `OX_SERVER` set, and it joins the pool.

---

## Configuration Search

Ox loads configuration — runtime definitions, workflow definitions, and
personas — by searching a list of directories in order. The first match
wins.

The search path is:

1. `.ox/` in the managed repository (project-local)
2. Each directory listed in `$OX_HOME` (colon-separated, left to right)

```sh
# project adds its own runtimes and workflows alongside ox defaults
OX_HOME=/opt/ox/defaults:~/.ox
```

With this search path, a project can:

- **Override a default** — place a `runtimes/claude.toml` in the repo's
  `.ox/` to replace the system-wide definition for that project.
- **Extend defaults** — add a project-specific `runtimes/my-agent.toml`
  or `workflows/deploy.toml` without touching the shared defaults.
- **Use only defaults** — omit `.ox/` from the repo entirely and rely
  on `$OX_HOME`.

The directory structure is the same at every level:

```
<search-dir>/
  runtimes/    *.toml — runtime definitions
  workflows/   *.toml — workflow definitions
  personas/    *.md — persona files (markdown with YAML frontmatter)
  skills/      <name>.md or <name>/ — skill packages (markdown + optional bin/)
  memory/      *.md — accumulated agent memory (per-persona, per-project)
```

ox-server resolves the search path at startup. Workflow and runtime
names must be unique across the merged search path — if the same name
appears in multiple directories, the first match (highest priority) is
used.

### Hot Reload

Configuration can be reloaded without restarting ox-server. Three
triggers:

- **SIGHUP signal** — `kill -HUP <pid>`. Standard Unix convention.
- **API endpoint** — `POST /api/config/reload`.
- **CLI** — `ox-ctl reload`.

On reload, ox-server re-reads all files from the search path, validates
them (persona vars checked against runtime definitions), and atomically
swaps the live config. If validation fails, the old config is kept and
an error is returned.

`ox-ctl config check` validates config files without applying —
reports errors and shows what would change (added/removed workflows,
runtimes, personas).

In-flight executions are unaffected by a reload. The herder picks up
new config on its next trigger evaluation cycle (within 30 seconds).

---

## Key Principles

**Event sourced.** All state is derived from an append-only event log.
Nothing is mutated in place. Current state is a projection; history is
always recoverable. See [events.md](events.md).

**Single writer to main.** Nothing writes cx or code to main except
ox-server's merge operation. Agents write cx freely on their branch. The
user writes cx through interactive workflow steps on a branch. The herder
reads from main — which only advances via merges — so it never races with
in-progress work. The merge is the serialisation point.

**Fungible executors.** ox-runner instances have no identity. Any runner can
run any step. The step assignment is self-contained. Fresh clone, fresh
workspace, fresh runtime process for every step. Pool size — and therefore
the WIP limit — is controlled by the number of registered runners.

**Branch discipline applies to everyone.** All work — agent or human —
happens on branches. User interactions (approving objectives, giving
feedback, adjusting plans) happen through interactive workflow steps that
work on a branch and merge when done. No direct writes to main from any
actor.

**External events arrive via watcher plugins.** ox-server does not poll
any external system itself. Watcher processes (one per source) observe
source systems and push source events to `POST /api/events/ingest`. The
server stores events, matches triggers, and fires workflows — it has no
built-in knowledge of cx, GitHub, Linear, or any other tracker. cx is
the reference watcher. See [event-sources.md](event-sources.md).

**Deterministic orchestration.** Ox never asks an agent which step should
run next. Agents produce outputs and artifacts; workflow definitions decide
how those facts route execution.

**Two-phase completion.** A step is not complete until the runner confirms
it — after pushing the branch and collecting signals. The herder does not
advance until confirmation. See [execution.md](execution.md).

**Artifacts are first-class.** Every step produces artifacts: logs, commits,
cx activity, and declared files. Streaming artifacts are observable in
real-time. See [artifacts.md](artifacts.md).

**Personas are primary.** A workflow step names a persona, not a
runtime. The persona declares what runtime it uses, what model, and
what skills it needs. Runtimes are mechanical — "how to run claude" —
and personas are meaningful — "who is doing this work." Swapping a
persona can change the runtime, model, and skills without touching
the workflow.

**Orchestration is configuration.** Personas, skills, and workflow
definitions are files. The infrastructure is generic. The Inspired
workflow (PM → tech lead → engineer → reviewer) is one expression of
the model, not the model itself. See [docs/workflows/](../workflows/).

**Skills are portable.** A skill is files on disk and executables on
PATH. No protocol dependency. Any agent that can read files and run
commands can use ox skills. The ecosystem grows because the format is
agent-agnostic — the lock-in is in orchestration and the registry, not
the file format. See [skills.md](skills.md).

**The system can improve itself.** Execution artifacts, metrics, and
signals can feed retro workflows that update memory files. This is a
workflow pattern built on the engine, not a separate orchestration system.
See [self-improvement.md](self-improvement.md).

**The herder is dumb.** No AI in the infrastructure layer. The herder counts
things, checks conditions, and fires triggers. Intelligence lives in agents
and personas.

---

## What Ox Is Not

**Not a UI (yet).** ox-server exposes an API and SSE stream sufficient
for a rich human interface. ox-ctl is the CLI interface within this
repository. The web dashboard is part of the platform layer — see
[platform.md](platform.md).

**Not cx.** cx is a standalone tool that works without ox. Ox depends on cx;
cx does not depend on ox.

**Not seguro.** seguro is a standalone sandbox tool. Ox uses seguro for
execution isolation; seguro has no knowledge of ox workflows.

**Not an agent coordinator agent.** Ox does not put an LLM in charge of
workflow control. Agents perform bounded cognitive tasks inside workflow
steps. Ox handles orchestration deterministically.

**Not a dynamic process inventor.** Ox does not infer workflow structure,
choose arbitrary next steps, or invent process from model output. If a
workflow needs a new path, encode that path in the workflow definition.
Agents may propose workflow changes as artifacts, but applying those
changes is a normal branch/merge operation.

---

## Roadmap Layers

The core product is deterministic event-driven workflow execution for
hands-off agentic work. Several layers amplify that core but are not
required for the engine to be useful:

- **Skills** package tools, scripts, and instructions for reuse across
  personas and workflows.
- **Ecosystem** is a future distribution layer for sharing proven skills,
  personas, and workflows.
- **Platform** is hosted Ox: cloud runners, GitHub integration, a web
  dashboard, authentication, multi-tenancy, billing, and budget controls.
- **Self-improvement** is a later-stage workflow pattern where retros review
  execution history and propose memory updates.

---

## Further Reading

- [events.md](events.md) — event model, log, SSE, projections
- [workflows.md](workflows.md) — workflow TOML, steps, transitions, triggers
- [artifacts.md](artifacts.md) — artifact model, streaming, implicit artifacts
- [execution.md](execution.md) — pool, runners, two-phase completion, signals
- [runners.md](runners.md) — runner model, lifecycle, heartbeats, pool
- [runtimes.md](runtimes.md) — runtime definitions, command templates, runtime interface
- [secrets.md](secrets.md) — secret management, delivery, injection
- [metrics.md](metrics.md) — step metrics, collection, metric types
- [cx.md](cx.md) — cx integration, branch discipline, git log events
- [ox-ctl.md](ox-ctl.md) — CLI reference
- [skills.md](skills.md) — skill format, hierarchy, resolution, packaging
- [ecosystem.md](ecosystem.md) — registry for skills, personas, workflows
- [self-improvement.md](self-improvement.md) — retro workflows, memory layers
- [platform.md](platform.md) — SaaS architecture, cloud runners, web dashboard
- [../design.md](../design.md) — implementation architecture
- [../vm-layout.md](../vm-layout.md) — VM filesystem layout for runners
- [../workflows/](../workflows/) — reference workflow definitions
