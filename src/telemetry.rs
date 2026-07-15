//! Anonymous, opt-out usage analytics → PostHog.
//!
//! Why this exists: `orx` shipped with no telemetry, so we had no way to see
//! installs, DAU/WAU, retention, or which commands people actually use. This
//! module sends anonymous events (a random per-install UUID as the only
//! identity — never any PII, prompt text, file paths, ids, or repo contents).
//!
//! Guarantees, enforced by this module:
//! - **Opt-out and loud about it.** Honors `DO_NOT_TRACK`, `ORX_TELEMETRY`,
//!   `CI`, a `--no-telemetry` flag, and a persisted `orx telemetry off`. A
//!   disabled run sends nothing and generates no install id. (The sole disk
//!   touch on a disabled run is the one-time first-login notice recording that
//!   it was shown — local bookkeeping only, never a network call.)
//! - **Never blocks or crashes the CLI.** Sends are fire-and-forget on a
//!   background task with a bounded flush window (modeled on
//!   [`crate::updates::UpdateWarning`]); every error is swallowed. A telemetry
//!   fn returning to a command's `?` chain is impossible — they all return `()`.
//! - **musl-safe.** Reuses a rustls `reqwest` client; adds no TLS/C dependency.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;

/// Public, write-only PostHog project key (openresearch CLI project). A `phc_`
/// key can only *ingest* events — it cannot read data or change settings — so
/// it is safe to commit and ship in the binary, exactly as PostHog intends for
/// client-side keys. Do NOT put a personal (`phx_`) key here.
const POSTHOG_KEY: &str = "phc_u2i23xa8CBjcZpQprf6kdDzzR8vb2iTpRT8FmdcREBvX";

/// Prefix stamped onto EVERY event name. This PostHog project is shared with
/// the website/cloud-agent analytics, so CLI events must be separable by name
/// alone — not just by the `source: "cli"` property (which one forgotten
/// dashboard filter would leak past). Applied centrally in `build_payload`, so
/// every current and future CLI event is `cli_*` by construction; call sites
/// pass the bare name (`command`, `experiment_started`).
const EVENT_PREFIX: &str = "cli_";

/// US PostHog cloud. Overridable with `ORX_TELEMETRY_HOST` so tests can point
/// at a throwaway local listener instead of production.
fn posthog_host() -> String {
    std::env::var("ORX_TELEMETRY_HOST").unwrap_or_else(|_| "https://us.i.posthog.com".to_string())
}

/// Flush window granted to an in-flight send before `#[tokio::main]` tears the
/// runtime down (which cancels un-awaited spawned tasks). On an interactive
/// terminal we can afford the fuller window; on a pipe/CI/cron run we still
/// grant a small one rather than skipping — unlike the update checker (whose
/// dropped cache-warm is a harmless skipped optimization), a dropped telemetry
/// event is *lost data*, and much of this CLI's usage is non-interactive
/// (agents driving `orx` with redirected stderr). Skipping entirely would bias
/// DAU toward humans at a TTY. The bound stays small so scripted runs pay only
/// a tiny tail.
const FLUSH_GRACE_TTY: Duration = Duration::from_millis(250);
const FLUSH_GRACE_PIPE: Duration = Duration::from_millis(120);

/// The flush window appropriate to the current stderr (fuller on a TTY, small
/// on a pipe/CI). Single source of truth for the two constants.
fn flush_window() -> Duration {
    if std::io::stderr().is_terminal() {
        FLUSH_GRACE_TTY
    } else {
        FLUSH_GRACE_PIPE
    }
}

// ---------------------------------------------------------------------------
// Settings — install id + persisted opt-out, at config_dir()/settings.json
// ---------------------------------------------------------------------------

/// Machine-local CLI settings. Lives at `$XDG_CONFIG_HOME/openresearch/
/// settings.json` (the config dir, NOT the R2-snapshotted data dir) so the
/// anonymous install id stays per-install rather than travelling with an agent
/// box's data snapshot. Modeled on `K8sSettings`. Unknown fields on older files
/// parse fine and are dropped on the next save.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Settings {
    /// Random anonymous id (uuid v4), generated once on first enabled run.
    #[serde(default)]
    pub install_id: Option<String>,
    /// Set by `orx telemetry off`. `Some(true)` = user opted out persistently.
    #[serde(default)]
    pub telemetry_disabled: Option<bool>,
    /// Whether the one-time opt-out notice has been printed (after login).
    #[serde(default)]
    pub notice_shown: Option<bool>,
}

