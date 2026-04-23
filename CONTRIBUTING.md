# Contributing to ox

This doc is for people **building ox from source**. If you just want to
use ox, the [README](README.md) is where you should be.

## prerequisites

Same as running ox: a Linux host with KVM, seguro, cx, and Claude Code
installed. See the [README](README.md#getting-started) for setup.

On top of that you'll want a recent stable Rust toolchain (`rustup
update stable`) and `make`.

## building

Clone the repo and install all six binaries into `~/.cargo/bin`:

```bash
git clone https://github.com/integral-dragon/oxv2
cd oxv2
make install
```

Cargo can't install a whole workspace in one command, so `make install`
loops the binary crates (`ox-server`, `ox-herder`, `ox-cx-watcher`,
`ox-runner`, `ox-ctl`, `ox-rt`) and runs
`cargo install --path <crate> --locked` on each. Re-run `make install`
after local changes to pick them up.

Other targets:

| Target           | What it does                                          |
|------------------|-------------------------------------------------------|
| `make build`     | `cargo build --workspace`                             |
| `make test`      | `cargo test --workspace`                              |
| `make fmt`       | `cargo fmt --all`                                     |
| `make fmt-check` | `cargo fmt --all -- --check`                          |
| `make clippy`    | `cargo clippy --workspace --all-targets -D warnings`  |
| `make check`     | `fmt-check` + `clippy` + `test` — run before commits  |
| `make uninstall` | `cargo uninstall` for each binary                     |
| `make clean`     | `cargo clean`                                         |

`make check` is the one-shot gate — if it's green, you're ready to
commit.

## workspace layout

| Crate            | Role                                                    |
|------------------|---------------------------------------------------------|
| `ox-core`        | Shared types, storage, event log (library only)         |
| `ox-server`      | HTTP + SSE API, trigger poller, SQLite event store      |
| `ox-herder`      | Runner pool manager — keeps N runners alive             |
| `ox-cx-watcher`  | Watches `cx log`, ingests cx node state into ox-server  |
| `ox-runner`      | Step executor — spawns step processes, proxies APIs     |
| `ox-ctl`         | Operator CLI (`ox-ctl up/down/status/events/…`)         |
| `ox-rt`          | In-step helper (`ox-rt done`, `metric`, `artifact`)     |

Shipped defaults (`defaults/workflows`, `defaults/runtimes`,
`defaults/personas`) are embedded into the binaries at build time and
extracted to `~/.ox/defaults/` on first run.

## dev loop

ox follows a test-first discipline. For every change:

1. **Red** — write a failing test for the next slice of behavior.
   Commit with a message starting `red — …`.
2. **Green** — write the minimum production code to turn the test
   green. Commit with `green — …`.
3. **Verify** — `make check` clean before pushing.

Prefer **vertical slices** over horizontal layers: each commit should
leave a complete thin capability working end-to-end, not one layer of
many features. A task with five slices should produce five red/green
pairs, not one giant red and one giant green.

Debug with tests, not inline `println!`s — assert what you expect and
let the failure tell you where reality diverges.

## running the ensemble for development

For day-to-day development the same `ox-ctl up` the README describes is
usually what you want — it picks up whatever's in `~/.cargo/bin` from
your last `make install`.

If you need to run pieces by hand (attaching a debugger, poking at one
process in isolation), the README's
[running the ensemble by hand](README.md#running-the-ensemble-by-hand)
section has the raw invocations.

## filing a PR

1. Branch off `main`.
2. `make check` clean.
3. Push and open a PR against `integral-dragon/oxv2`.
4. Keep PRs small and focused — one vertical slice per PR where
   possible.
