# Bug

`ox-ctl status` after `ox-ctl up --runners=1` with a lingering drained runner:
```
pool        2 runners (0 executing, 1 idle)
```
Count is incoherent — the 2 includes a drained runner that isn't part of the live pool. User expects "1 runner".

## Plan

### Slice 1 — server splits `pool_size` from `pool_drained`
`ox-server/src/api.rs` status(): `pool_size` counts only non-Drained; new `pool_drained` field. Invariant: `pool_size == pool_executing + pool_idle`. Client struct in `ox-core/src/client.rs` gets the new field.

### Slice 2 — CLI shows drained separately + sorts detail table
`cmd_status` prints `pool  N runners (X executing, Y idle, Z drained)`. Detail table sorted executing → idle → drained → else, then by id, so drained never leads.