fn settings_path() -> PathBuf {
    crate::config::config_dir().join("settings.json")
}

/// How reading `settings.json` turned out. The corrupt case is kept distinct
/// from absent so a mutation refuses to clobber a file that might still hold a
/// persisted opt-out we just failed to parse (a torn write, a hand-edit) —
/// otherwise `orx telemetry off` could be silently undone by the next write.
enum SettingsState {
    /// File doesn't exist — never configured. Safe to create fresh.
    Absent,
    /// Parsed cleanly.
    Loaded(Settings),
    /// File exists but didn't parse. Treat as "present but unknown".
    Corrupt,
}

fn read_settings_state() -> SettingsState {
    match std::fs::read_to_string(settings_path()) {
        Err(_) => SettingsState::Absent,
        Ok(raw) => match serde_json::from_str::<Settings>(&raw) {
            Ok(s) => SettingsState::Loaded(s),
            Err(_) => SettingsState::Corrupt,
        },
    }
}

/// Loads settings, or `None` when the file is absent or unreadable/corrupt.
/// A corrupt file is treated as absent for *reads* — telemetry must never
/// surface a hard failure over its own bookkeeping. (Writes are more careful;
/// see `mutate_settings`.)
pub(crate) fn load_settings() -> Option<Settings> {
    match read_settings_state() {
        SettingsState::Loaded(s) => Some(s),
        _ => None,
    }
}

