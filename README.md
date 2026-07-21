# OpenResearch CLI (`orx`)

**Run autoresearch on your machine.**

- **Run research agents in parallel**. Spins up agents in different worktrees
  so you can investigate several different directions at once.
- **Works with Claude Code, Codex, and OpenCode**
- **Bring your own compute**. Works with SSH, Slurm, Kubernetes, Modal,
  HuggingFace and more.
- **Give it a goal**. Can run the entire autoresearch loop from literature
  review to experiment analysis.
- **Local and private**. Your code and your data stays on your machine.

https://github.com/user-attachments/assets/33b62182-0795-490d-9366-0fb0b4bd49fd

## Quick start

```sh
curl -LsSf https://openresearch.sh/install.sh | sh
orx up
```

The dashboard opens at `http://127.0.0.1:4791`. Give the agent a goal — for
example, ask it to reproduce a paper:

```
/reproduce-paper <paper URL or title> on <compute>
```

or turn one into an interactive marimo notebook:

```
/paper-to-marimo <paper URL or title> on <compute>
```

## The dashboard

`orx up` runs a single local process on `127.0.0.1` — an embedded web UI plus a
JSON/SSE API over a local SQLite store. From there you get:

- **Agent chat** — a research assistant with full project context, backed by
  your locally installed harness: Claude Code, Codex, or OpenCode (pick the
  harness and model in the UI). Ask it to analyze runs, dig into results, edit
  code, and spin up new experiments.
- **The experiment tree** — every experiment is a git branch: a runnable
  snapshot of your code. The root is your baseline; children are variants
  measured against it, so lineage stays explicit.
- **Runs** — launch on your compute (Modal, Hugging Face Jobs, Kubernetes,
  Slurm, any SSH box, your own machine) or OpenResearch managed GPUs, and watch
  live logs, statuses, git diffs, files, and linked W&B runs stream in.
- **Autoresearch** — describe a goal and let the agent run autonomously toward
  it: proposing, launching, and analyzing experiments.

Everything binds to loopback only; nothing on the dashboard's paths leaves your
machine except the compute and paper-search calls you initiate.

### On a remote machine

Develop from your laptop while the dashboard runs next to your GPUs:

```sh
orx up --remote user@host        # or an ~/.ssh/config alias; append :PORT for a custom SSH port
```

This starts `orx up` on the remote box over SSH, tunnels the port back, and
opens your browser locally. Note the remote server is unauthenticated on that
host's loopback, so other users on the same box can reach it.

## Commands

Run `orx --help` (or `orx <command> --help`) for full usage. The highlights:

| Area | Commands |
|---|---|
| Dashboard | `up` |
| Auth | `login`, `logout` |
| Projects | `projects`, `explore`, `project`, `create-project`, `env` |
| Experiments | `experiments`, `create-experiment`, `exp status/cmd/run/cancel` |
| Runs & evidence | `runs`, `logs`, `search-logs`, `artifacts`, `artifact`, `wandb`, `query`, `chart`, `report` |
| Compute | `compute`, `instance create` |
| Literature | `lit`, `paper` (alphaXiv full-text search — no login required) |
| Agent integration | `install-skills`, `skill` |
| Maintenance | `version`, `update`, `telemetry` |

`orx install-skills` drops the OpenResearch skill into your local coding agents
(Claude Code, Codex, OpenCode, Cursor) so they can drive `orx` themselves —
`orx login` offers this too.

## Installing

The install script above fetches the latest prebuilt release (macOS and Linux,
x86_64 and arm64) and is the same as:

```sh
curl -LsSf https://github.com/alphaXiv/openresearch-cli/releases/latest/download/openresearch-cli-installer.sh | sh
```

`orx update` keeps script-installed binaries current; interactive terminals
also get a once-a-day background check with a one-line stderr notice (silence
it with `ORX_NO_UPDATE_CHECK=1`).

### From source

Requires Rust (stable) via [rustup](https://rustup.rs). The prebuilt dashboard
UI is committed at `ui/dist`, so a plain build works:

```sh
cargo build --release          # binary at target/release/orx
cargo install --path .         # or install onto your PATH (~/.cargo/bin)
```

To hack on the dashboard UI itself (Vite + React, embedded into the binary at
build time):

```sh
cd ui && pnpm install && pnpm build
```

Run the tests with `cargo test`.

## Configuration

- **API URL** — defaults to production (`https://api.openresearch.sh`);
  override with `--api-url` or `OPENRESEARCH_API_URL`.
- **Credentials** — `orx login` opens your browser, mints a personal access
  token, and stores it at `${XDG_CONFIG_HOME:-~/.config}/openresearch/credentials.json`
  (mode `0600`). Sent as `Authorization: Bearer …` on every request.

## Usage analytics

`orx` sends **anonymous** usage analytics to help prioritize features. It's
opt-out, and the `orx up` onboarding surfaces the choice on first run.

- **Collected:** command name, a random per-install UUID, CLI version, OS/arch,
  a CI flag, and coarse event labels (e.g. "a run launched on `modal`").
- **Never collected:** code, prompts, file contents or paths, project or
  experiment ids/names, repo names, tokens, emails — nothing identifying. The
  install UUID is not tied to your account.

```sh
orx telemetry off        # persistent, per-machine
orx telemetry status     # current state + the anonymous install id
orx <cmd> --no-telemetry # per-run
```

Events are fire-and-forget on a background task and never block a command.
