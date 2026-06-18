# OpenResearch CLI (`orx`)

A small command-line interface for OpenResearch. Log in via the browser, then
query your projects from the terminal.

## Requirements

- Rust (stable) with Cargo. Install via [rustup](https://rustup.rs):

  ```sh
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

## Build & run

```sh
# Build (debug):
cargo build

# Run during development:
cargo run -- login
cargo run -- projects

# Build an optimized release binary at target/release/orx:
cargo build --release
./target/release/orx login

# Or install `orx` onto your PATH (~/.cargo/bin):
cargo install --path .
orx login
orx projects
```

Run the tests with `cargo test`.

### Commands

| Command | Description |
|---|---|
| `orx login [--api-url <url>]` | Opens your browser, authenticates, and stores a personal access token. |
| `orx logout` | Removes the stored token. |
| `orx install-skills [--agent claude\|codex\|opencode\|cursor\|all]` | Installs the `orx` skill shim into local coding agents (Claude Code, Codex, OpenCode, Cursor) so they auto-discover the CLI (`orx login` also offers this). |
| `orx projects [--all]` | Lists your projects, grouped by organization. `--all` includes archived. |
| `orx compute [--gpu <id>] [--count <n>] [--provider <name>] \| --cpu]` | Lists the GPU compute catalog across all providers and regions, or CPU-only offers with `--cpu`, sorted by price. |
| `orx instance create <orgId> (--gpu <id> [--count <n>] [--disk <gb>] [--provider <name>] \| --cpu <flavor> [--vcpus <n>])` | Spins up a standalone instance in an organization (not tied to an experiment), like the dashboard's "Spin up". |
| `orx exp status <expId>` | Shows an experiment's status, branch, parent, run command, and latest run, plus a local `git diff` recipe for what the run changed. |
| `orx exp cmd <expId> [--set <command>]` | Views or sets the experiment's run command. |
| `orx exp run <expId> (--gpu <id> [--count <n>] [--disk <gb>] [--provider <name>] \| --cpu <flavor> [--vcpus <n>] \| --sandbox <id>)` | Launches a run on new GPU, new CPU-only, or existing compute. |
| `orx exp cancel <expId>` | Cancels the in-flight run. |
| `orx lit "<query>" [--limit <n>] [--json]` | Full-text search alphaXiv's paper corpus (no login required). |
| `orx paper <id\|url> [--full]` | Fetch a paper's machine-readable report, or its full text with `--full`. |
| `orx version [--check] [--json]` | Prints the CLI version; `--check` compares it to the latest release. |
| `orx update [--dry-run] [--force]` | Updates orx by re-running the release installer (installer installs only). |

### Updating

`orx update` only manages binaries installed by the release installer script
(it checks the cargo-dist install receipt at
`${XDG_CONFIG_HOME:-~/.config}/openresearch-cli/`). If you installed with
`cargo install`, update the same way instead. In interactive terminals orx
also checks for new releases in the background at most once a day and prints
a one-line notice to stderr; set `ORX_NO_UPDATE_CHECK=1` (or
`OPENRESEARCH_CLI_DISABLE_UPDATE=1`) to turn that off. Agents, scripts, pipes,
and CI never see the notice.

### Configuration

- **API URL** resolves from `--api-url` → `OPENRESEARCH_API_URL` → the built-in
  default (`http://localhost:4000`). Point it at the production API host for
  real use:

  ```sh
  export OPENRESEARCH_API_URL=https://api.openresearch.sh
  ```

- **Credentials** are stored at
  `${XDG_CONFIG_HOME:-~/.config}/openresearch/credentials.json` (mode `0600`).

## How login works

`orx login` starts a temporary HTTP listener on a random `127.0.0.1` port, opens
`{api}/auth/cli/login` in your browser, and waits for the API to redirect back
with a freshly minted personal access token after you authenticate. The token is
sent as `Authorization: Bearer …` on every subsequent request.