/// Atomically persist settings: write a uuid-named temp in the same dir, then
/// rename into place (atomic on the same filesystem). This is the pattern
/// `updates::write_check_cache` uses, and for the same reason — separate `orx`
/// processes can write `settings.json` concurrently (the in-process
/// `SETTINGS_LOCK` doesn't guard across PIDs), and a bare `fs::write` can leave
/// a torn file that then parses as corrupt.
fn write_settings(settings: &Settings) -> std::io::Result<()> {
    let path = settings_path();
    let Some(parent) = path.parent() else {
        return Err(std::io::Error::other("settings path has no parent"));
    };
    std::fs::create_dir_all(parent)?;
    let body = match serde_json::to_string_pretty(settings) {
        Ok(s) => format!("{s}\n"),
        Err(e) => return Err(std::io::Error::other(e)),
    };
    let tmp = parent.join(format!(".settings.json.{}.tmp", uuid::Uuid::new_v4()));
    if let Err(e) = std::fs::write(&tmp, body) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Serializes settings mutations *within this process*. Multiple concurrent
/// read-modify-write callers exist here — the background `cli_command` send
/// (which lazily generates the install id) and the `telemetry` subcommand
/// (which flips `telemetry_disabled`) can run at once. Without this, one
/// whole-object write clobbers the other's field. The mutex makes each mutation
/// re-read, patch only its own field, and write back, so updates merge.
static SETTINGS_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();

fn settings_lock_path() -> PathBuf {
    crate::config::config_dir().join("settings.lock")
}

/// Acquire the cross-process advisory lock guarding the settings RMW. Returns
/// the held guard (kept alive for the critical section), or `None` if the lock
/// file can't be opened/locked — in which case we proceed unlocked rather than
/// abandon the mutation (best-effort: the in-process mutex still holds, and a
/// lost cross-process update is strictly better than a dropped one). `flock` is
/// advisory and released when the fd closes at end of scope.
fn lock_settings_file() -> Option<fd_lock::RwLock<std::fs::File>> {
    let path = settings_lock_path();
    std::fs::create_dir_all(path.parent()?).ok()?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .ok()?;
    Some(fd_lock::RwLock::new(file))
}

/// Read-modify-write `settings.json`. `f` patches the current settings; the
/// result is persisted atomically (temp + rename).
///
/// Held across the whole read→modify→write: the in-process `SETTINGS_LOCK`
/// (serializes threads) AND a cross-process `flock` on `settings.lock`. The
/// flock is what prevents a *lost update* between two `orx` PROCESSES — e.g. a
/// concurrent `cli_command` install-id write silently reverting an `orx
/// telemetry off` after it reported success. The atomic rename alone only
/// prevents torn files, not lost updates; the flock closes that gap by making
/// each process's read+write mutually exclusive. (We take a *blocking* `write()`
/// to serialize contending writers, unlike `update.rs`'s `try_write()`, which
/// wants to reject a second concurrent updater — a different intent.) If the
/// file is corrupt the
/// mutation is REFUSED (writes nothing) so a persisted opt-out hiding in an
/// unparseable file is never silently dropped — the caller's change is lost,
/// but the user's privacy choice is preserved.
fn mutate_settings<F: FnOnce(&mut Settings)>(f: F) -> std::io::Result<()> {
    let _guard = SETTINGS_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Best-effort cross-process lock; held for the critical section below.
    // (`settings_flock` owns the file handle; `_flock` is the write guard that
    // borrows it — both must live to end of scope for the lock to hold.)
    let mut settings_flock = lock_settings_file();
    let _flock = settings_flock.as_mut().and_then(|l| l.write().ok());

    let mut settings = match read_settings_state() {
        SettingsState::Absent => Settings::default(),
        SettingsState::Loaded(s) => s,
        SettingsState::Corrupt => {
            return Err(std::io::Error::other(
                "settings.json is unreadable; refusing to overwrite",
            ));
        }
    };
    f(&mut settings);
    write_settings(&settings)
}

/// The anonymous `distinct_id`: an existing install id, or a freshly generated
/// one persisted on first use. Returns `None` only if the id can't be persisted
/// (so a run that couldn't write never invents a throwaway id that would inflate
/// install counts on every invocation).
fn install_id() -> Option<String> {
    // Fast path: already generated.
    if let Some(id) = load_settings().and_then(|s| s.install_id) {
        return Some(id);
    }
    // Generate + persist under the lock, re-checking inside in case a
    // concurrent caller won the race and already wrote one.
    let mut result = None;
    mutate_settings(|s| {
        if s.install_id.is_none() {
            s.install_id = Some(uuid::Uuid::new_v4().to_string());
        }
        result = s.install_id.clone();
    })
    .ok()?;
    result
}

// ---------------------------------------------------------------------------
// Opt-out decision
// ---------------------------------------------------------------------------

/// Why telemetry is off, for `orx telemetry status`. `None` = enabled.
pub(crate) enum DisabledReason {
    Flag,
    DoNotTrack,
    OrxTelemetry,
    Ci,
    Persisted,
    CorruptSettings,
}

impl DisabledReason {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            DisabledReason::Flag => "--no-telemetry flag",
            DisabledReason::DoNotTrack => "DO_NOT_TRACK is set",
            DisabledReason::OrxTelemetry => "ORX_TELEMETRY is off",
            DisabledReason::Ci => "CI is set",
            DisabledReason::Persisted => "disabled via `orx telemetry off`",
            DisabledReason::CorruptSettings => "settings file unreadable (failing safe)",
        }
    }
}

/// Resolves whether telemetry is disabled and why. Env/flag/CI are checked
/// before the persisted setting so a disabled run never reads or writes disk.
/// `cli_flag` is the `--no-telemetry` global flag.
pub(crate) fn disabled_reason(cli_flag: bool) -> Option<DisabledReason> {
    if cli_flag {
        return Some(DisabledReason::Flag);
    }
    // Presence (any value) opts out — the DO_NOT_TRACK convention.
    if std::env::var_os("DO_NOT_TRACK").is_some() {
        return Some(DisabledReason::DoNotTrack);
    }
    // Explicit off switch: 0 / off / false (case-insensitive).
    if let Ok(v) = std::env::var("ORX_TELEMETRY") {
        let v = v.trim().to_ascii_lowercase();
        if v == "0" || v == "off" || v == "false" {
            return Some(DisabledReason::OrxTelemetry);
        }
    }
    // Keep CI pipelines out of DAU/retention.
    if std::env::var_os("CI").is_some() {
        return Some(DisabledReason::Ci);
    }
    // Persisted state is checked last (the only branch that reads disk).
    match read_settings_state() {
        SettingsState::Loaded(s) if s.telemetry_disabled == Some(true) => {
            Some(DisabledReason::Persisted)
        }
        // A present-but-unreadable file might hold an opt-out we can't parse;
        // fail safe (disabled) rather than track someone who may have opted out.
        SettingsState::Corrupt => Some(DisabledReason::CorruptSettings),
        _ => None,
    }
}

