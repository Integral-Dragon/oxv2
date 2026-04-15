**Capability.** ox-server can accept a batch of source events from an HTTP client, dedup them, advance a watcher cursor atomically, and append `EventType::Source` rows to the bus. Nothing calls it yet; cx poller still runs.

**PRD edits (done in a prior commit, verify on pickup):**
- `docs/prd/README.md` — replaced "git log as event source" with watcher-plugin principle.
- `docs/storage.md` — added `watcher_cursors` + `ingest_idempotency` tables; marked `CxState` deprecated.

**Design edits (done):**
- `docs/design.md` — added `ingest` module, `EventType::Source`, `## Event Ingestion` section; marked `cx` module as removed-in-migration.
- `docs/api.md` — added `### Watchers` section with the three routes; added routes to router structure.

**Code (red/green). Red — unit tests in `ox-server/src/api.rs`:**
1. `GET /api/watchers/cx/cursor` on an empty db returns `{ cursor: null, updated_at: null }` + 200.
2. `POST /api/events/ingest` with `cursor_before: null, cursor_after: "abc", events: [e1]` returns 200, appends one `EventType::Source` row, and updates the cursor row.
3. Same POST replayed returns 200, `ingest_idempotency` suppresses the duplicate event, cursor stays at `"abc"`.
4. POST with wrong `cursor_before` returns 409, no writes, `last_error` stashed.

**Green:**
- Add `EventType::Source` + `SourceEventData` to `ox-core/src/events.rs`.
- Add `watcher_cursors` + `ingest_idempotency` tables in `ox-server/src/db.rs`.
- Add `bus.get_watcher_cursor()` and `bus.ingest_batch()` (new batch append primitive).
- Add `list_watchers`, `get_watcher_cursor`, `ingest_batch` handlers + routes.

cx poller remains live and untouched — tree is green and ships.