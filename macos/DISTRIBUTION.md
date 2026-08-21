# Distributing OpenResearch.app

`scripts/build-macos-app.sh` builds the app; `scripts/package-macos-app.sh`
signs, notarizes, and packages it into a DMG. CI
(`.github/workflows/release-macos-app.yml`) runs both after a release (see the
trigger caveat below) and attaches `OpenResearch.dmg`:

```
https://github.com/alphaXiv/openresearch-cli/releases/latest/download/OpenResearch.dmg
```

The release job is a no-op until the **repository** variable
`MACOS_SIGNING_ENABLED` is `true` (a repo variable, not an environment one — the
cheap gate job has no `environment:` and only sees repo/org variables).

The attach runs automatically only when the `Release` workflow was dispatched by
a PAT (see below); GitHub suppresses the `workflow_run` cascade for runs authored
by `GITHUB_TOKEN`. To attach a DMG to a release that missed it, run the **Attach
macOS app to release** workflow manually (Actions → Run workflow → enter the tag,
e.g. `v0.1.99`), or:

```bash
gh workflow run release-macos-app.yml -f tag=v0.1.99
```

The manual path checks out `inputs.tag` and runs the signing scripts under the
`release-signing` environment, so the **required reviewer approving the run is
the real gate on the certificate** — verify the tag points at trusted code (the
`main`-only deployment-branch restriction covers the workflow file, not the
checked-out tag).

## The app updater's contract

Installed apps update themselves from `macos-app.json`, uploaded to the release
by the same step that uploads the DMG:

```json
{ "version": "0.1.104", "tag": "v0.1.104", "asset": "OpenResearch.dmg", "sha256": "…" }
```

Two rules, both load-bearing:

- **Never publish the manifest without its DMG, or ahead of it.** The updater
  treats the manifest as proof the app build exists; a manifest whose asset is
  missing points every installed app at a 404, and one that outlives a re-run's
  new DMG fails every checksum. Hence the upload order in the workflow: delete
  the old manifest, upload the DMG, then publish the new manifest. A *missing*
  manifest is fine — the updater reads a 404 as "nothing to update to" and
  retries later, which is what happens between a release being published and
  this workflow attaching its DMG.
- **The app's version comes from this file, not the CLI's `dist-manifest.json`.**
  The DMG is attached after the release, so the CLI's version can be ahead of the
  published app build. An app install that read the CLI's manifest would
  advertise, and endlessly retry, a build that does not exist for it — so
  `updates::fetch_latest_for_channel` picks the manifest by install channel.

The digest is published here because the release's own `sha256.sum` is generated
by cargo-dist before this job runs and does not cover the DMG.

Before swapping the bundle, `src/updates/macos_app.rs` checks the digest, then
requires `codesign` against a Developer ID requirement pinned to team
`9P69UXUJUK` *and* an `spctl` verdict of `source=Notarized Developer ID`. The
signature check — not the digest — is what makes an unattended swap safe, so
**changing the signing identity breaks self-update for every installed app**:
update `EXPECTED_TEAM_ID` and ship that release before retiring the old cert.

## Configure signing (CI)