/// Convenience: `true` when events should be sent.
fn is_enabled(cli_flag: bool) -> bool {
    disabled_reason(cli_flag).is_none()
}

/// Persist the opt-out flag (used by `orx telemetry on|off`). `true` writes
/// `telemetry_disabled = Some(true)`; `false` clears it. Goes through the same
/// lock as every other mutation so a concurrent install-id write can't clobber
/// it. Returns the io result so the command can report a write failure.
pub(crate) fn set_persisted_disabled(disabled: bool) -> std::io::Result<()> {
    mutate_settings(|s| {
        s.telemetry_disabled = if disabled { Some(true) } else { None };
    })
}

/// Process-global capture of the `--no-telemetry` flag, set once in `main`
/// before any command runs. Command modules fire events without having to
/// thread the global flag through their `run(args)` signatures — they read it
/// here, mirroring the codebase's "read global state at point of use" idiom
/// (cf. env-var reads scattered across modules).
static NO_TELEMETRY_FLAG: OnceLock<bool> = OnceLock::new();

/// Record the parsed `--no-telemetry` flag. Called once from `main`.
pub(crate) fn set_flag(no_telemetry: bool) {
    let _ = NO_TELEMETRY_FLAG.set(no_telemetry);
}

/// The recorded flag (defaults to `false` if `set_flag` was never called, e.g.
/// in unit tests).
fn flag() -> bool {
    *NO_TELEMETRY_FLAG.get().unwrap_or(&false)
}

// ---------------------------------------------------------------------------
// Event construction + send
// ---------------------------------------------------------------------------

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

fn is_ci() -> bool {
    std::env::var_os("CI").is_some()
}

