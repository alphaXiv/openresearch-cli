# OpenResearch CLI (`orx`)

A small command-line interface for OpenResearch. Log in via the browser, then
query your projects from the terminal.

## Requirements

- Node.js ≥ 24 (the CLI runs TypeScript directly via Node's native type
  stripping — no build step).

## Usage

```sh
# Run locally without installing:
node src/index.ts login
node src/index.ts projects

# Or link it so `orx` is on your PATH:
npm link
orx login
orx projects
```

### Commands

| Command | Description |
|---|---|
| `orx login [--api-url <url>]` | Opens your browser, authenticates, and stores a personal access token. |
| `orx logout` | Removes the stored token. |
| `orx projects [--all]` | Lists your projects, grouped by organization. `--all` includes archived. |

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
