# Storage

ox-server uses SQLite for all persistent state: the event log, artifact
metadata, and optionally artifact content. This document covers the
schema, write and read paths, projection rebuilding, and artifact
storage.

For the event model itself, see [prd/events.md](prd/events.md). For
the artifact model, see [prd/artifacts.md](prd/artifacts.md).

---

## SQLite Configuration

The database file is `$OX_DATA/ox.db`. ox-server opens it at startup
with these pragmas:

```sql
PRAGMA journal_mode = WAL;          -- concurrent readers during writes
PRAGMA synchronous = NORMAL;        -- durability without fsync on every commit
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;         -- wait up to 5s on lock contention
```

WAL mode is critical. The SSE stream, API reads, and event appends all
happen concurrently. WAL allows readers to proceed without blocking on
writes and vice versa.

A single `rusqlite::Connection` handles all writes (event appends are
serialised). A connection pool (`r2d2` or a simple `Vec<Connection>`)
serves read-only queries.

---

## Schema

### events

The append-only event log. Source of truth for all state.

```sql
CREATE TABLE events (
    seq        INTEGER PRIMARY KEY,  -- monotonically increasing
    ts         TEXT    NOT NULL,      -- ISO 8601 UTC
    event_type TEXT    NOT NULL,      -- dotted namespace, e.g. "step.confirmed"
    data       TEXT    NOT NULL       -- JSON payload
);
```

`seq` is assigned by the writer, not `AUTOINCREMENT`. The writer holds
a monotonic counter in memory and assigns the next value on each append.
This avoids SQLite's autoincrement overhead and guarantees no gaps.

`data` is stored as a JSON text column. SQLite's JSON functions can
query it directly for ad-hoc analysis, but ox-server never queries
event data in the hot path — it reads events sequentially and applies
them to projections.

### artifacts_meta

Artifact metadata. Tracks what artifacts exist, their state, and
where their content lives.

```sql
CREATE TABLE artifacts_meta (
    execution_id TEXT    NOT NULL,
    step         TEXT    NOT NULL,
    attempt      INTEGER NOT NULL,
    name         TEXT    NOT NULL,
    source       TEXT    NOT NULL,     -- "log", "git-commits", "cx-diff", "file"
    streaming    INTEGER NOT NULL,     -- 0 or 1
    status       TEXT    NOT NULL,     -- "pending", "streaming", "closed"
    size         INTEGER,              -- bytes, set on close
    sha256       TEXT,                 -- set on close
    PRIMARY KEY (execution_id, step, attempt, name)
);
```

### artifacts_chunks

Artifact content stored in SQLite. Each chunk is a byte range within
an artifact.

```sql
CREATE TABLE artifact_chunks (
    execution_id TEXT    NOT NULL,
    step         TEXT    NOT NULL,
    attempt      INTEGER NOT NULL,
    name         TEXT    NOT NULL,
    offset       INTEGER NOT NULL,     -- byte offset within the artifact
    data         BLOB    NOT NULL,
    PRIMARY KEY (execution_id, step, attempt, name, offset)
);
```

