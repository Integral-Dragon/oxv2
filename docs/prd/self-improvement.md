# Self-Improvement

> **Status:** Later-stage workflow pattern. Ox should first prove that
> deterministic workflows can run useful hands-off agent work. The
> self-improvement loop depends on that execution history existing.

Ox captures a complete record of every execution: event logs, step
artifacts (logs, commits, cx diffs), metrics (tokens, duration,
retries, signals), and outcomes (completed, escalated, merged). This
record is the input to a self-improvement loop where retro agents
review what happened and update memory files that shape future runs.

The system gets better at its own work over time. Not through
fine-tuning or weight updates, but through the same mechanism humans
use: reflection, pattern recognition, and writing things down.

---

## The Loop

```
agents do work
    → artifacts capture everything
        → retro agent reviews outcomes
            → memory files updated
                → next run's prompts are better
                    → agents do better work
```

This is a workflow like any other. The retro agent is a persona. The
memory update is a step that writes files and merges to main. The
trigger is a cx event (`cx.phase_complete`, `execution.escalated`,
or a poll trigger on a schedule). Ox already has every primitive
needed.

---

## Memory Layers

Memory accumulates at four scopes. Each scope has different content,
different readers, and different update cadence.

### Per-persona memory

What a specific persona has learned about working in this project.

```
.ox/memory/personas/software-engineer.md
```

Content examples:
- "Tests in this repo use a custom harness — check Makefile, not cargo test"
- "The auth module has implicit config-ordering dependencies — read config loader before refactoring"
- "When this persona touches the API layer, integration tests catch more issues than unit tests"

**Written by:** retro agent after reviewing executions by this persona.
**Read by:** this persona on every future step (injected into prompt context).
**Update cadence:** after each phase or after escalations.

### Per-project memory

What any agent should know about this codebase, independent of persona.

```
.ox/memory/project.md
```

Content examples:
- "The CI matrix takes 12 minutes — don't wait for it in a tight loop"
- "Module X is being deprecated in favor of module Y — new code should use Y"
- "The database migration system requires migrations to be idempotent"

**Written by:** retro agent after reviewing cross-persona patterns.
**Read by:** all personas working on this project.
**Update cadence:** after each phase.

### Per-phase memory

What happened in a specific batch of work. Phase memory is a summary,
not a journal — it captures decisions, surprises, and patterns from a
set of related executions.

```
.ox/memory/phases/phase-{id}.md
```

Content examples:
- "3 of 5 tasks in this phase hit the same test flake in module X — root cause was shared test state"
- "The reviewer persona rejected 4 proposals for insufficient error handling — may need to update the engineer persona's instructions"
- "Token spend was 2x budget because agents explored dead-end approaches — tighter scoping in proposals would help"

**Written by:** retro agent when a phase completes.
**Read by:** planning agents, humans reviewing phase outcomes, the retro agent itself (to track trends).
**Update cadence:** once per phase.

### Cross-project memory

Meta-learnings that apply beyond a single project. These feed back
into the ecosystem — updated skill instructions, persona defaults,
workflow recommendations.

```
.ox/memory/cross-project.md
```

Content examples:
- "The postgres-query skill works better when agents run pg-schema first — update the skill's prompt.md"
- "Two-step review (plan review + code review) catches 3x more issues than single-step review"
- "Escalation rate drops 40% when the engineer persona includes examples of common pitfalls"

**Written by:** retro agent with access to multiple project histories (platform-level, not project-level).
**Read by:** ecosystem maintainers, persona/skill/workflow authors.
**Update cadence:** periodic (weekly, monthly) or on-demand.

---

## Retro Workflows

A retro is a workflow. It uses the same step graph, persona, and
trigger system as any other workflow.

### Phase retro

Triggered when a phase completes. Reviews all executions in the phase.

