# Events

Ox is event-sourced. All state is derived from an append-only event log.
Nothing is mutated in place. Current state is a projection of the log;
history is always recoverable by replaying it.

Components react to events rather than polling for state changes. The
event stream is the control plane.

---

## Two Planes

The event stream and the artifact API are deliberately separate:

**Event stream** — control plane. Low-volume lifecycle and routing events.
All subscribers receive all events. Used by ox-herder, ox-runner, and the
any UI.

**Artifact API** — data plane. Actual artifact content (log bytes, commit
lists, file contents). Fetched or streamed on demand per artifact. Not
routed through the event log. See [artifacts.md](artifacts.md).

Heartbeats are also kept off the event stream — they are timestamp writes
to the runner projection, not events. Only the meaningful signal
(`runner.heartbeat_missed`) enters the log. See [execution.md](execution.md).

This keeps the event stream lean. A subscriber watching for step transitions
is not burdened by log chunks or per-second pings from unrelated runners.

---

## The Event Log

ox-server maintains an append-only event log. Events are written
sequentially and assigned a monotonically increasing sequence number.
The log is the source of truth; all queryable state is derived from it.

Events are never modified or deleted. Projections are rebuilt by replaying
the log from the beginning, or from a snapshot plus a tail.

---

## Event Delivery

ox-server exposes the event log as an SSE stream:

```
GET /api/events/stream
```

Subscribers receive events in real-time as they are appended. The
`Last-Event-ID` header resumes from the last received sequence number —
the server replays all events after that sequence and then continues
streaming. This covers real-time delivery, reconnect, and catch-up after
a restart in a single interface.

### SSE Redaction

`secret.set` events are redacted before SSE broadcast — the `value`
field is stripped. The event log stores the full payload (including the
value); SSE subscribers receive only the secret name. This is the only
event type where the SSE payload differs from the stored payload. See
[secrets.md](secrets.md).

---

## Event Schema

Every event shares a common envelope:

```json
{
  "seq":  42,
  "ts":   "2026-04-04T12:00:00Z",
  "type": "step.confirmed",
  "data": { ... }
}
```

`seq` — monotonically increasing sequence number.
`ts` — server-side timestamp of when the event was appended.
`type` — dotted namespace string identifying the event type.
`data` — event-specific payload defined by the emitting component.

Event types are namespaced by component:

| Namespace | Defined in |
|-----------|-----------|
| `runner.*` | [runners.md](runners.md) |
| `execution.*` | [execution.md](execution.md) |
| `step.*` | [execution.md](execution.md) |
| `artifact.*` | [artifacts.md](artifacts.md) |
| `secret.*` | [secrets.md](secrets.md) |
| `cx.*` | [cx.md](cx.md) |
| `git.*` | [cx.md](cx.md) |
| `trigger.*` | [workflows.md](workflows.md) |

---

## Projections

Projections are read-only views derived from the event log. ox-server
maintains them in memory and rebuilds them on restart by replaying the log.

```
GET /api/state/pool          current runner registrations and assignments
GET /api/state/executions    active and recent executions with step attempt history
GET /api/state/cx            current cx node states (mirrors .complex/)
```

Projections are eventually consistent with the log — they reflect all
events up to the moment the request is served. For real-time needs,
subscribe to the SSE stream.

---

## The Herder as Event Subscriber

The herder subscribes to `GET /api/events/stream` and reacts to events
from all components. Its tick loop exists only for time-based checks that
cannot be expressed as event reactions: heartbeat staleness detection,
idle step timeouts, and periodic checkpoint triggers. All other herder
behaviour is event-driven.