/// Builds the PostHog capture payload for an event. Every event carries the
/// same base context (source, version, os, arch, ci) plus `$process_person_
/// profile: false` to keep it anonymous. `extra` supplies event-specific
/// properties — callers MUST keep these free of PII (coarse enums only).
fn build_payload(event: &str, distinct_id: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut props = json!({
        // Anonymous: don't build a person profile for this distinct_id.
        "$process_person_profile": false,
        "source": "cli",
        "cli_version": crate::updates::current_version().to_string(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "ci": is_ci(),
    });
    // Merge event-specific props FIRST so the invariant base context above can
    // never be silently overwritten by a caller's `extra` (defense in depth —
    // callers are expected to keep `extra` PII-free and disjoint, but a stray
    // `source`/`ci`/`os` key must not be able to corrupt identity fields).
    if let (Some(obj), Some(add)) = (props.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            obj.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    json!({
        "api_key": POSTHOG_KEY,
        // Every CLI event name is `cli_`-prefixed so it's separable from the
        // website's events in this shared project by name alone.
        "event": format!("{EVENT_PREFIX}{event}"),
        "distinct_id": distinct_id,
        // Client-side event time (UTC ISO-8601). Without this PostHog buckets on
        // ingestion time, which for a fire-and-forget send that may land seconds
        // late would smear DAU/retention — the metrics this feature exists for.
        "timestamp": iso8601_utc(crate::store::now_ms()),
        "properties": props,
    })
}

/// Format epoch milliseconds as a UTC ISO-8601 timestamp
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`). Pure civil-date math on the UTC timeline — no
/// timezone or DST involved — so no date crate is needed (the codebase has
/// none). Uses the standard days-from-civil algorithm.
fn iso8601_utc(ms: i64) -> String {
    let ms = ms.max(0);
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);

    // days-from-civil inverse (Howard Hinnant's algorithm), epoch 1970-01-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Fire one event, right now, on the current task, awaiting the send. All
/// errors are swallowed — telemetry never fails a command. Intended to be
/// wrapped in `tokio::spawn` by callers that don't want to wait.
async fn send(event: String, extra: serde_json::Value) {
    let Some(distinct_id) = install_id() else {
        return;
    };
    let payload = build_payload(&event, &distinct_id, extra);
    let url = format!("{}/i/v0/e/", posthog_host());
    // Bounded per-request timeout so a hung endpoint can't keep the background
    // task alive indefinitely.
    let _ = http()
        .post(&url)
        .timeout(Duration::from_secs(3))
        .json(&payload)
        .send()
        .await;
}

/// Spawn a fire-and-forget send, returning its handle (or `None` when disabled).
/// Not awaited here — callers hold the handle and optionally grant it a flush
/// window at shutdown. Reads the `--no-telemetry` flag from the process-global
/// set in `main`, so there is exactly one flag-access idiom.
fn spawn_event(
    event: impl Into<String>,
    extra: serde_json::Value,
) -> Option<tokio::task::JoinHandle<()>> {
    if !is_enabled(flag()) {
        return None;
    }
    Some(tokio::spawn(send(event.into(), extra)))
}

/// Handles of spawned event sends that haven't been flushed yet. Draining +
/// flushing happens once, at `TelemetrySession::finish` (i.e. process exit), so
/// firing an event never blocks the command's own user-facing output. A
/// key-event `capture` returns immediately; the send races the command's
/// remaining work and is given its landing window only at the end.
static PENDING: OnceLock<std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>> = OnceLock::new();

fn pending() -> &'static std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>> {
    PENDING.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Fire an event without blocking: spawn the send and register its handle for
/// the single exit-time flush. Crucially does NOT await the flush window here —
/// awaiting inline would delay the caller's next `println!` (the "✓ …" success
/// line) by up to the flush duration. Safe to call unconditionally — no-ops when
/// telemetry is disabled. Deliberately not `async` beyond the spawn: nothing to
/// await.
fn capture(event: impl Into<String>, extra: serde_json::Value) {
    if let Some(handle) = spawn_event(event, extra) {
        if let Ok(mut v) = pending().lock() {
            v.push(handle);
        }
    }
}

/// Flush every pending event send (the session's `cli_command` plus any key
/// events fired during the command) within ONE shared window — the sends run
/// concurrently, so total tail latency is bounded by a single `FLUSH_GRACE`, not
/// N×. Called once from `TelemetrySession::finish`.
async fn flush_pending() {
    let handles: Vec<_> = match pending().lock() {
        Ok(mut v) => std::mem::take(&mut *v),
        Err(_) => return,
    };
    if handles.is_empty() {
        return;
    }
    // Await all handles together under one deadline. `join_all` completes when
    // every send finishes; the timeout caps the wait if any is slow.
    let _ = tokio::time::timeout(flush_window(), futures::future::join_all(handles)).await;
}

// ---------------------------------------------------------------------------
// Per-invocation session — the DAU/retention/command-usage backbone
// ---------------------------------------------------------------------------

/// Fires a `cli_command` event at the start of a run (so commands that
/// `std::process::exit` on their own are still counted) and, in `finish`, grants
/// all pending sends a brief window to land. Modeled on
/// [`crate::updates::UpdateWarning`].
pub(crate) struct TelemetrySession;

impl TelemetrySession {
    /// Fire the invocation event now. `command` is a stable event label (see
    /// `command_name` in `main.rs`), not raw user input. Inert when disabled.
    /// The `--no-telemetry` flag is read from the process-global (set in `main`
    /// before this is called), matching every other event path. The handle is
    /// registered in the pending set and flushed by `finish`.
    pub(crate) fn start(command: &str) -> TelemetrySession {
        // Bare base name; `build_payload` prefixes it → wire event `cli_command`.
        capture("command", json!({ "command": command }));
        TelemetrySession
    }

    /// Grant every pending send (this session's `cli_command` plus any key
    /// events fired during the command) one shared flush window to land before
    /// the runtime is torn down. Never touches stdout or the exit code.
    /// `_success` is accepted for a future `cli_command_failed` split (the call
    /// site in `main` already threads it, so keeping the param avoids
    /// re-touching main).
    pub(crate) async fn finish(self, _success: bool) {
        flush_pending().await;
    }
}

// ---------------------------------------------------------------------------
// First-run onboarding notice
// ---------------------------------------------------------------------------

/// The one-time disclosure, printed to stderr after the first `orx login`.
///
/// The disclosure is printed regardless of whether telemetry is currently
/// enabled, and `notice_shown` is recorded ONLY once it has actually been
/// printed. This is deliberate: the common first-login-under-`CI` case (or with
/// `DO_NOT_TRACK` set) would otherwise mark the notice "shown" while printing
/// nothing, and the user would *never* see the disclosure even after telemetry
/// later became active on that machine — a real gap for an opt-out system whose
/// whole basis is "we told you once." When disabled, the text says so and names
/// the reason. Best-effort throughout — never fails login, never touches stdout.
pub(crate) fn show_notice_once() {
    if load_settings().and_then(|s| s.notice_shown) == Some(true) {
        return;
    }

    match disabled_reason(flag()) {
        None => {
            eprintln!();
            eprintln!(
                "orx collects anonymous usage analytics to help improve the tool. \
                 No code, prompts, file contents, or identifiers are ever sent."
            );
            eprintln!(
                "Opt out anytime with `orx telemetry off`, DO_NOT_TRACK=1, or --no-telemetry."
            );
        }
        Some(reason) => {
            eprintln!();
            eprintln!(
                "orx can collect anonymous usage analytics to help improve the tool \
                 (no code, prompts, file contents, or identifiers are ever sent)."
            );
            eprintln!(
                "It is currently off ({}). Enable it with `orx telemetry on`.",
                reason.as_str()
            );
        }
    }

    // Only reached after the disclosure was actually printed above, so we never
    // mark it shown without showing it.
    let _ = mutate_settings(|s| s.notice_shown = Some(true));
}

// ---------------------------------------------------------------------------
// Key event: experiment started
// ---------------------------------------------------------------------------

/// The motivating key event. Fire only on success.
///
/// - `kind`: `"create"` (a node was created) or `"run"` (a run was launched).
/// - `local`: `true` when dispatched via the local `orx up` store path,
///   `false` for a hosted server experiment. NB this is a dispatch axis, not a
///   "runs on this machine" axis — e.g. `local=true, target="openresearch"` is
///   an ephemeral OpenResearch box provisioned for a local-mode run, and
///   `local=true, target="hf"` drives the user's own HF account from local mode.
/// - `target`: for a run, a COARSE compute label — the backend/provider name
///   (`"hf"`, `"modal"`, `"k8s"`, `"ssh"`, `"slurm"`, `"openresearch"`,
///   `"local"`) for local-mode runs, or the managed compute shape (`"gpu"`,
///   `"cpu"`, `"existing"`) for server runs. `None` for `create` (no compute).
///   Always a fixed enum label, never an id, name, or path.
///
/// One vocabulary axis per property: `target` is always "what compute", `local`
/// is always "which dispatch path". `kind`+`local` tell you how to read `target`.
///
/// Non-blocking: the send is spawned and registered for the exit-time flush, so
/// this returns immediately and never delays the command's own success output.
pub(crate) fn capture_experiment_started(kind: &str, local: bool, target: Option<&str>) {
    let mut extra = json!({ "kind": kind, "local": local });
    if let (Some(obj), Some(t)) = (extra.as_object_mut(), target) {
        obj.insert("target".to_string(), json!(t));
    }
    capture("experiment_started", extra);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    // Serializes the telemetry tests below, which mutate process-global env
    // (DO_NOT_TRACK / ORX_TELEMETRY / CI / XDG_CONFIG_HOME). IMPORTANT: this lock
    // is telemetry-module-local — it does NOT protect against a test in ANOTHER
    // module reading those same vars concurrently under the default
    // multithreaded test runner. Today no other test reads them at runtime (the
    // k8s/slurm/ssh tests are pure functions; localbox uses a disjoint
    // ORX_DATA_DIR), so there's no race. Any NEW test elsewhere that touches
    // these vars or config_dir() must isolate itself (e.g. its own temp
    // XDG_CONFIG_HOME) — it cannot rely on this lock.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        fn new(vars: &[&'static str]) -> Self {
            let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let saved = vars
                .iter()
                .map(|k| (*k, std::env::var(k).ok()))
                .collect::<Vec<_>>();
            for k in vars {
                std::env::remove_var(k);
            }
            EnvGuard { _lock: lock, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    const OPT_VARS: &[&str] = &["DO_NOT_TRACK", "ORX_TELEMETRY", "CI", "XDG_CONFIG_HOME"];

    #[test]
    fn opt_out_precedence() {
        let _g = EnvGuard::new(OPT_VARS);
        // Point config dir at a fresh throwaway path so the persisted-setting
        // branch reads nothing (unique per run to avoid cross-run leftovers).
        let dir = std::env::temp_dir().join(format!("orx-tel-none-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        // Clean env, no flag → enabled.
        assert!(is_enabled(false));

        // --no-telemetry flag.
        assert!(matches!(disabled_reason(true), Some(DisabledReason::Flag)));

        // DO_NOT_TRACK: any value (even empty) opts out.
        std::env::set_var("DO_NOT_TRACK", "");
        assert!(matches!(
            disabled_reason(false),
            Some(DisabledReason::DoNotTrack)
        ));
        std::env::remove_var("DO_NOT_TRACK");

        // ORX_TELEMETRY off switches.
        for v in ["0", "off", "false", "OFF", "False"] {
            std::env::set_var("ORX_TELEMETRY", v);
            assert!(
                matches!(disabled_reason(false), Some(DisabledReason::OrxTelemetry)),
                "ORX_TELEMETRY={v} should disable"
            );
        }
        // A non-off value leaves it enabled.
        std::env::set_var("ORX_TELEMETRY", "1");
        assert!(is_enabled(false));
        std::env::remove_var("ORX_TELEMETRY");

        // CI presence.
        std::env::set_var("CI", "true");
        assert!(matches!(disabled_reason(false), Some(DisabledReason::Ci)));
        std::env::remove_var("CI");

        // Flag beats everything.
        std::env::set_var("CI", "true");
        assert!(matches!(disabled_reason(true), Some(DisabledReason::Flag)));
    }

    #[test]
    fn persisted_opt_out() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-persist-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        // Nothing persisted → enabled.
        assert!(is_enabled(false));

        // Persist an opt-out and confirm it disables.
        set_persisted_disabled(true).unwrap();
        assert!(matches!(
            disabled_reason(false),
            Some(DisabledReason::Persisted)
        ));

        // Clearing it re-enables (and doesn't wipe the install id via the lock).
        let _ = install_id();
        set_persisted_disabled(false).unwrap();
        assert!(is_enabled(false));
        assert!(
            load_settings().and_then(|s| s.install_id).is_some(),
            "clearing opt-out must not clobber the install id"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn disjoint_mutations_merge_and_dont_clobber() {
        // Each mutation re-reads under the lock and patches only its own field,
        // so field-disjoint writes accumulate rather than overwrite. This is the
        // in-process analog of the cross-process flock guarantee that a
        // concurrent install-id write can't drop a persisted opt-out.
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-merge-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        set_persisted_disabled(true).unwrap(); // sets telemetry_disabled
        let _ = install_id(); // sets install_id via a separate mutation
        mutate_settings(|s| s.notice_shown = Some(true)).unwrap();

        let s = load_settings().expect("settings present");
        assert_eq!(s.telemetry_disabled, Some(true), "opt-out survived");
        assert!(s.install_id.is_some(), "install id survived");
        assert_eq!(s.notice_shown, Some(true), "notice flag survived");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_id_is_stable() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-id-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        let first = install_id().expect("id generated");
        let second = install_id().expect("id persisted");
        assert_eq!(first, second, "install id must be stable across calls");
        // A valid uuid.
        assert!(uuid::Uuid::parse_str(&first).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn payload_shape_is_anonymous_and_pii_free() {
        let payload = build_payload(
            "experiment_started",
            "test-distinct-id",
            json!({ "kind": "run", "local": false, "target": "modal" }),
        );

        assert_eq!(payload["api_key"], POSTHOG_KEY);
        // The bare name is `cli_`-prefixed on the wire so CLI events are
        // separable from the website's events in this shared PostHog project.
        assert_eq!(payload["event"], "cli_experiment_started");
        assert_eq!(payload["distinct_id"], "test-distinct-id");

        let props = &payload["properties"];
        // Anonymous marker present and false.
        assert_eq!(props["$process_person_profile"], false);
        assert_eq!(props["source"], "cli");
        assert!(props["cli_version"].is_string());
        assert!(props["os"].is_string());
        assert!(props["arch"].is_string());
        assert!(props["ci"].is_boolean());
        // Event-specific coarse props.
        assert_eq!(props["kind"], "run");
        assert_eq!(props["target"], "modal");

        // Client timestamp present at top level (so PostHog buckets on event
        // time, not ingestion time).
        assert!(payload["timestamp"].is_string());

        // Guard against PII creep: the whole serialized payload must not contain
        // obvious identifying keys.
        let text = serde_json::to_string(&payload).unwrap();
        for banned in ["email", "token", "path", "repo", "prompt", "title", "home"] {
            assert!(
                !text.contains(banned),
                "payload leaked a `{banned}` field: {text}"
            );
        }
    }

    #[test]
    fn extra_cannot_overwrite_base_context() {
        // A caller passing base keys in `extra` must not corrupt identity/context.
        let payload = build_payload(
            "command",
            "did",
            json!({ "source": "EVIL", "ci": "EVIL", "command": "login" }),
        );
        let props = &payload["properties"];
        assert_eq!(props["source"], "cli", "base source must win");
        assert!(props["ci"].is_boolean(), "base ci must win");
        // Non-colliding extra keys still land.
        assert_eq!(props["command"], "login");
    }

    #[test]
    fn every_event_name_is_cli_prefixed() {
        // This PostHog project is shared with the website; CLI events must be
        // separable by name alone. `build_payload` prefixes unconditionally, so
        // the two base names map to their prefixed wire names and nothing can
        // emit an unprefixed event.
        for (bare, wire) in [
            ("command", "cli_command"),
            ("experiment_started", "cli_experiment_started"),
        ] {
            let p = build_payload(bare, "did", json!({}));
            assert_eq!(p["event"], wire, "`{bare}` must serialize as `{wire}`");
        }
    }

    #[test]
    fn iso8601_is_correct() {
        // 0 ms → the Unix epoch.
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00.000Z");
        // A known instant: 2021-01-01T00:00:00.000Z = 1_609_459_200_000 ms.
        assert_eq!(iso8601_utc(1_609_459_200_000), "2021-01-01T00:00:00.000Z");
        // Millis + time-of-day: 2021-01-01T00:00:00.123Z + 1h2m3s.
        let ms = 1_609_459_200_000 + 123 + ((3600 + 2 * 60 + 3) * 1000);
        assert_eq!(iso8601_utc(ms), "2021-01-01T01:02:03.123Z");
        // Leap-year day: 2020-02-29T12:00:00.000Z = 1_582_977_600_000 ms.
        assert_eq!(iso8601_utc(1_582_977_600_000), "2020-02-29T12:00:00.000Z");
        // Negative clamps to epoch rather than panicking.
        assert_eq!(iso8601_utc(-5), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn corrupt_settings_fails_safe_and_is_not_clobbered() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-corrupt-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        // Write a corrupt settings.json.
        let cfg = dir.join("openresearch");
        std::fs::create_dir_all(&cfg).unwrap();
        let path = cfg.join("settings.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        // Fail safe: an unreadable file is treated as "disabled", not "enabled".
        assert!(matches!(
            disabled_reason(false),
            Some(DisabledReason::CorruptSettings)
        ));

        // A mutation must REFUSE rather than overwrite the corrupt file, so a
        // persisted opt-out potentially hiding in it is never silently dropped.
        assert!(set_persisted_disabled(true).is_err());
        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            after, b"{ this is not json",
            "corrupt file must be preserved"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn capture_is_non_blocking_and_drains_at_flush() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-flush-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        // Dead endpoint: the send will fail fast; we only care about registration
        // and that flush_pending returns (bounded, never hangs).
        std::env::set_var("ORX_TELEMETRY_HOST", "http://127.0.0.1:9");
        set_flag(false);
        // Start from a clean pending set (this is the only test that uses it,
        // but guard against ordering just in case).
        pending().lock().unwrap().clear();

        // Disabled → nothing registered.
        std::env::set_var("DO_NOT_TRACK", "1");
        capture("experiment_started", json!({ "kind": "run" }));
        assert!(
            pending().lock().unwrap().is_empty(),
            "disabled capture must register nothing"
        );
        std::env::remove_var("DO_NOT_TRACK");

        // Enabled → capture returns immediately (non-blocking) and registers a
        // handle for the exit-time flush.
        capture("experiment_started", json!({ "kind": "run" }));
        assert_eq!(
            pending().lock().unwrap().len(),
            1,
            "enabled capture must register exactly one pending send"
        );

        // Draining the pending set empties it and returns within bounds.
        flush_pending().await;
        assert!(
            pending().lock().unwrap().is_empty(),
            "flush_pending must drain the pending set"
        );

        std::env::remove_var("ORX_TELEMETRY_HOST");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
