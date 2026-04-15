# Platform

Ox runs locally today: clone the repo, build it, run `ox-up`, manage
agents from a terminal. The platform layer extends the same deterministic
workflow engine into a hosted service where teams connect repos, configure
workflows, and run agents in the cloud.

The architecture doesn't fork. The same ox-server, ox-herder, and
ox-runner components run in the cloud. The platform adds the product
surface and hardening teams need around that engine: authentication,
repo permissions, runner identity, secret handling, cloud runners,
multi-tenancy, a web UI, billing, and budget controls.

---

## Git Integration

Locally, ox-server hosts the repo via smart HTTP. On the platform,
users already have repos on GitHub, GitLab, or their own git servers.
Two models serve different markets.

### GitHub App (primary integration)

Ox installs as a GitHub App with repository access. This is the
expected onboarding path for most users.

**What changes from local:**
- Clone source is GitHub, not ox-server's `/git/` endpoint
- Branch push goes to GitHub, not ox-server
- `merge_to_main` creates a PR and merges via GitHub API instead of
  local git2 merge
- cx mutations still happen on branches — they land on main via the
  same merge path

**What stays the same:**
- Event log, projections, and all server-side logic
- Runner workspace provisioning (clone, branch, push)
- Two-phase completion (done → push → confirm)
- All artifacts, metrics, and signals

The GitHub App model means `merge_to_main` becomes: create PR →
auto-merge (if checks pass) or merge immediately (if configured).
The step waits for the merge to complete before confirming.

### Agent-native git hosting

An alternative to GitHub that's designed around agent workflows, not
human PR review queues.

**Why this matters:**
- GitHub's model optimises for human review: PRs, code browsing UI,
  comment threads, review requests. Agents don't need any of this.
- Branches in ox are disposable workspaces, not long-lived feature
  branches. GitHub's branch model adds overhead.
- High-throughput autonomous commits don't fit the PR approval queue
  model. An agent team producing 50 merges/day needs a different flow.
- First-class support for `.complex/` metadata, memory files, and
  skill resolution — git hosting that understands ox's conventions.

ox-server already has the bones of this: smart HTTP git endpoint,
single-writer-to-main discipline, event-sourced merge history. The
platform version extends it with:

- Multi-repo support (one ox-server hosts many repos)
- Authentication and access control
- Web-based repo browsing (read-only, API-first)
- Webhook integration for external CI/CD systems

This is a larger bet. The GitHub App model ships first. Agent-native
hosting is the long-term play for teams that outgrow GitHub's
agent-hostile ergonomics.

---

## Cloud Runners

Instead of seguro VMs on a laptop, runners spin up as cloud
infrastructure. The runner protocol is unchanged — register, subscribe
SSE, heartbeat, execute.

### Execution environments

| Environment | Trade-off | Use case |
|-------------|-----------|----------|
| Firecracker microVMs | Fast boot, strong isolation, low cost | Default for platform |
| Cloud VMs (GCP, AWS) | Heavier, more configurable | Users needing specific OS/GPU |
| Kubernetes pods | Existing infra, less isolation | Enterprise self-hosted |
| Fly.io machines | Fast cold start, simple deployment | Early platform launch |

The VM filesystem layout (see [vm-layout.md](../vm-layout.md)) is the
contract. Any environment that provides the expected mounts and
environment variables works. Runners are fungible — the platform
provisions them, ox doesn't care where they came from.

### Pool management

The platform manages pool size automatically based on:
- Active execution count (scale up when work is queued)
- Runner idle time (scale down when nothing is happening)
- User-configured WIP limits (respect budget constraints)
- Warm pool size (keep N runners ready for fast dispatch)

The herder's pool model doesn't change. It sees runners register and
tracks heartbeats. The platform's provisioner sits outside ox,
starting and stopping runner processes based on demand.

### Skill provisioning

Cloud runners cache skill images and repos. On dispatch:
1. Check cache for required skills at pinned versions
2. Pull/clone any missing skills
3. Provision the workspace with skills on PATH

Warm runners with pre-cached popular skills reduce dispatch latency.

---

## Multi-Tenancy

Each tenant (user or org) gets isolated state. The isolation model
scales from simple to sophisticated as the platform grows.

### Phase 1: tenant-per-instance

Each tenant gets their own ox-server + ox-herder. Simple, strong
isolation, easy to reason about. More expensive in infrastructure
but eliminates cross-tenant concerns entirely.

Practical for early platform launch — 10-100 tenants, each with their
own SQLite-backed ox-server instance.

### Phase 2: shared infrastructure

Multiple tenants on shared ox-server instances with logical isolation:
- Event log partitioned by tenant
- Projections scoped by tenant
- Runner pools isolated per tenant (a tenant's runners only see their
  own events)