Needs an Apple Developer Program account with a **Developer ID Application**
certificate (see Apple's [notarizing docs](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)).
From it you produce the six values below.

1. Create the **`release-signing` environment** (Settings → Environments): add
   **required reviewers** and set **Deployment branches → `main`**. Add these as
   **environment** secrets (not repo-wide):

   | Secret | Value |
   | --- | --- |
   | `MACOS_CERT_P12_BASE64` | `base64 -i cert.p12` of the exported Developer ID cert |
   | `MACOS_CERT_PASSWORD` | the `.p12` export password |
   | `MACOS_SIGN_IDENTITY` | `Developer ID Application: <name> (TEAMID)` — `security find-identity -v -p codesigning` |
   | `MACOS_NOTARY_APPLE_ID` | your Apple ID email |
   | `MACOS_NOTARY_TEAM_ID` | your Team ID |
   | `MACOS_NOTARY_PASSWORD` | an app-specific password (account.apple.com) |

2. Set repo **variable** `MACOS_SIGNING_ENABLED = true` to switch the pipeline on.

3. Add repo **secret** `RELEASE_DISPATCH_TOKEN` — a fine-grained PAT scoped to
   this repo with **Actions: read and write** (classic: `repo` + `workflow`).
   `release-on-bump.yml` dispatches `release.yml` with it so the Release run is
   owned by a real token and its completion cascades to the macOS attach. If it's
   missing or invalid, releases still ship but the DMG is a manual dispatch.
   Fine-grained PATs expire (≤1yr) — rotate it before then, or releases keep
   shipping without the auto-attach until it's renewed.

Also enable **Require a pull request** + **Require review from Code Owners** on
`main` (see `.github/CODEOWNERS`) so the signing scripts can't change unreviewed.
Never commit the `.p12`.

`package-macos-app.sh` signs with `macos/entitlements.plist`, which grants the
Apple-events entitlement the Dock-click tab focus needs. Without it that path is
denied in signed builds only — unsigned local bundles never exercise the check —
and the first Dock click prompts once for Automation access.

## Build / sign locally

```bash
rustup target add aarch64-apple-darwin x86_64-apple-darwin   # once, for universal
ORX_APP_UNIVERSAL=1 bash scripts/build-macos-app.sh
xcrun notarytool store-credentials orx-notary \
  --apple-id you@example.com --team-id TEAMID --password <app-specific-password>
MACOS_SIGN_IDENTITY="Developer ID Application: <name> (TEAMID)" \
  MACOS_NOTARY_PROFILE=orx-notary bash scripts/package-macos-app.sh
```

Without the two `MACOS_*` vars, `package-macos-app.sh` still makes an **unsigned**
`dist/OpenResearch.dmg` for quick local testing.

## DMG installer window

`package-macos-app.sh` builds a styled DMG: opening it shows `OpenResearch.app`
and an `/Applications` alias to drag it onto, over a branded background.

- The background is generated by `scripts/generate-dmg-background.mjs` (pure Node,
  no image deps — same approach as `generate-icon.mjs`), at 1x + 2x combined into
  a HiDPI `background.tiff`.
- The window layout (size, icon positions, background) is applied by Finder via
  AppleScript on a temporary read-write image, then flattened into the compressed
  DMG. This needs a Finder session, so it runs on the macOS CI runner but not in a
  headless shell; if it fails, a functional but unstyled DMG still ships. The
  window/icon constants live at the top of the "styled DMG" block, and the
  matching canvas size lives in `generate-dmg-background.mjs`.

## App-mode runtime environment

Finder launches the bundle through launchd, so the process starts with
`PATH=/usr/bin:/bin:/usr/sbin:/sbin` and no shell rc ever sourced. Detection
would then find no `codex` at all, and `claude`/`opencode` only at their default
installer drop locations — the "works in my terminal, broken in the app" bug.

`commands::app::hydrate_shell_env` therefore probes the user's shell
(`$SHELL -ilc`, interactive because `.zshrc` is where these exports live) once at
startup and installs the result via `local::shell_env`, which harness lookup,
harness children, and directory resolution consult instead of the process
environment. It is best-effort and capped at 5s; every outcome is logged. To see
it, run the bundled binary from a terminal:

```bash
/Applications/OpenResearch.app/Contents/MacOS/OpenResearch
```

## Coexisting with a CLI install

The app and a `curl`-installed `orx` share one data dir and one config dir, so
both must be safe to have at once:

- **Ports** — the app binds an ephemeral loopback port rather than `orx up`'s
  4791.
- **Store** — SQLite in WAL with a 5s busy timeout; concurrent readers/writers
  are expected. Run supervisors hold a per-run exclusive lock, so a second
  server recovering the same active run exits instead of double-driving it.
- **Lifecycle lock** — app mode takes the same read lock `dispatch` takes for
  `orx up`, so `orx delete` refuses to wipe the store under a running app.
- **`orx` on the agent's PATH** — the bundle ships `Contents/MacOS/orx`, a
  symlink to the executable, and `chat::prepare_env` prepends that directory.
  Agents shelling out to `orx` therefore get *this* build rather than whatever
  CLI version happens to be installed, and a DMG-only user needs no CLI at all.
  Invoked under that name the binary stays a plain CLI — see
  `launched_as_app_bundle`.

- **Directories** — `ORX_DATA_DIR`, `XDG_DATA_HOME`, and `XDG_CONFIG_HOME` are
  imported by the same startup probe (`local::shell_env::IMPORTED`), so a rc
  file that redirects the store moves the app with it. Otherwise the app would
  read the default database while the CLI read the user's, and the lock above
  would guard a file neither shares.
- **Claude configuration** — `CLAUDE_CONFIG_DIR` is imported by that probe too,
  so account metadata, installed skills, and spawned Claude processes all use
  the same profile as the user's terminal.
