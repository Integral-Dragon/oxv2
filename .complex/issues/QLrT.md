Add to defaults/runtimes/claude.toml:

[[runtime.failure_signals]]
name = "auth_failed"
pattern = '"is_error":true.*"error":"authentication_error"|Invalid authentication credentials'
retriable = false

Manual repro per parent task:
1. seguro sandbox with invalid ~/.claude/.credentials.json
2. kick off code-task execution
3. assert: 1 attempt, escalates immediately with signal:auth_failed (not 4 retries with exited_silent)