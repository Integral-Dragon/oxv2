# Skills

A skill is a capability package that gives an agent the ability to do
something beyond reading and writing code. A skill is a markdown file
with YAML frontmatter — the same format as a persona. The markdown
body contains instructions the agent reads. An optional `bin/`
directory alongside it provides executables the agent can run.

Skills are the answer to "what can this agent actually do?" Personas
define who the agent is (and declare what skills they need). Workflows
define what steps to take. Skills define what tools are available.

---

## Skill Format

A skill is a markdown file with YAML frontmatter, optionally
accompanied by a `bin/` directory and `examples/` directory:

```
my-skill/
  my-skill.md      # manifest (frontmatter) + agent instructions (body)
  bin/              # executables the agent can invoke
  examples/         # usage examples (agent can read for few-shot learning)
```

Or as a single file with no executables:

```
skills/
  web-search.md    # just instructions, no bin/
```

### The markdown file

The frontmatter declares metadata the host system needs. The body is
the agent's interface documentation.

```markdown
---
name: postgres-query
version: 0.3.0
description: Query PostgreSQL databases, inspect schemas, and explain query plans
inputs:
  connection_url:
    type: secret
    description: PostgreSQL connection string
  max_rows:
    type: string
    default: "100"
    description: Row limit for queries
requires:
  bins: [psql]
tags: [database, postgresql, observability]
---

# postgres-query

You have access to PostgreSQL query tools. Use these when you need to
inspect database schema, run read-only queries, or understand query
performance.

## Available commands

- `pg-query <sql>` — Run a read-only SQL query. Returns results as a table.
- `pg-schema [table]` — Show table schema. Without args, lists all tables.
- `pg-explain <sql>` — Run EXPLAIN ANALYZE on a query.

## Guidelines

- Always use pg-schema before writing queries against unfamiliar tables.
- Queries are read-only. If you need writes, escalate to a human.
- Results are limited to {max_rows} rows by default.
```

### Frontmatter fields

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Skill identifier, unique within a registry namespace |
| `version` | yes | Semver version |
| `description` | yes | One-line description — shown to the agent in the skill index |
| `inputs` | no | Named parameters the skill needs. Types: `secret`, `string`, `bool`, `int` |
| `requires.bins` | no | Binaries that must be on PATH for the skill to work |
| `requires.network` | no | Hosts the skill needs to reach (for network-isolated runners) |
| `tags` | no | Categories for registry discovery |

The markdown body supports `{name}` interpolation — skill inputs,
secrets, and builtins are available, same as in runtime definitions.

### bin/

Executables the agent calls. These are placed on PATH when the skill
is in scope. They can be shell scripts, compiled binaries, Python
scripts with shebangs — anything executable.

```bash
#!/usr/bin/env bash
# bin/pg-query — run a read-only SQL query
set -euo pipefail
psql "$PG_CONNECTION_URL" --no-psqlrc -t -A -c "SET statement_timeout = '10s'; $1"
```

Skill inputs declared as `type: secret` are injected as environment
variables (uppercased, prefixed with the skill name):
`connection_url` on skill `postgres-query` becomes
`PG_CONNECTION_URL`.

### examples/

Optional directory of usage examples. The agent can read these for
few-shot learning when deciding how to use the skill. Plain text or
markdown files showing input/output pairs.

---

## Skill Hierarchy

Skills are declared at four levels. Each level adds to the set — the
agent sees the union.

```
runtime skills  ∪  persona skills  ∪  workflow skills  ∪  step skills  =  available skills
```

### Runtime level

Base tools available to every step that uses this runtime. These are
capabilities so fundamental to the runtime that they're always present.

```toml
# .ox/runtimes/claude.toml
[runtime]
name = "claude"
skills = ["ox-skills/shell", "ox-skills/web-search"]
```

### Persona level

Tools this persona needs regardless of workflow. A security auditor
needs different tools than a rapid prototyper — that's a property of
the persona, not the step.

```markdown
---
name: database-admin
runtime: claude
model: sonnet
skills: [postgres-query, redis-cli]
---

You are a database administrator...
```

### Workflow level

Domain-specific tools added for all steps in this workflow.

```toml
# .ox/workflows/data-pipeline.toml
[workflow]
name = "data-pipeline"
skills = ["acme-corp/pg-tools", "acme-corp/grafana-reader"]
```

### Step level

Additional tools for a specific step. Used when a step needs a
capability that other steps in the workflow don't.

```toml
[[step]]
name = "analyze-logs"
persona = "inspired/software-engineer"
skills = ["acme-corp/es-query"]
prompt = "Analyze the recent error logs and identify root causes."
```

### No subtraction

Skills only add. A step cannot remove a skill granted by the runtime,
persona, or workflow. This keeps the model simple and predictable — if
a persona declares a skill, it's always there. If you need a
restricted environment, define a separate persona with a different
skill set.

---

## Skill Resolution

Skills are resolved by ox-server at dispatch time, the same as
runtime definitions and personas.