- Secrets strictly isolated

SQLite → Postgres migration for durability and concurrent access.
Shared infrastructure reduces cost and operational burden at scale.

### What's scoped per tenant

| Resource | Isolation |
|----------|-----------|
| Event log | Separate stream per tenant |
| Projections | Separate in-memory state per tenant |
| Runners | Dedicated pool per tenant |
| Secrets | Strictly isolated, no cross-tenant access |
| Artifacts | Separate storage per tenant |
| Git repos | Per-tenant repository set |
| Workflows/personas | Per-tenant config (may reference shared ecosystem) |

---

## Web Dashboard

`ox-ctl` becomes a web interface. The SSE event stream and REST API
already support everything a rich UI needs — the dashboard is a
consumer, not a new backend.

### Views

**Execution timeline** — live view of running executions. Steps
progress in real-time via SSE. Click into a step to see streaming
logs, metrics, and artifacts.

**Task board** — the cx work graph visualised. Nodes with states,
edges showing dependencies, tags showing workflow triggers. Drag to
reorder, click to inspect, create tasks directly.

**Workflow editor** — visual step graph builder. TOML under the hood,
but presented as a flowchart with drag-and-drop steps, transition
wiring, and inline persona/skill configuration.

**Artifact viewer** — browse execution artifacts. Stream logs live.
View commit diffs. Inspect cx changes.

**Metrics dashboard** — token spend, duration, escalation rate, retry
counts over time. Per-persona, per-workflow, per-project views. The
self-improvement trends (see [self-improvement.md](self-improvement.md))
are visualised here.

**Ecosystem catalog** — browse and search the skill, persona, and
workflow registry. View publisher profiles, download counts, version
history.

**Approval gates** — human-in-the-loop decisions surfaced as UI
actions instead of CLI commands. Review a proposal, approve a merge,
respond to an escalation — all from the dashboard.

---

## Authentication and Authorisation

### User identity

Standard OAuth flow (GitHub, Google, email). Users belong to orgs.
Orgs own repos, workflows, secrets, and runner pools.

### Roles

| Role | Can do |
|------|--------|
| Owner | Everything. Manage members, billing, secrets, workflows |
| Admin | Manage workflows, personas, skills, runners. Cannot manage billing |
| Operator | View executions, approve gates, manage tasks. Cannot change config |
| Viewer | Read-only access to executions, metrics, artifacts |

### API authentication

API tokens scoped to tenant + role. Used by:
- `ox-ctl` connecting to the platform (instead of localhost)
- CI/CD integrations creating tasks or triggering workflows
- Webhook receivers

---

## Billing

Natural metering points already exist in the system. Billing wraps
the existing metrics.

### Metered dimensions

| Dimension | Source | Description |
|-----------|--------|-------------|
| Runner-minutes | `duration_ms` per step | Compute time |
| AI tokens | Proxy-collected metrics | Input + output tokens |
| Artifact storage | Artifact size tracking | GB stored |
| Executions | Event count | Workflow runs per month |

### Pricing models

**Usage-based** — pay for what you use. Runner-minutes + tokens +
storage. Good for variable workloads.

**Tier-based** — flat monthly fee for a pool size (WIP limit) and
token budget. Predictable for teams. Overage billed at usage rates.

**BYOK discount** — users who bring their own AI provider API keys
pay only for compute and storage, not tokens. The platform
orchestrates but doesn't mark up API costs.

### Budget controls

Users set spending limits per:
- Execution (max tokens/duration before escalation)
- Workflow (monthly budget cap)
- Org (total monthly spend limit)

The herder respects these limits — an execution that exceeds its
budget escalates rather than continuing to burn tokens.

---

## Onboarding

The path from signup to first useful execution:

1. **Sign up** — OAuth with GitHub
2. **Connect repo** — install GitHub App on a repo
3. **Pick a workflow** — choose from ecosystem templates or start with
   `code-task`
4. **Set secrets** — API keys for AI providers (or use platform-provided)
5. **Create a task** — write a cx issue (via dashboard) with workflow tag
6. **Watch it run** — live execution view, streaming logs

Time from signup to first agent running: under 5 minutes for a user
who has a repo and an API key ready.

---

## Self-Hosted Option

The platform is the hosted version of the same open-source ox engine.
Teams that want to run their own infrastructure can:

- Run ox-server, ox-herder, and ox-runner on their own machines
- Connect to the ecosystem registry for community skills/personas/workflows
- Use the web dashboard as a separate deployable (or stick with ox-ctl)
- Manage their own runner pool and secrets

The hosted platform and self-hosted ox share the same config format,
the same workflow definitions, and the same skill ecosystem. The
difference is who manages the infrastructure.
