## Observed

Ran `ox-ctl up` in a repo while a stale `ox-server` was already bound to port 4840 (from a different repo). The server process failed at startup with:

```
Error: Address already in use (os error 98)
```

but `ox-ctl` still launched `ox-herder`, `ox-cx-watcher`, and the runners. They all silently attached to the wrong server — the watcher got back a cursor that was a commit sha from the *other* repo's cx history, so every `cx log --since <sha>` failed with `Invalid revision range` and no events ever ingested.

Also noticed `.ox/run/ox.pids` was never written, even though child processes were running.

## Expected

If `ox-server` fails to bind, `ox-ctl up` should:

1. Detect the failure (non-zero exit / log "Address already in use").
2. Abort before launching the other components.
3. Surface the error to the user clearly.

## Repro

1. Start `ox-server --port 4840` in repo A.
2. In repo B, run `ox-ctl up`.
3. Observe: repo-B watcher/herder/runners start anyway and talk to repo A's server.