```toml
[[trigger]]
on       = "cx.phase_complete"
workflow = "retro-phase"

[workflow]
name = "retro-phase"

[[step]]
name      = "analyze"
persona   = "retro-analyst"
workspace = { git_clone = true, branch = "retro/{task_id}", push = true }
prompt    = """Review all executions in this phase. For each execution, examine:
- Logs: what did the agent attempt? Where did it struggle?
- Metrics: token spend, duration, retries, escalations
- Signals: no_commits, dirty_workspace, exited_silent
- Commits: what code was actually produced?
- Source side effects: what did the step mutate in cx / GitHub / Linear?
- Outcomes: did review pass or fail? Did the task escalate or get
  shadowed? Did a follow-up task have to clean up this work? A retro
  that only examines process optimizes for legible process; a retro
  that examines outcomes optimizes for quality.

Write the phase summary to .ox/memory/phases/phase-{task_id}.md.
Pick at most ONE persona to update this run and propose a diff to
.ox/memory/personas/<chosen>.md. Cross-persona observations go in
.ox/memory/project.md. Multiple personas per run dilute review
attention and produce diffs nobody reads carefully.

Every proposed entry must cite at least THREE concrete execution
references supporting it. One-off observations are noise; patterns
are signal. Name the references in the entry itself so future
retros can re-check the pattern.

For every entry you add, find one to delete — stale, redundant, or
never-referenced across subsequent runs. Memory files have a tight
token budget and additive-only updates bloat them until signal
drowns. Without downward pressure, memory converges to its size
ceiling and degrades every future run.

Post the proposal as a cx comment with --tag retro-proposal, linking
to the cited executions so the reviewer can verify the evidence."""

[[step]]
name    = "review"
persona = "retro-reviewer"
prompt  = """Review the retro's proposed memory changes against the
evidence. Memory has higher leverage than code — a bad memory entry
shapes every future run, not just one module. Reject aggressively.

1. Read the proposal: `cx comments {workflow.task_id} --tag retro-proposal`
2. Read the memory diff: `git diff origin/main..HEAD -- .ox/memory/`
3. For each proposed entry, verify:
   - Does it cite ≥3 execution references that genuinely support
     the claim, not retrospective pattern-matching on noise?
   - If it contradicts an older entry, is the new evidence strong
     enough to override, or should both be flagged as unresolved?
   - For deletions: is the entry genuinely stale, or does the retro
     just not like it?

A rejected retro is cheaper than a memory file poisoned with
hallucinated patterns. Post verdict as a cx comment and report
pass/fail."""

[[step.transition]]
match = "pass"
goto  = "merge"

[[step.transition]]
match = "fail"
goto  = "analyze"

[[step]]
name   = "merge"
action = "merge_to_main"
workspace = { branch = "retro/{task_id}" }
```

### Escalation retro

Triggered when an execution escalates. Reviews the specific failure.

```toml
[[trigger]]
on       = "execution.escalated"
workflow = "retro-escalation"
```

Escalation retros are higher-priority — they represent a failure the
system should learn from immediately, not at the end of a phase.

### Scheduled retro

A poll trigger that fires periodically to consolidate and prune memory.

```toml
[[trigger]]
on            = "node.ready"
source        = "cx"
tag           = "retro-schedule"
poll_interval = "24h"
workflow      = "retro-consolidate"
```

The consolidation retro reads all memory files, checks whether
observations are still relevant (has the code changed? was the issue
fixed?), prunes stale entries, and merges related observations.

---

## Memory File Format

Memory files are markdown. They live in `.ox/memory/` and are
committed to the repo like any other file. They go through the same
branch → merge flow as code changes.

```markdown
# Project Memory

## Codebase Patterns
- Database migrations must be idempotent — the migrate tool replays from scratch in CI
- The `handlers/` module uses a macro-based dispatch pattern — read `dispatch!` macro before adding endpoints

## Known Pitfalls
- Test suite has shared state in `tests/fixtures/db.rs` — causes flakes when tests run in parallel
- The `config::reload()` path doesn't re-validate secrets — don't assume secrets are fresh after reload

## What Works
- Breaking large refactors into type-change-first, then logic-change steps reduces review rejections
- Running `cargo clippy` before submitting catches 80% of reviewer nitpicks
```

The format is deliberately simple. No frontmatter, no structured
schema, no machine-readable sections. The reader is an LLM — it
understands markdown prose. The retro agent writes in whatever
structure makes sense for the content.

### Injection into prompts

Memory files are injected into the agent's context at step dispatch,
alongside the persona and task prompt. The prompt assembly order is:

