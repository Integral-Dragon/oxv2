New module ox-runner/src/scan.rs:

- pub struct SignalMatch { name: String, line: String }
- pub fn scan_failure_signals(log_path: &Path, signals: &[CompiledFailureSignal]) -> Vec<SignalMatch>
  - seek End - tail_bytes, read forward, drop bytes to first valid UTF-8 boundary, scan
  - per signal: regex.find(&tail) → record name + matched line
  - missing/empty file → empty result (warn for missing)

Wire into ox-runner/src/runner.rs handle_step() around line 623 (post-exit, pre-existing-signal-collection):
- compile assignment-supplied raw patterns per step
- run scan, extend signals: Vec<String> with matched names
- attach signal_matches: Vec<SignalMatch> to step.signals event payload (parallel field)

Update ox-core/src/events.rs StepSignalsData with #[serde(default)] signal_matches: Vec<SignalMatch>.

Assignment payload from server must carry the raw failure_signals patterns (server already loaded + validated; runner re-compiles per step).

Red fixture-based tests:
- match found in tail
- no match
- partial UTF-8 at boundary
- missing log file
- empty log file
- file < tail_bytes (whole file scanned)
- file > tail_bytes (early-only match missed)
- multiple matches → one entry per signal name