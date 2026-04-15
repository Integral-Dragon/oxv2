**Capability.** A standalone `ox-cx-watcher` binary observes a cx repo, maps cx facts to source events, and posts them to `POST /api/events/ingest` with CAS retry. Tested standalone; `ox-ctl up` does **not** launch it yet. To avoid double-firing, the in-server cx poller's trigger path is gated off (or the watcher runs observe-only during testing).

**PRD edits:**
- `docs/prd/cx.md` — full rewrite as "cx watcher: the reference event source." Lives in its own crate; stateless on disk; maps cx node states and comments into `node.ready` / `node.claimed` / `node.done` / `comment.added` source events; cursor advancement goes through the server. Stop claiming cx is part of the server's core. Doc stays cx/node/.complex-heavy — that is the point.
- `docs/prd/README.md` — "Agents call cx directly" → "steps may mutate source-specific state; cx is the local reference example." Move "cx on main" from core invariants into a "cx reference integration" callout.

**Design edits:**
- `docs/design.md` — add `ox-cx-watcher` crate to the workspace diagram.
- `docs/event-sources-design.md` already covers the crate layout.

**Code (red/green). Red — integration smoke test `ox-cx-watcher/tests/smoke.rs`:**
Spin up an in-memory ox-server, point the watcher at a temp cx repo with a small history. Assert:
1. Expected `SourceEvent`s land on the bus (`node.ready` for currently actionable nodes).
2. `watcher_cursors.cx` equals the repo HEAD after first batch.
3. A restart re-GETs the cursor and produces zero duplicates.

**Green:**
- New workspace member `ox-cx-watcher`.
- Move `ox-server/src/cx.rs` logic into the crate (copy, don't delete).
- `client.rs`: GET cursor on boot; POST batch with CAS retry on 409; backoff on 5xx.
- `mapping.rs`: cx node state → source event; state-suppression filter lives here; comments become `comment.added` with comment id in idempotency key.
- `main.rs`: clap args (`--server`, `--repo`, `--interval`), driver loop, graceful shutdown.
- Cold start: `cursor: null` → snapshot current cx state, emit events for actionable work, set `cursor_before: null, cursor_after: <HEAD>`.

Depends on: Slice 1, Slice 2.