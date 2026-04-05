# Metrics

Every step produces metrics. Metrics are structured observations about
what happened during a step — how long it took, what resources it
consumed, what the runtime reported. They are stored per step attempt
and queryable via the ox-server API.

Metrics come from three sources. Runner-collected and derived metrics
are always present. Runtime-reported metrics depend on what the runtime
definition declares and what the runtime process reports.

---

## Runner-Collected Metrics

ox-runner collects these automatically on every step. No configuration
needed.

| Metric | Type | Description |
|--------|------|-------------|
| `duration_ms` | gauge | Wall clock time from spawn to exit |
| `exit_code` | gauge | Runtime process exit code |
| `cpu_ms` | gauge | CPU time consumed (user + system) |
| `memory_peak_bytes` | gauge | Peak resident memory |
| `disk_read_bytes` | counter | Bytes read from disk |
| `disk_write_bytes` | counter | Bytes written to disk |
| `network_rx_bytes` | counter | Bytes received |
| `network_tx_bytes` | counter | Bytes sent |

Resource metrics (CPU, memory, disk, network) are collected from the
execution environment when available (cgroups, VM stats, etc.). If the
environment does not expose them, they are omitted — not zeroed.

---

## Runtime Metrics

Runtime metrics come from two sources: the API proxy and the runtime
process itself. Both are declared in the runtime definition and stored
alongside runner-collected metrics.

### Proxy-Collected Metrics

Runtime definitions can declare API proxies (see
[runtimes.md](runtimes.md#proxy)). When a proxy is configured, ox-runner
starts a local proxy that intercepts API traffic, extracts metrics from
request/response payloads, and reports them automatically. The runtime
process does not need to do anything — the proxy collects on its behalf.

```toml
# in .ox/runtimes/claude.toml

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

The `provider` field tells the proxy how to parse responses. Each
provider knows how to extract token counts, model identifiers, and
latency from that API's response format. The set of providers is built
into ox-runner (`anthropic`, `openai`).

### Runtime-Reported Metrics

The runtime process can also report metrics directly via the runtime
interface. This is a third capability alongside `done` and `artifact`:

**metric** — report a named metric with a value. The runtime can report
metrics at any point during execution. ox-runner forwards them to
ox-server.

```toml
[[runtime.metrics]]
name   = "cache_hit_rate"
type   = "gauge"
source = "runtime"
description = "Fraction of lookups served from cache"
```

`source = "runtime"` (the default) means the runtime process reports
this metric via the interface. `source = "proxy"` means the proxy
collects it. A runtime definition can mix both sources.

### Undeclared Metrics

A runtime may report metrics not declared in the definition — they are
accepted and stored, but have no type or description metadata.
Declarations are for schema discovery, not enforcement.

---

## Metric Types

| Type | Value | Semantics |
|------|-------|-----------|
| `gauge` | single numeric value | A point-in-time measurement. Duration, peak memory, exit code |
| `counter` | single numeric value | A cumulative total. Tokens, bytes, API calls. Only increases |
| `histogram` | multiple numeric values | A distribution. Reported as individual observations; ox-server computes percentiles, min, max, mean |
| `label` | string | A categorical value. Model name, runtime version. Not numeric |

### Reporting

**gauge** and **counter** — the runtime reports a name and a numeric
value. For counters, the runtime may report incremental updates or a
final total. ox-runner stores the last value seen.

```
metric input_tokens 14523
metric output_tokens 3847
metric model sonnet
```

**histogram** — the runtime reports individual observations. Each report
appends to the distribution:

```
metric response_latency_ms 234
metric response_latency_ms 189
metric response_latency_ms 412
```

ox-server computes summary statistics (count, min, max, mean, p50, p95,
p99) from the observations when the step closes.

---

## Derived Metrics

ox-runner computes these after the runtime exits, from the workspace
state and implicit artifacts. Always collected when the data is
available.

| Metric | Type | Source | Description |
|--------|------|--------|-------------|
| `commits` | gauge | git log | Number of commits on the branch |
| `lines_added` | gauge | git diff | Lines added vs branch base |
| `lines_removed` | gauge | git diff | Lines removed vs branch base |
| `files_changed` | gauge | git diff | Number of files modified |
| `cx_nodes_created` | gauge | cx-diff | cx nodes created by this step |
| `cx_nodes_updated` | gauge | cx-diff | cx nodes modified by this step |
| `cx_comments_added` | gauge | cx-diff | cx comments added by this step |

---

## Storage and Access

Metrics are stored per step attempt — `{execution_id}/{step}/{attempt}`.
Each attempt has its own complete set of metrics from all three sources.

### API

```
GET /api/executions/{id}/steps/{step}/metrics
```

Returns all metrics for the latest attempt. With `?attempt=N`, returns
metrics for a specific attempt.

```json
{
  "runner": {
    "duration_ms": 245000,
    "exit_code": 0,
    "cpu_ms": 18200,
    "memory_peak_bytes": 524288000,
    "network_tx_bytes": 1048576
  },
  "runtime": {
    "input_tokens": 14523,
    "output_tokens": 3847,
    "model": "sonnet",
    "api_calls": 12,
    "response_latency_ms": {
      "count": 12,
      "min": 142,
      "max": 891,
      "mean": 312,
      "p50": 267,
      "p95": 734,
      "p99": 891
    }
  },
  "derived": {
    "commits": 3,
    "lines_added": 247,
    "lines_removed": 89,
    "files_changed": 8,
    "cx_comments_added": 1
  }
}
```

### ox-ctl

```
ox-ctl exec show <id>
```

Includes a metrics summary for each step in the execution detail view.

### Events

Metrics are included in the `step.confirmed` event payload as a summary.
This keeps the control plane informed without a separate event — metrics
are part of the step result.

```json
{
  "type": "step.confirmed",
  "data": {
    "execution_id": "aJuO-e1",
    "step": "implement",
    "metrics": {
      "duration_ms": 245000,
      "input_tokens": 14523,
      "output_tokens": 3847
    }
  }
}
```

The `metrics` field in the event carries a subset — runner duration and
runtime-declared counters/gauges. Full metrics including histograms and
derived metrics are available via the API.
