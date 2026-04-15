# Ecosystem

> **Status:** Future leverage layer. The core Ox product works without a
> registry. The ecosystem becomes valuable after deterministic workflows,
> reusable skills, and proven personas exist in enough real projects to
> share.

Ox's value is in the workflow engine and the composability of its
configuration: skills define what agents can do, personas define who
agents are, workflows define how work flows. All three are files in git
repos. The ecosystem is a registry and community that makes these
shareable, discoverable, and composable across projects and teams.

---

## Three Shareable Primitives

| Primitive | What it is | Format | Typical scope |
|-----------|-----------|--------|---------------|
| **Skills** | Capability packages (tools, scripts, instructions) | Markdown with YAML frontmatter + optional `bin/` | Generic (database, search, deploy) or domain-specific |
| **Personas** | Agent identity (runtime, skills, expertise, judgment) | Markdown with YAML frontmatter | Generic (reviewer, engineer) or team-specific |
| **Workflows** | Step graphs with transitions, retries, escalation | TOML file | Generic (code-task, bug-fix) or org-specific |

Skills and personas share the same format: markdown with YAML
frontmatter. The frontmatter carries metadata for the engine; the
markdown body carries instructions for the agent. TOML is reserved for
the orchestration layer (runtimes, workflows, triggers).

All three share the same distribution mechanism: git repos published
to the registry, referenced by name in configuration.

---

## Registry

The registry maps names to git repos + versions. It is a lookup
service, not a package host — the source of truth is always the git
repo.

### Namespaces

```
ox-community/postgres-query     # community-maintained
acme-corp/deploy-pipeline       # org-private
dragon/careful-coder            # individual publisher
```

Namespaces are scoped to publishers. A publisher is a verified
identity (individual or org) that can push to their namespace.

### Manifest format

Skills and personas carry their manifest in YAML frontmatter.
Workflows carry theirs in the TOML `[workflow]` header.

```markdown
# Skill or persona — markdown with YAML frontmatter
---
name: senior-reviewer
version: 1.0.0
description: Thorough code reviewer focused on correctness and maintainability
runtime: claude
model: sonnet
tags: [review, quality]
---

You are a senior code reviewer...
```

```toml
# Workflow — TOML
[workflow]
name        = "code-task"
version     = "2.1.0"
description = "Propose, review, implement, review, merge"
tags        = ["development", "standard"]
```

### Resolution

When a workflow references `ox-community/senior-reviewer`, the
resolution path is:

1. Check local search path (`skills/`, `personas/`, `workflows/`)
2. Check local cache (previously resolved registry entries)
3. Query registry API: name → git URL + version tag
4. Clone at pinned version into cache
5. Validate manifest matches expected type

Resolution happens at ox-server startup (or on config reload). Not at
dispatch time — dispatch uses the already-resolved definitions.

### Version pinning

References can pin versions:

```toml
[workflow]
persona = "ox-community/senior-reviewer@1.0.0"
skills  = ["ox-community/postgres-query@^0.3"]
```

Without a version pin, the latest release is used. For production
workflows, pinning is strongly recommended.

---

## Publishing

Publishing is pushing a tagged release to a git repo and registering
it with the registry.

```bash
# Tag a release
git tag v1.0.0
git push origin v1.0.0

# Register with the ecosystem
ox publish                      # reads manifest, registers with registry
```

`ox publish` validates the manifest, checks that the tag exists, and
registers the mapping in the registry. The registry stores:

- Namespace/name
- Type (skill, persona, workflow)
- Git URL
- Version tags
- Description and tags (for search/discovery)
- Publisher identity
- Checksum of manifest at publish time

### Visibility

- **Public** — anyone can use. Community skills, standard workflows.
- **Org-private** — visible only to members of the publishing org.
  For internal tools, proprietary personas, company-specific workflows.

---

## Composition

The power of the ecosystem is mix-and-match. A workflow references
personas and skills by registry name. A user combines community
building blocks with their own customisations.

```toml
[workflow]
name    = "our-code-task"
persona = "ox-community/careful-coder@2.0"
skills  = ["ox-community/shell", "ox-community/web-search"]

[[step]]
name    = "implement"
skills  = ["acme-corp/internal-api-client"]
prompt  = "Implement the task."

[[step]]
name    = "review-code"
persona = "acme-corp/senior-reviewer"    # override workflow persona
```

