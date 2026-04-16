## Motivation

When \`claude\` (or any runtime) fails for infrastructure reasons — auth error, quota, network — ox-runner today classifies it identically to a legitimate workflow step failure. Observed in elu: 6 executions burned all 4 retries and escalated because \`~/.claude/.credentials.json\` in the seguro sandbox was invalid. Every attempt exited within ~1.2s with \`\"is_error\":true, \"error\":\"authentication_error\"\` in the claude stream-json output, then ox-runner signaled \`exited_silent\`, server retried, escalation followed.

Retrying an auth error four times accomplishes nothing except slow-walking toward escalation. The runner has the information to distinguish \"runtime infrastructure broke\" from \"agent ran and failed\"; we just don't use it.

## Constraint: ox-runner stays runtime-agnostic

Today ox-runner has zero runtime-specific knowledge — it runs a command, tails stdout, watches the ox-rt socket for \`done\`/\`metric\`/\`artifact\`, and collects generic signals (\`exited_silent\`, \`fast_exit\`, \`no_commits\`, \`dirty_workspace\`). This boundary must be preserved. Any detection for claude-specific or codex-specific errors belongs in that runtime's config, not in runner code.

## Design

Add a generic **declarative log-pattern signal detection** feature. The runner gains one new capability: \"scan log tail for regexes the runtime config declared, emit matched names as signals.\" Runtime-specific knowledge (what auth failure *looks like* in claude's output) stays in \`claude.toml\`.

### Config shape (runtime.toml)

\`\`\`toml
[[runtime.failure_signals]]
name = \"auth_failed\"
pattern = '\"error\":\"authentication_error\"|Invalid authentication credentials'
retriable = false   # workflow engine skips retries, escalates directly
tail_bytes = 65536  # optional, default 65536
\`\`\`

### Scan logic (ox-runner)

- **What**: last N bytes of the step's log file (default 64 KiB), not last N lines (bytes bound is predictable — no risk of pathological line lengths).
- **How**: \`seek(End, -N)\`, read forward, strip partial-UTF-8 at the start, scan as one string.
- **When**: after process exit, before signal collection (\`runner.rs\` around line 623). Exactly one pass per step.
- **Regex engine**: Rust's \`regex\` crate — linear-time guaranteed, no catastrophic backtracking.
- **Compile**: at config load, not at scan time. Bad pattern fails server startup, never a step mid-execution.
- **Multiple matches**: each matched signal is an independent entry in \`signals\`.
- **Diagnostics**: include the matched line (or its offset) in the \`step.signals\` event payload so operators see why a signal fired.

### Policy (workflow engine / core)

- A signal with \`retriable = false\` bypasses \`max_retries\` and escalates the step directly.
- Reuses existing signal-to-failure plumbing — the runner already reports signals, the engine already decides action. This change is additive: today only \`exited_silent\` triggers failure; after this, any declared non-retriable signal can too.

### Claude runtime config

Ship \`claude.toml\` with an \`auth_failed\` pattern matching both the stream-json error field and the human-readable text, so it works whether claude is invoked with or without \`--output-format stream-json\`.

## Risk ledger

| Risk | Mitigation |
|---|---|
| False positive from agent content (e.g. editing auth code) | Bounded to log tail; patterns target structured terminal output like \`\"is_error\":true\` |
| Regex DoS / catastrophic backtracking | \`regex\` crate is linear-time by construction |
| Bad pattern crashes runner mid-step | Patterns compile at startup |
| Runaway scan cost on long-running steps | Hard byte cap per scan, one scan per step |
| Silent ingestion of unexpected signals | Every matched signal is recorded in \`step.signals\` with its name + matched line |
| Log file missing / unreadable | Empty signals, log warning |
| Behavioral regression | Opt-in: runtimes without \`failure_signals\` behave identically to today |

## Explicitly out of scope

- **Streaming / during-execution matching** — expands blast radius, invites partial-match bugs.
- **Scanning the whole log** — cost scales with step duration.
- **Automatic retry policy in the runner** — runner emits signals, workflow engine decides action. Detection and policy stay separate.
- **Runtime-specific parsing (stream-json decoding, etc.) in ox-runner** — violates the generic-runner boundary.
- **Wrapper scripts per runtime** — pushes complexity into a parallel tree of shell glue.

## Proposed slices

1. **Config schema** — add \`RuntimeFailureSignal { name, pattern, retriable, tail_bytes }\` to runtime config in ox-core; parse + compile regexes at load; unit tests for parse + compile-error failure modes.
2. **Runner scan** — \`scan_failure_signals(log_path, runtime_cfg) -> Vec<SignalMatch>\` in ox-runner, called from \`runner.rs\` post-exit pre-signal-collection. Unit tests with a fixture log file covering: match, no match, partial-UTF-8 boundary, missing file, empty file, tail smaller than file, tail larger than file.
3. **Workflow policy** — workflow engine (\`ox-core/src/workflow.rs\` or wherever retry decisions live) honors \`retriable: false\` and escalates directly. Unit test: signal with \`retriable = false\` → \`RetryDecision::Exhausted\` on first failure.
4. **Claude runtime** — ship \`defaults/runtimes/claude.toml\` with an \`auth_failed\` pattern. Integration check: run a step against a sandbox with a broken credential, assert one failure → immediate escalate.

Each slice is a vertical red/green pair. Slice 4 is the end-to-end payoff that proves the machinery works.

## Repro for verification

1. In a seguro sandbox, replace \`~/.claude/.credentials.json\` with an invalid token.
2. Kick off a \`code-task\` execution.
3. Before: 4 retries, ~5s each, escalates with \`signal:exited_silent\`.
4. After: 1 attempt, escalates immediately with \`signal:auth_failed\`.

## Related

- Originated during elu debugging session 2026-04-16; DB at \`/home/dragon/projects/elu/.ox/run/ox.db\` has the full event history in case the repro needs revisiting.