# Plan

Let `ox-ctl up` pick up `runners` from `config.toml` so users don't have to type `--runners N` every invocation. Mirror the existing `heartbeat_grace` pattern.

## Slices

### Slice 1 — OxConfig gains `runners: usize`
`ox-core/src/config.rs`: add field with `#[serde(default = "default_runners")]` (default=2). `load_config` picks it up from the search path with first-wins merge, same as `heartbeat_grace`. Test: `config.toml` with `runners = 4` deserializes to `4`; default preserved when absent.

### Slice 2 — ox-ctl up resolves runners flag → env → config → default
`ox-ctl/src/main.rs` Up command: `runners: Option<usize>` (drop clap default). `cmd_up` loads OxConfig via `config::resolve_search_path(cwd)` + `config::load_config`, uses `flag.unwrap_or(config.runners)`. Pure helper `resolve_runners(flag, config_value)` for unit testing; wiring into cmd_up manual-verified.

## Out of scope
- Changing precedence of OX_RUNNERS env — clap still reads it before we fall back to config.
- Any broader config story (port, log dir, etc).