1. Persona (who you are)
2. Project memory (what you should know about this codebase)
3. Persona memory (what you've learned from past runs)
4. Task prompt (what to do now)
5. Previous step output (what happened last)
6. Skill index (what tools you have)

Memory sections are omitted when the corresponding files don't exist
or are empty.

### Memory as scratchpad, persona as durable record

Memory is an overlay — injected alongside the persona at dispatch,
not written back into the persona source. That has a real advantage:
memory can be pruned, versioned, and experimented with independently
of the persona. But it also means the persona source (e.g.,
`inspired/software-engineer.md`) drifts from what the agent actually
sees at runtime. A reader of the source won't know why the agent
behaves a certain way; the real behavior lives in a separate file
they may not think to read.

The correction pressure is **promotion**: when a persona-memory
entry has stabilized across N phases — still cited, still relevant,
no contradictions — the consolidation retro proposes promoting it
into the persona source and retiring the memory entry. Memory is
the scratchpad where new observations are tested; persona source
is the durable record of what survived the test. Without promotion,
memory files become the shadow truth and the persona source
ossifies into historical documentation.

---

## Consolidation

Unbounded memory growth degrades performance — more context means
more tokens, slower processing, and diluted signal. The consolidation
step actively manages memory size.

### Pruning rules

- **Stale observations** — if the code referenced by an observation has
  changed significantly, the observation may no longer apply. The retro
  agent checks `git log` on referenced files.
- **Redundant entries** — multiple observations about the same thing get
  merged into one.
- **Low-value entries** — observations that were never referenced in a
  subsequent run's logs (the agent didn't encounter that situation
  again) are candidates for removal after N phases.
- **Contradicted entries** — a new observation that contradicts an
  older one is a *trigger for investigation*, not an automatic winner.
  Recency is a heuristic, not truth. Flag both entries as unresolved,
  require the retro to produce fresh evidence supporting one or the
  other, and resolve only after the contradiction is examined.
  Silently removing the older entry is how memory drifts toward
  whichever observation is loudest, not whichever is correct.

### Size targets

Memory files should stay small enough to fit comfortably in a prompt
context window. Guidelines:

| Scope | Target size | Rationale |
|-------|-------------|-----------|
| Per-persona | < 2KB | Read on every step by this persona |
| Per-project | < 4KB | Read on every step by every persona |
| Per-phase | < 2KB | Read by planning agents, not on every step |
| Cross-project | < 4KB | Read by ecosystem maintainers, not by agents |

These are guidelines, not hard limits. The retro agent uses judgment
about what's worth keeping within the size budget.

---

## Measuring Improvement

The metrics system (see [metrics.md](metrics.md)) already captures
the signals needed to measure whether the self-improvement loop is
working:

| Metric | What it measures | Improving means |
|--------|-----------------|-----------------|
| Escalation rate | % of executions that escalate | Decreasing |
| Retries per execution | Average retry count | Decreasing |
| Tokens per task | Token spend per completed task | Decreasing |
| Duration per task | Wall-clock time per task | Decreasing |
| Review pass rate | % of proposals/code that pass first review | Increasing |
| Signal frequency | How often no_commits, dirty_workspace fire | Decreasing |

These metrics are tracked over time. The retro agent can reference
trends: "escalation rate dropped from 30% to 15% over the last 3
phases" or "token spend increased 2x this phase — investigate."

The platform dashboard (see [platform.md](platform.md)) visualises
these trends, making the self-improvement loop visible to the human
operator.

### Goodhart on adoption

The subtle failure mode: a retro that watches its proposals get
adopted will learn to write *adoptable-looking* proposals rather
than *useful* ones. The metrics above will drift in whatever
direction the retro has learned correlates with approval, which is
not necessarily the direction of quality.

Guard against this with periodic **shadow retros**: a human (or a
second cold-started retro with no access to the production retro's
output) reviews a phase independently and compares conclusions. If
the human consistently agrees, the loop is healthy. If they
consistently disagree on which patterns matter, the retro has
drifted — revise its persona, or prune whatever stale framing is
driving the drift. Without an outside check, a closed feedback loop
is indistinguishable from progress at the metric level while
quietly degrading at the quality level.

---

## Bootstrapping

A fresh project has no memory. The first runs operate without any
accumulated knowledge. The self-improvement loop bootstraps naturally:

1. First phase runs with no memory — baseline performance
2. First retro writes initial observations
3. Second phase runs with initial memory — performance should improve
4. Second retro refines and adds to memory
5. Pattern continues, with consolidation preventing bloat

Projects can also seed memory from templates:

```bash
ox init --memory-template ox-community/rust-project-memory
```

Community-maintained memory templates capture common patterns for
specific ecosystems (Rust, Python, React, etc.) so new projects
don't start completely cold.

Seeded entries should enter tagged as **unverified** and be the
first candidates for consolidation until a retro has had a chance
to confirm or prune them against actual project behavior. A seeded
entry has the same syntactic authority as one earned through three
retro cycles, but none of the evidence — and inherited-but-wrong
memory is worse than no memory, because it biases every future
retro against noticing the error. Treat seeds as hypotheses to
test, not facts to inherit.