Chunks are appended as they arrive from ox-runner. A complete artifact
is the concatenation of all chunks ordered by offset. The chunk size is
determined by the writer (ox-runner sends chunks as they arrive from
the runtime's stdout or artifact writes).

### Note on Secrets

Secret values are stored in the `events` table as part of `secret.set`
event payloads. This is intentional — the event log is the source of
truth, and secrets are event-sourced like everything else. Disk
encryption handles at-rest protection.

The `SecretsState` projection (name→value map) is rebuilt from
`secret.set` and `secret.deleted` events on startup, just like
`PoolState` and `ExecutionsState`.

### runners

Mutable projection table for heartbeats. Not part of the event log.

```sql
CREATE TABLE runners (
    runner_id      TEXT PRIMARY KEY,
    last_seen      TEXT NOT NULL,       -- ISO 8601 UTC, updated on heartbeat
    execution_id   TEXT,                -- current step (from heartbeat)
    step           TEXT,
    attempt        INTEGER
);
```

This is the only mutable table. Heartbeats are high-frequency timestamp
updates that do not belong in the event log. The step fields record what
the runner was last working on, so `runner.heartbeat_missed` events can
include the orphaned step. ox-server's background heartbeat checker checks
`last_seen` on its tick to detect stale runners.

Rows are inserted on `runner.registered` and deleted on `runner.drained`
or heartbeat expiry.

---

## Write Path

Event append is the critical path. It must be fast, serialised, and
immediately visible to SSE subscribers.

```
caller (API handler)
  │
  ▼
EventBus::append(event_type, data)
  │
  ├─ acquire write lock
  ├─ assign seq = next_seq; next_seq += 1
  ├─ ts = Utc::now()
  ├─ INSERT INTO events (seq, ts, event_type, data)
  ├─ apply event to in-memory projections
  ├─ broadcast to SSE subscribers (tokio::broadcast channel)
  └─ release write lock
```

The write lock serialises event appends. It is held for the duration of
the INSERT + projection update + broadcast. This is fast — a single
SQLite insert plus a few HashMap operations plus a channel send.

The broadcast channel is unbounded for the duration of a single event.
SSE subscriber tasks receive from the broadcast channel and write to
their HTTP response. If a subscriber falls behind, it will reconnect
using `Last-Event-ID` and replay from the database.

### Ordering Guarantee

Events are ordered by `seq`. The write lock ensures that the seq
assigned, the row inserted, the projection updated, and the broadcast
sent all happen atomically from the perspective of any reader. A
subscriber will never see a projection state that is ahead of or behind
the events it has received.

---

## Read Path

### Event Replay

Used for SSE reconnection and projection rebuilding.

```sql
SELECT seq, ts, event_type, data
FROM events
WHERE seq > ?
ORDER BY seq ASC
```

The query is indexed on the primary key (seq). Replaying the full log
on startup is O(n) in the number of events — acceptable for the
expected event volume (thousands to low tens of thousands per day).

### Projection Queries

Projections are in-memory structs. API endpoints read from these
directly — no SQL queries in the API hot path for state reads.

```
GET /api/state/pool         → reads PoolState
GET /api/state/executions   → reads ExecutionsState
GET /api/state/cx           → reads CxState
```

---

## Projections

Projections are in-memory views rebuilt from the event log. ox-server
maintains four projections:

### PoolState

Tracks runner registrations, assignments, and status.

Applied events: `runner.registered`, `runner.drained`,
`runner.heartbeat_missed`, `step.dispatched`, `step.confirmed`,
`step.failed`.

### ExecutionsState

Tracks executions, step attempts, and workflow positions.

Applied events: `execution.created`, `execution.completed`,
`execution.escalated`, `execution.cancelled`, `step.dispatched`,
`step.done`, `step.signals`, `step.confirmed`, `step.failed`,
`step.advanced`, `step.retrying`.

### SecretsState

Current secrets — a name→value map. Used internally by ox-server to
resolve `{secret:NAME}` references at step dispatch time.

Applied events: `secret.set`, `secret.deleted`.

### CxState

Mirrors the current state of `.complex/` on main. A map of node IDs
to their current state, tags, and recent comments.

Applied events: `cx.task_ready`, `cx.task_claimed`,
`cx.task_integrated`, `cx.task_shadowed`, `cx.comment_added`,
`cx.phase_complete`.

### Rebuild on Startup

```rust
fn rebuild_projections(db: &Connection) -> (PoolState, ExecutionsState, SecretsState, CxState) {
    let mut pool = PoolState::default();
    let mut execs = ExecutionsState::default();
    let mut secrets = SecretsState::default();
    let mut cx = CxState::default();

    let mut stmt = db.prepare(
        "SELECT seq, ts, event_type, data FROM events ORDER BY seq ASC"
    ).unwrap();

    for event in stmt.query_map([], EventEnvelope::from_row).unwrap() {
        let event = event.unwrap();
        pool.apply(&event);
        execs.apply(&event);
        secrets.apply(&event);
        cx.apply(&event);
    }

    (pool, execs, secrets, cx)
}
```

Each projection implements an `apply(&EventEnvelope)` method. The same
method is used during startup replay and during live operation when new
events are appended.

---

## Artifact Storage

Artifact content is stored in SQLite by default. This keeps deployment
simple — one file contains everything. For large deployments, artifact
content can be stored on the filesystem with SQLite holding only
metadata.

### SQLite Storage (Default)

Content is written as chunks to `artifact_chunks`. The chunk API:

**Write** — ox-runner POSTs chunks as they arrive:

```sql
INSERT INTO artifact_chunks (execution_id, step, attempt, name, offset, data)
VALUES (?, ?, ?, ?, ?, ?)
```

The offset is tracked by the caller. Each chunk is a contiguous byte
range.

**Read** — fetch a complete artifact:

```sql
SELECT data FROM artifact_chunks
WHERE execution_id = ? AND step = ? AND attempt = ? AND name = ?
ORDER BY offset ASC
```

Concatenate the `data` blobs in order.

**Stream** — for live artifact streaming, ox-server uses a
notification mechanism: when a chunk is written, a `tokio::watch`
channel signals waiting stream readers to poll for new chunks.

### Filesystem Storage (Optional)

When configured (`--artifact-store=fs`), artifact chunks are written
to files:

```
$OX_DATA/artifacts/{execution_id}/{step}/{attempt}/{name}
```

Each write appends to the file. Reads use standard file I/O. Live
streaming uses `inotify` (Linux) or polling.

The `artifacts_meta` table is used in both modes for metadata tracking.

---

## Capacity

Expected scale for a single ox installation:

| Dimension | Expected range |
|-----------|---------------|
| Events per day | 1,000–50,000 |
| Total events (retained) | 100,000–1,000,000 |
| Active executions | 1–20 |
| Runners | 1–10 |
| Artifact storage | 1–50 GB |

SQLite handles this comfortably. The event log table will be a few
hundred MB at the upper end. Artifact storage is the main growth
driver — log artifacts from long agent sessions can be several MB each.

### Retention

Events are never deleted from the log — the log is the source of truth.
Artifact content may be garbage-collected for old executions. A future
`ox-ctl gc` command would delete artifact chunks for executions older
than a threshold while preserving the event log and metadata.
