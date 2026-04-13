Port bin/ox-rt to a new ox-rt/ crate in the cargo workspace. Same CLI surface, same Unix-socket protocol, same git preflight for `done`. No behavioral change for runtime users. Drops the python3+bash dependency from every runner VM.

Acceptance:
- New ox-rt/ crate produces target/debug/ox-rt
- Subcommands: done [--force] <output>, metric <name> <value>, artifact <name> [content], artifact-done <name>
- Reads OX_SOCKET env var; connects, sends newline-terminated message, reads response, errors if response starts with `error:`
- done preflight: refuses dirty tree, unpushed branch, HEAD ahead of origin, or branch with no commits ahead of origin/main — all skippable with --force
- bin/ox-rt deleted; ox-up/seguro shares target/debug/ so runners pick up the Rust binary automatically