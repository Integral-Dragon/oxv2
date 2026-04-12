Red: snapshot test of exec list output with all origin variants; clap rejects --task; GET /api/executions?task=X returns 400 or ignores.

Green minimum:
- Delete task field from ExecCommands::List (main.rs:88)
- Drop _task param through call stack (main.rs:179, 243)
- Delete task from ListExecutionsQuery + filter line in api.rs
- format_origin helper with 24-char ellipsis
- ORIGIN column in table rendering (main.rs:256–291)
- origin in execution_summary JSON and ExecutionDetail
- cmd_exec_show renders origin