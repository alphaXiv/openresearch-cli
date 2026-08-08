# OpenResearch CLI (`orx`)

> [!IMPORTANT]
> If you are an OpenResearch user or someone who is interested in autoresearch,
> we'd love to chat with you. Please email
> [contact@alphaxiv.org](mailto:contact@alphaxiv.org) if interested.

### Stay updated or use OpenResearch compute

To receive email updates about the OpenResearch CLI or spin up compute from
OpenResearch, create an account at [openresearch.sh](https://openresearch.sh).

### Run autoresearch on your machine

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
- **Runs** — every backend receives the same immutable archive of the recorded
  Git commit. Modal, Hugging Face Jobs, Kubernetes, Slurm, SSH, Ray,
  OpenResearch, and local runs do not require a hosted repository or push.
- **Autoresearch** — describe a goal and let the agent run autonomously toward
  it: proposing, launching, and analyzing experiments.

Everything binds to loopback only. Creating a local project and launching
compute do not publish code; upstream repositories and paper search use the
network only when you choose those import flows.

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
| Literature | `lit`, `paper` (full-text search across alphaXiv, OpenAlex, bioRxiv — no login required) |
| Agent integration | `install-skills`, `skill` |
| Maintenance | `version`, `update`, `telemetry`, `delete database/cli/all` |

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

`orx` sends usage analytics linked to a random installation ID—not an account—to
help prioritize features. It's opt-out, and the `orx up` onboarding surfaces the
choice on first run.

- **Collected:** command name, a random per-install UUID, CLI version, OS/arch,
  the official build channel, a CI flag, coarse install type, and coarse event
  labels (e.g. onboarding completed, project created, chat session started, or
  a run launched on `modal`). When onboarding is completed, the disclosed
  research profile is also sent unfiltered: selected research areas, the
  Other-area description, research background, and representative paper IDs
  and titles.
- **Not automatically added:** code, prompts, file contents or paths, project or
  experiment IDs/names, repo names, tokens, emails, or account identifiers.
  Anything entered in the onboarding profile is sent exactly as submitted and
  may contain identifying information. The random install UUID is not tied to
  your account.

```sh
orx telemetry off        # persistent, per-machine
orx telemetry status     # current state + the random install id
orx <cmd> --no-telemetry # per-run
```

Only official prebuilt release artifacts can send usage analytics. Source,
worktree, `cargo install --path`, and cargo-dist PR/dry-run builds remain off and
do not create an installation ID. `ORX_TELEMETRY_ENV=off` additionally disables
analytics in an official binary; it cannot enable analytics in a source build.

Events are fire-and-forget on a background task and never block a command.