### Local skills

Skills in the configuration search path are referenced by name:

```yaml
skills: [my-custom-tool]
# resolves to: .ox/skills/my-custom-tool/ or $OX_HOME/skills/my-custom-tool/
```

The search path includes a `skills/` directory alongside `runtimes/`,
`workflows/`, and `personas/`:

```
<search-dir>/
  runtimes/    *.toml — runtime definitions
  workflows/   *.toml — workflow definitions
  personas/    *.md — persona files (markdown with YAML frontmatter)
  skills/      <name>/ or <name>.md — skill packages
  memory/      *.md — accumulated agent memory
```

A skill can be a directory (with `.md` file + `bin/` + `examples/`)
or a single `.md` file (instructions only, no executables).

### Registry skills

Skills from the ecosystem registry are referenced by namespace/name:

```yaml
skills: [ox-community/postgres-query]
```

Registry skills are resolved to a git URL + version by the registry,
cloned (or cached) by the platform, and made available on the search
path. See [ecosystem.md](ecosystem.md).

### Resolution at dispatch

When ox-server dispatches a step:

1. Resolve the persona (from step or workflow default)
2. Collect skills from runtime + persona + workflow + step definitions
3. Deduplicate by name (same skill referenced at multiple levels is
   included once)
4. Resolve each skill via the search path (local) or registry (namespaced)
5. Validate that `requires.bins` are satisfiable in the target runner
   environment
6. Resolve `inputs` — map secret-typed inputs from the secrets
   projection, string-typed from workflow vars or defaults
7. Include the resolved skill specs in the dispatch payload

The runner receives everything it needs to provision skills without
access to the registry or search path.

### Runner provisioning

On receiving a dispatch with skills, ox-runner:

1. Places each skill's `bin/` contents into a skills directory on PATH
2. Sets environment variables for each skill's secret inputs
3. Assembles a skill index (names + descriptions) for prompt injection
4. Makes skill markdown files available at a known path the runtime can
   reference

The prompt assembly step gains a new section listing available skills
with their descriptions. The agent reads individual skill files when
it decides to use a specific skill.

---

## Format Rationale

### Why markdown with YAML frontmatter

The industry is converging on "files that shape agent behavior":

- **Claude Code** — `.claude/commands/*.md` with YAML frontmatter
- **Cursor** — `.cursor/rules/*.md` with frontmatter
- **Codex** — `AGENTS.md`, pure markdown
- **Devin** — playbooks, markdown

Nobody has a two-file manifest + instructions pattern. It's all
markdown. The ox skill format follows this convention — a single
markdown file is the core unit. The `bin/` directory is an ox addition
for executable tools, but the skill identity is the markdown file.

This means ox skills are portable:

- A Claude Code user can drop a skill's `.md` into `.claude/commands/`
- A Codex user can reference it in their system prompt
- A Cursor user can add it to `.cursor/rules/`
- The `bin/` scripts work anywhere with a shell

### Same format as personas

Skills and personas are both markdown with YAML frontmatter. This is
deliberate — they're both agent-facing documents with a small amount
of metadata. The consistency makes the ecosystem simpler: one format
for everything agents read, TOML for everything the engine reads
(runtimes, workflows, triggers).

---

## Skill Design Principles

**Agent-agnostic.** A skill is files on disk + executables on PATH.
Claude, Codex, or any other agent that can read files and run commands
can use it. No protocol dependency (no MCP, no custom IPC).

**Forkable.** Don't like how a skill works? Fork the repo. Change the
instructions or the scripts. Pin your fork in your workflow.

**Inspectable.** The agent can read the source of its own tools. The
markdown is not a black box — the agent sees the instructions and can
reason about them. The `bin/` scripts are readable shell or source
code.

**Composable.** Skills are independent units. They don't depend on
each other (a skill that needs another skill should bundle it or
declare a `requires`). They compose through the hierarchy — personas
and workflow authors decide what combination of skills an agent gets.

**Versionable.** Pinned to git tags. Auditable via git history. No
silent updates to a skill that has shell access and secret injection.

---

## Packaging

### Short-term: git repos

A skill is a git repo (or a directory in a mono-repo). `ox install`
clones it. Version pinning is git tags. No registry needed — just git
URLs.

```bash
ox install https://github.com/ox-skills/postgres-query --version 0.3.0
```

### Medium-term: Docker images

For skills with complex system dependencies (native libraries, specific
OS packages), the skill can declare a Docker image in its frontmatter.
The runner pulls the image and mounts the skill's `bin/` from the
container, or runs skill commands inside the container.

```yaml
---
name: opencv-analysis
version: 0.2.0
image: ox-skills/opencv-analysis:0.2.0
---
```

This is opt-in. Most skills are just shell scripts and don't need
containers.

### Long-term: ox package manager

A purpose-built package manager that understands skill metadata,
version resolution, dependency trees, and trust verification. Replaces
raw git cloning with proper dependency management. See
[ecosystem.md](ecosystem.md).