This workflow uses a community persona as the default, adds community
shell and web-search skills at the workflow level, adds an org-private
API client skill for the implementation step, and overrides the
persona for the review step with an org-specific one.

The persona declares its own runtime and model — the workflow author
doesn't need to specify those. Swapping from a Claude-based persona to
a Codex-based persona is just changing the persona reference; the
workflow TOML doesn't change.

### Override precedence

When the same name appears at multiple levels:

1. Step-level persona overrides workflow-level persona
2. Skills are additive across all levels (runtime ∪ persona ∪ workflow ∪ step)
3. Local search path overrides registry for same-named entries

This lets a project fork a community persona, place it in `.ox/personas/`,
and have it take precedence over the registry version without changing
any workflow references.

---

## Trust and Curation

Skills get shell access and secret injection. Personas shape agent
judgment. Trust matters.

### Verified publishers

Publishers verify their identity (GitHub account, org membership, or
similar). Verified publishers get a badge in the registry. Unverified
publishers can still publish, but their packages are flagged.

### Permission declarations

Skills declare what they need in their frontmatter:

```yaml
---
name: postgres-query
inputs:
  connection_url:
    type: secret
    description: PostgreSQL connection string
requires:
  bins: [psql]
  network: ["*.postgres.example.com:5432"]
---
```

The `network` field declares what external hosts the skill needs to
reach. Runners with network isolation (seguro, cloud VMs) can enforce
this — only allow traffic to declared destinations.

Personas declare the skills and secrets they need:

```yaml
---
name: database-admin
runtime: claude
model: sonnet
skills: [postgres-query]
secrets: [pg_connection_url]
---
```

The `secrets` field is informational — the runner warns when a
persona's expected secrets aren't set, but doesn't block execution.

### Community signals

- **Download counts** — how widely used
- **Publisher reputation** — how many packages, how long active
- **Version frequency** — actively maintained or abandoned
- **Audit trail** — git history of the skill source is always available

### Pinned versions

Production workflows should pin skill and persona versions. The
registry supports `ox audit` to check for:

- Unpinned dependencies
- Skills with known issues
- Outdated versions with available patches

---

## Discovery

The registry provides search and browse:

```bash
ox search "postgres"                 # search across all types
ox search --type skill "database"    # skills only
ox search --type persona "review"    # personas only
ox browse ox-community               # list a namespace's packages
```

The web dashboard (see [platform.md](platform.md)) provides a visual
catalog with categories, popularity, and publisher profiles.

---

## Cross-Agent Compatibility

The skill format is deliberately agent-agnostic. A skill is files on
disk and executables on PATH. Any agent runtime that can read files
and run commands can use ox skills.

This means the ecosystem is not locked to ox:

- A Claude Code user can clone an ox skill repo and add the `bin/`
  to their PATH manually
- A Codex user can reference `prompt.md` in their system prompt
- A Cursor user can add `prompt.md` content to their rules
- A standalone script can call `bin/` commands directly

Ox provides the orchestration — automatic provisioning, secret
injection, prompt assembly, version management — but the skills
themselves are portable. This is intentional: ecosystem adoption
grows faster when there's no lock-in at the format level. The lock-in
is in the orchestration and the registry, not the file format.

### Format convergence

The industry is converging on "markdown files that shape agent
behavior": Claude Code skills, Cursor rules, Codex AGENTS.md, Devin
playbooks. The ox format (markdown with YAML frontmatter) is the
same convention used by Claude Code and Cursor. An ox skill or
persona is already a valid file for these tools:

- The markdown body works as a Claude Code skill, Cursor rule, or
  AGENTS.md section
- `bin/` scripts work anywhere with a shell
- YAML frontmatter is either understood (Claude Code) or ignored
  (Codex, Cursor)

Adapters can import from other formats:

```bash
ox import-skill --from cursor-rules .cursor/rules/postgres.md
ox import-skill --from claude-skill ~/.claude/commands/deploy.md
```

---

## Ecosystem and Self-Improvement

The ecosystem connects to the self-improvement loop (see
[self-improvement.md](self-improvement.md)) at the cross-project
level. Aggregate learnings about skills and personas — which
combinations work well, which fail often, what common pitfalls
exist — feed back into the ecosystem as:

- Updated skill instructions (better `prompt.md`)
- Updated persona defaults (tuned from aggregate performance data)
- Community-contributed "known issues" on registry entries
- Recommended skill/persona/workflow combinations

The registry becomes not just a package index but a knowledge base
about what works.
