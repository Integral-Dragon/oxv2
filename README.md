# ox

GitHub for agents. An event-sourced workflow engine that runs multi-agent
teams in isolated sandboxes, coordinated by a human. You play the repo
owner — setting direction, reviewing escalations, controlling budget —
while agents handle contribution.

See [the site](https://integral-dragon.github.io/oxv2/) and
[`docs/prd/`](docs/prd/README.md) for the full pitch.

## getting started

ox is **Linux only**. Runners boot inside QEMU/KVM virtual machines
through [seguro](https://github.com/dragon-panic/seguro), which assumes a
Linux host with KVM available.

### 1. install rust

If you don't already have a Rust toolchain:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

Make sure `~/.cargo/bin` is on your `PATH`.

### 2. install seguro and cx

ox depends on two sister projects from
[github.com/dragon-panic](https://github.com/dragon-panic):

- **[seguro](https://github.com/dragon-panic/seguro)** — the VM sandbox
  runners execute inside. Agents only see their cloned workspace and a
  read-only mount of the ox binaries. Never the host filesystem.
- **[complex](https://github.com/dragon-panic/complex)** (`cx`) — the task
  tracker. ox watches `cx log` and triggers workflows when nodes tagged
  `workflow:code-task` become `ready`. cx state lives in git, so the
  event source is `git log`.

cx is a simple cargo install:

```bash
cargo install --git https://github.com/dragon-panic/complex
```

seguro needs more setup — host packages, KVM access, and a base VM image
that gets built once from a script in the repo. The short version on
Arch:

```bash
sudo pacman -S qemu-full virtiofsd dosfstools mtools openssh
sudo usermod -aG kvm $USER     # log out / in for this to take effect

git clone https://github.com/dragon-panic/seguro
cd seguro
cargo install --path .                # installs `seguro` to ~/.cargo/bin
./scripts/build-image.sh              # downloads Ubuntu 24.04, builds base.qcow2 (~500 MB)
cd ..
```

For Debian/Ubuntu host packages and the full security-model rundown, see
the [seguro README](https://github.com/dragon-panic/seguro). `ox-ctl up`
launches runners with `--net dev-bridge --unsafe-dev-bridge` so they can
reach `ox-server` on the host.

### 3. install claude code and log in

Runners execute Claude Code inside the VM, but the credentials come from
the host. You need Node.js for `npm` first — on Arch:

```bash
sudo pacman -S nodejs npm
```

Then install Claude Code and log in once on the host:

```bash
npm install -g @anthropic-ai/claude-code
claude
# inside the prompt:
/login
```

`/login` writes `~/.claude/.credentials.json`. `ox-ctl up` reads that
file on start and seeds it as the `claude_credentials` secret. Without
it, any step using the `claude` runtime will fail.

**Optional — OpenAI Codex.** If you want to use the `codex` runtime,
install and log in to the Codex CLI the same way:

```bash
npm install -g @openai/codex
codex login
```

`codex login` opens a browser for the ChatGPT OAuth flow and writes
`~/.codex/auth.json`. `ox-ctl up` reads that file on start and seeds it
as the `codex_auth` secret. Skip this if you only use the claude
runtime.

### 4. install ox

```bash
cargo install --git https://github.com/integral-dragon/oxv2
```

That installs all five binaries (`ox-server`, `ox-herder`, `ox-runner`,
`ox-ctl`, `ox-rt`) into `~/.cargo/bin`. The shipped defaults for
workflows, runtimes, and personas are baked into the binaries and
extracted to `~/.ox/defaults/` (read-only) on first run.

Building ox from source? See [CONTRIBUTING.md](CONTRIBUTING.md).

### 5. start ox in a project

From any project directory:

```bash
cd ~/projects/my-project
ox-ctl up
```

You should see something like:

```
starting ox for my-project (repo=/home/you/projects/my-project, port=4840)

  server    pid=12345  port=4840
  secrets   claude_credentials seeded
  herder    pid=12346
  runner-1  pid=12347  (seguro) workspace=.../.ox/run/runner-1
  runner-2  pid=12348  (seguro) workspace=.../.ox/run/runner-2
```

`ox-ctl up` spawns `ox-server` and `ox-herder` on the host, launches
seguro VMs as runners (`--runners N` to change the count, default 2),
seeds `claude_credentials` from `~/.claude/.credentials.json`, and
points everything at the current repo.

Everything ox writes for the project lives under `.ox/run/` in the repo
(`ox.db`, per-process logs, runner workspaces, pidfile). Runners reach
the server on the host via the QEMU user-mode gateway (`10.0.2.2`) —
`ox-ctl up` wires that up automatically.

Other commands:

```bash
ox-ctl status         # pidfile state + server pool/exec summary
ox-ctl events         # tail the SSE event stream
ox-ctl down           # stop the ensemble
ox-ctl reset          # wipe the SQLite db and logs (must be down first)
```

Flags on `ox-ctl up`:

| Flag            | Default  | Purpose                               |
|-----------------|----------|---------------------------------------|
| `--runners N`   | `2`      | Number of seguro runners to launch    |
| `--port N`      | `4840`   | Host port for ox-server               |

Both also read from `OX_RUNNERS` / `OX_PORT` env vars.

### 6. file a task and watch it run

ox doesn't poll for new git commits — it watches the `cx` graph. To
trigger a workflow, file a node in cx, tag it, surface it to `ready`,
and commit. The server picks it up on the next 10-second tick.

First, if your project doesn't have one yet, initialize cx and create a
root node:

```bash
cx init
cx add "my-project"
```

Then file an issue under that root, tagged for the built-in `code-task`
workflow:

```bash
# replace <root-id> with the id from `cx add` (or `cx tree`)
cx new <root-id> "build a website worthy of oxen" \
    --tag workflow:code-task

# promote it from latent to ready
cx surface <new-node-id>

# commit so it shows up in `cx log` (which ox is watching)
git add .complex
git commit -m "task: build a website worthy of oxen"
```

Within ten seconds the server's poller sees the new ready node, the
matching trigger fires, and an execution lands on an idle runner. Watch
it happen:

```bash
ox-ctl events                        # stream events as they happen
ox-ctl exec list                     # list executions
ox-ctl exec show <id>                # drill into one
ox-ctl exec logs <id> propose -f     # follow the agent's step log
```

That's the loop: file a task, surface it, commit, walk away.

## project layout

ox loads workflows, runtimes, and personas from a config search path. For
a project, drop a `.ox/` directory at the repo root:

```
your-project/
  .ox/
    config.toml          # optional; lists trigger files
    workflows/           # workflow definitions (TOML)
      code-task.toml
    runtimes/            # runtime definitions (TOML)
      claude.toml
    personas/            # persona files copied into step workspaces
      software-engineer.md
  src/
  ...
```

Search order (first match wins):

1. `{repo}/.ox/` — the repo the server is pointed at
2. Each directory in `$OX_HOME` (colon-separated, left to right)
3. `~/.ox/defaults/` — extracted from the binary on first run, locked
   read-only. Re-extracts automatically on upgrade. You can read these
   to learn the format, but override by copying into `{repo}/.ox/`.

The shipped defaults give you three runtimes (`claude`, `codex`,
`shell`) and one workflow (`code-task`), which is enough to start.

## ox-ctl

```bash
ox-ctl up                             # start local ensemble (see above)
ox-ctl down                           # stop it
ox-ctl reset                          # wipe db and logs
ox-ctl status                         # pidfile + server health + pool

ox-ctl workflows                      # list loaded workflows
ox-ctl runners list                   # show registered runners

ox-ctl exec list                      # list executions
ox-ctl exec show aJuO-e1              # execution detail
ox-ctl exec cancel aJuO-e1            # cancel an execution
ox-ctl exec logs aJuO-e1 propose -f   # follow step logs

ox-ctl events                         # stream all events (SSE)
ox-ctl events --type step.done        # filter by event type
ox-ctl events --since 42              # replay from sequence 42

ox-ctl trigger node-123               # evaluate triggers for a cx node

ox-ctl secrets list
ox-ctl secrets set openai_api_key --value sk-...
ox-ctl secrets delete old_key
```

Add `--json` for machine-readable output. `--server <URL>` targets a
non-local server (default: `http://localhost:4840`).

## writing workflows

Workflows are TOML files in `.ox/workflows/`. A workflow is a list of
steps with transitions between them.

```toml
[workflow]
name        = "my-workflow"
description = "a simple two-step workflow"

[[step]]
name   = "generate"
output = "diff"

[step.workspace]
git_clone = true
branch    = "{task_id}"
push      = true

[step.runtime]
type    = "claude"
model   = "sonnet"
persona = "software-engineer"
prompt  = "Implement the feature described in the task."

[[step.transition]]
match = "pass"
goto  = "verify"

[[step.transition]]
match = "fail"
goto  = "generate"           # retry

[[step]]
name        = "verify"
output      = "verdict"
max_retries = 2

[step.runtime]
type   = "shell"
prompt = "cd {workspace} && cargo test && ox-rt done pass"

[[step.transition]]
match = "pass"
goto  = "merge"

[[step.transition]]
match = "*"
goto  = "generate"

[[step]]
name   = "merge"
action = "merge_to_main"

[step.workspace]
branch = "{task_id}"
```

### step fields

| Field             | Description                                            |
|-------------------|--------------------------------------------------------|
| `name`            | Unique step identifier                                 |
| `output`          | What this step produces (`diff`, `verdict`, etc.)      |
| `max_retries`     | Times to retry on failure (default: 3)                 |
| `max_visits`      | Max times the workflow can enter this step             |
| `max_visits_goto` | Step to jump to when max_visits exceeded               |
| `action`          | Built-in action instead of a runtime (`merge_to_main`) |
| `on_fail`         | Step to jump to on failure                             |

### workspace provisioning

```toml
[step.workspace]
git_clone = true          # full clone — origin/main always available
branch    = "{task_id}"   # create or checkout this branch
push      = true          # push changes after step completes
read_only = false
```

### transitions

Transitions match on the step's output string. Matches are checked in
order; `*` is a catch-all. A prefix match uses `:` (e.g. `"fail:"`
matches `"fail:lint"`).

### triggers

Triggers live in separate files (not inside workflow definitions) and
are loaded via `config.toml`. This lets you reuse template workflows
while defining your own routing.

```toml
# .ox/config.toml
triggers = [
    "workflows/triggers.toml",   # paths relative to this file's directory
]
heartbeat_grace = 60             # seconds
```

```toml
# .ox/workflows/triggers.toml
[[trigger]]
on       = "cx.task_ready"       # event type to watch
tag      = "workflow:code-task"  # optional tag filter
workflow = "code-task"           # workflow to execute
```

Trigger files are additive across the search path. If no `config.toml`
exists, ox looks for `workflows/triggers.toml` in each search-path
directory as a default.

## writing runtimes

Runtimes define how a step process is spawned. They live in
`.ox/runtimes/`.

```toml
[runtime]
name = "my-runtime"

[runtime.vars]
prompt = { type = "string", required = false, default = "" }
model  = { type = "string", required = false }

[runtime.command]
cmd = ["my-tool", "--prompt", "{tmp_dir}/ox-prompt"]
interactive_cmd = ["my-tool", "--interactive"]

[[runtime.command.optional]]
when = "model"
args = ["--model", "{var.model}"]

[runtime.env]
MY_API_KEY = "{secret.my_api_key}"
```

**Files** — place files before execution:

```toml
[[runtime.files]]
content = "{secret.claude_credentials}"
to      = "{home}/.claude/.credentials.json"

[[runtime.files]]
from = "{persona}"
to   = "{workspace}/config.md"
```

Path placeholders: `{workspace}` (git work dir), `{tmp_dir}` (outside
git), `{home}` (runner HOME). Bare relative paths go to `{tmp_dir}`.

**Persona + prompt assembly** — if a runtime declares a `persona` field
(type `file`), the persona content is loaded from `personas/` on the
search path and prepended to the step prompt. The combined content is
written to `{tmp_dir}/ox-prompt` and referenced via `{prompt_file}` in
the command.

**Proxy** — ox-runner can proxy API calls for metrics and rate limiting:

```toml
[[runtime.proxy]]
env      = "ANTHROPIC_BASE_URL"
provider = "anthropic"
target   = "https://api.anthropic.com"
```

**Metrics** — collected from proxied requests:

```toml
[[runtime.metrics]]
name   = "input_tokens"
type   = "counter"               # counter, histogram, or label
source = "proxy"
```

**Interactive (TTY) mode** — when a step sets `tty = true`, ox-runner
allocates a PTY and starts a TCP bridge. The bridge address is
advertised in the `step.running` event's `connect_addr` field. Connect
with any TCP client:

```
socat - TCP:<runner-host>:<port>
```

The runtime's `interactive_cmd` is used instead of `cmd`. PTY output is
teed to the step log. The unix socket (`$OX_SOCKET`) is available inside
the session for `ox-rt done` / metrics / artifacts.

A built-in `interactive` workflow provides a single-step interactive
shell session:

```bash
curl -s -X POST http://localhost:4840/api/executions \
  -H 'Content-Type: application/json' \
  -d '{"workflow":"interactive","vars":{"branch":"my-experiment"}}'
```

Then connect to the `connect_addr` from the `step.running` event.

## ox-rt: step-to-runner communication

Steps communicate back to the runner through `ox-rt`, a helper that
talks over a Unix socket (`$OX_SOCKET`, set automatically by the
runner).

```bash
ox-rt done pass                  # complete the step with output "pass"
ox-rt done "fail:lint errors"    # complete with a failure output
ox-rt metric input_tokens 14523  # report a metric
echo "content" | ox-rt artifact proposal   # stream artifact data
ox-rt artifact-done proposal     # close an artifact stream
```

## environment variables

| Variable           | Used by    | Description                                 |
|--------------------|------------|---------------------------------------------|
| `OX_SERVER`        | all        | Server URL (default: http://localhost:4840) |
| `OX_HOME`          | server     | Colon-separated config search path          |
| `OX_ENVIRONMENT`   | runner     | Environment label (default: local)          |
| `OX_WORKSPACE_DIR` | runner     | Base dir for step workspaces                |
| `OX_SOCKET`        | step procs | Unix socket for ox-rt (set by runner)       |
| `OX_EXECUTION_ID`  | step procs | Current execution ID (set by runner)        |
| `RUST_LOG`         | all        | Log level (default: info)                   |

## running the ensemble by hand

`ox-ctl up` is the recommended path. If you need to wire things up
manually — for debugging, or running on something that isn't seguro —
these are the moving parts:

```bash
# server (host) — first run extracts ~/.ox/defaults automatically
ox-server --port 4840 --repo /path/to/project --db /path/to/project/ox.db

# herder (host)
ox-herder --server http://localhost:4840

# runner(s) — run directly on the host or inside your own sandbox.
# If you're using seguro, the runner reaches the host via 10.0.2.2.
ox-runner --server http://10.0.2.2:4840 --environment seguro \
          --workspace-dir /tmp/ox-work

# seed Claude credentials
ox-ctl secrets set claude_credentials \
  --value "$(cat ~/.claude/.credentials.json)"
```

Server flags: `--port` (default `4840`), `--db` (default `ox.db` in the
repo), `--repo` (default cwd).

Herder flags: `--server`, `--pool-target` (default `2`),
`--heartbeat-grace`, `--tick-interval` (default `5s`).

Runner flags: `--server`, `--environment` (default `seguro`),
`--workspace-dir` (default `/tmp/ox-work`).
