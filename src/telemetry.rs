//! Anonymous, opt-out usage analytics → PostHog.
//!
//! Why this exists: `orx` shipped with no telemetry, so we had no way to see
//! installs, DAU/WAU, retention, or which commands people actually use. This
//! module sends anonymous events (a random per-install UUID as the only
//! identity — never any PII, prompt text, file paths, ids, or repo contents).
//!
//! Guarantees, enforced by this module:
//! - **Opt-out.** A `--no-telemetry` flag and a persistent `orx telemetry off`
//!   (also toggleable from the `orx up` onboarding step). A disabled run sends
//!   nothing, touches no disk, and generates no install id.
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
    /// Machine context tag (e.g. `"cloud-agent"` on OpenResearch cloud boxes),
    /// set once by `orx telemetry context <value>` during box provisioning.
    /// Stamped on every event as the `install_kind` property so first-party
    /// automation can be excluded from human usage metrics centrally (the
    /// PostHog internal/test-users filter) instead of per-insight. Absent = a
    /// human install (`install_kind: "human"`).
    #[serde(default)]
    pub machine_context: Option<String>,
    /// User-chosen data directory (Storage settings). Absent = fall back to the
    /// env/XDG/default chain in `store::data_dir()`. Persisted here — in the one
    /// `settings.json` — so a write can't clobber `install_id`/`telemetry_disabled`
    /// (each mutation re-reads and patches only its own field via `mutate_settings`).
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Default compute target for local-mode launches (Settings → Compute).
    /// Absent = no default: the CLI requires an explicit `--backend` and the
    /// HTTP run endpoint keeps its historical `hf` fallback.
    #[serde(default)]
    pub default_backend: Option<String>,
    /// Default `--flavor`, only meaningful alongside `default_backend` and only
    /// applied when a launch resolves to that same backend without a flavor.
    #[serde(default)]
    pub default_flavor: Option<String>,
}

/// The persisted data-dir choice, if any (non-empty). Read by `store::data_dir()`
/// on every open — so it goes through the plain `load_settings` reader, not the
/// locked RMW path. `crate::config` re-exports this as `settings_data_dir`.
pub(crate) fn persisted_data_dir() -> Option<String> {
    load_settings()
        .and_then(|s| s.data_dir)
        .filter(|s| !s.is_empty())
}

/// Set or clear the persisted data dir, preserving every other settings field.
/// Routes through `mutate_settings` so it inherits the in-process mutex, the
/// cross-process flock, the atomic temp+rename, and the corrupt-file refusal —
/// the same guarantees `orx telemetry off` relies on. `crate::config` re-exports
/// this as `set_settings_data_dir`.
pub(crate) fn set_persisted_data_dir(data_dir: Option<String>) -> std::io::Result<()> {
    mutate_settings(|s| s.data_dir = data_dir.filter(|v| !v.is_empty()))
}

/// The persisted default compute target, if any: `(backend, flavor)`. A flavor
/// without a backend is meaningless and is dropped. `crate::config` re-exports
/// this as `compute_default`.
pub(crate) fn compute_default() -> Option<(String, Option<String>)> {
    let s = load_settings()?;
    let backend = s.default_backend.filter(|b| !b.is_empty())?;
    Some((backend, s.default_flavor.filter(|f| !f.is_empty())))
}

/// Set or clear the default compute target, preserving every other settings
/// field (same `mutate_settings` guarantees as the data dir). Clearing the
/// backend also clears the flavor — a dangling flavor must not resurface if a
/// different backend is chosen later. `crate::config` re-exports this as
/// `set_compute_default`.
pub(crate) fn set_compute_default(
    backend: Option<String>,
    flavor: Option<String>,
) -> std::io::Result<()> {
    mutate_settings(|s| {
        s.default_backend = backend.filter(|b| !b.is_empty());
        s.default_flavor = if s.default_backend.is_some() {
            flavor.filter(|f| !f.is_empty())
        } else {
            None
        };
    })
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
    Persisted,
    CorruptSettings,
}

impl DisabledReason {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            DisabledReason::Flag => "--no-telemetry flag",
            DisabledReason::Persisted => "disabled via `orx telemetry off`",
            DisabledReason::CorruptSettings => "settings file unreadable (failing safe)",
        }
    }
}

/// Resolves whether telemetry is disabled and why. The flag is checked before
/// the persisted setting so a `--no-telemetry` run never reads disk.
/// `cli_flag` is the `--no-telemetry` global flag.
///
/// Opt-out surface is intentionally minimal: the `--no-telemetry` flag and the
/// persistent `orx telemetry off`. Automated/CI runs are not auto-disabled —
/// the `ci` property on every event lets those be filtered at query time
/// instead, which also catches automation the old `CI` env check missed
/// (agent boxes, Jenkins, ad-hoc scripts).
pub(crate) fn disabled_reason(cli_flag: bool) -> Option<DisabledReason> {
    if cli_flag {
        return Some(DisabledReason::Flag);
    }
    // Persisted state is the only branch that reads disk.
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

/// The effective machine context: the `ORX_TELEMETRY_CONTEXT` env var when set
/// and non-empty (so a fleet can re-tag per-process without touching disk),
/// else the persisted `machine_context`. `None` = a human install.
pub(crate) fn machine_context() -> Option<String> {
    if let Ok(v) = std::env::var("ORX_TELEMETRY_CONTEXT") {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    load_settings()
        .and_then(|s| s.machine_context)
        .filter(|v| !v.is_empty())
}

/// Persist (or clear, with `None`) the machine context, preserving every other
/// settings field via the locked RMW. Used by `orx telemetry context`.
pub(crate) fn set_machine_context(context: Option<String>) -> std::io::Result<()> {
    mutate_settings(|s| {
        s.machine_context = context
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
    })
}

/// The value stamped as every event's `install_kind` property: the machine
/// context when one is set, else `"human"`. This is the delineation axis for
/// "installs by humans" vs first-party automation (cloud agent boxes) — kept
/// separate from `ci`, which flags *third-party* automation (a user's own CI).
fn install_kind() -> String {
    machine_context().unwrap_or_else(|| "human".to_string())
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
/// same base context (source, version, os, arch, ci, install_kind) plus
/// `$process_person_profile: false` to keep it anonymous. `extra` supplies
/// event-specific properties — callers MUST keep these free of PII (coarse
/// enums only).
fn build_payload(event: &str, distinct_id: &str, extra: serde_json::Value) -> serde_json::Value {
    let mut props = json!({
        // Anonymous: don't build a person profile for this distinct_id.
        "$process_person_profile": false,
        "source": "cli",
        "cli_version": crate::updates::current_version().to_string(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "ci": is_ci(),
        // Human install vs first-party automation (e.g. "cloud-agent" boxes).
        // Fed into PostHog's internal-users filter so human-facing metrics
        // exclude our own fleet by default, without per-insight filters.
        "install_kind": install_kind(),
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

/// Shared `distinct_id` for consent events that must not touch the install id
/// (declines, and agrees whose id could not be persisted). A FIXED sentinel, not
/// a per-event UUID: a fresh UUID per event minted a brand-new PostHog person
/// every time (67 phantom "users" in the launch week alone), silently inflating
/// any unique-user metric that included the consent event. With one shared
/// sentinel, all such events collapse into a single well-known person that is
/// trivially excluded — while still writing nothing to disk and remaining
/// unlinkable to any real install.
const CONSENT_SENTINEL_ID: &str = "cli-consent-anonymous";

/// Record a telemetry consent decision — `cli_telemetry_consent` with
/// `{ agreed: bool }`. This is the ONE event that fires UNCONDITIONALLY: it must
/// land even when the user chose to disable telemetry, otherwise every rejection
/// would be invisible and the agree/reject ratio would be hopelessly skewed
/// toward "agree".
///
/// Identity policy (phantom-free by construction):
/// - `agreed` → the persistent install id. The user just consented to
///   analytics, so tying the consent to the same id their other events use is
///   fine — and it makes opt-ins joinable with the active-install population.
/// - declined (or the id couldn't be persisted) → [`CONSENT_SENTINEL_ID`]. A
///   user opting out must not get a persisted anonymous id as a side effect,
///   and must not mint a phantom person either.
///
/// Awaited with a bounded timeout so a caller (the `orx up` settings handler or
/// the `orx telemetry on/off` command) can fire-and-confirm without hanging.
/// Errors are swallowed — recording consent must never fail the action.
/// Resolve the consent event's `distinct_id` per the policy above. Split out of
/// `record_consent` so the identity rules are unit-testable without a network
/// send. NB the `agreed` path calls `install_id()`, which generates + persists
/// an id if absent — acceptable precisely because the user agreed.
fn consent_distinct_id(agreed: bool) -> String {
    if agreed {
        if let Some(id) = install_id() {
            return id;
        }
    }
    CONSENT_SENTINEL_ID.to_string()
}

pub(crate) async fn record_consent(agreed: bool) {
    let distinct_id = consent_distinct_id(agreed);
    let payload = build_payload(
        "telemetry_consent",
        &distinct_id,
        json!({ "agreed": agreed }),
    );
    let url = format!("{}/i/v0/e/", posthog_host());
    let send = async {
        let _ = http()
            .post(&url)
            .timeout(Duration::from_secs(3))
            .json(&payload)
            .send()
            .await;
    };
    // Cap the wait so the settings POST / command return promptly even if the
    // network is slow; the send itself also has its own 3s request timeout.
    let _ = tokio::time::timeout(Duration::from_secs(3), send).await;
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
    // (XDG_CONFIG_HOME, and ORX_TELEMETRY_HOST in one test). IMPORTANT: this lock
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

    const OPT_VARS: &[&str] = &["XDG_CONFIG_HOME", "ORX_TELEMETRY_CONTEXT"];

    #[test]
    fn opt_out_precedence() {
        let _g = EnvGuard::new(OPT_VARS);
        // Point config dir at a fresh throwaway path so the persisted-setting
        // branch reads nothing (unique per run to avoid cross-run leftovers).
        let dir = std::env::temp_dir().join(format!("orx-tel-none-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        // Clean state, no flag → enabled. Automated/CI environments are NOT
        // auto-disabled (that's a query-time filter via the `ci` property).
        assert!(is_enabled(false));

        // The only per-run opt-out is the --no-telemetry flag.
        assert!(matches!(disabled_reason(true), Some(DisabledReason::Flag)));

        let _ = std::fs::remove_dir_all(&dir);
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

        let s = load_settings().expect("settings present");
        assert_eq!(s.telemetry_disabled, Some(true), "opt-out survived");
        assert!(
            s.install_id.is_some(),
            "install id survived a separate opt-out mutation"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn data_dir_and_telemetry_fields_dont_clobber_each_other() {
        // Regression: the Storage feature persists `data_dir` in the *same*
        // settings.json telemetry owns. Both must go through mutate_settings so a
        // data-dir write preserves install_id/telemetry_disabled and vice-versa —
        // otherwise a completed data-dir move reverts on the next CLI run.
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-datadir-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        set_persisted_disabled(true).unwrap(); // telemetry_disabled
        let id = install_id().expect("install id"); // install_id
        set_persisted_data_dir(Some("/tmp/orx-moved".into())).unwrap(); // data_dir

        let s = load_settings().expect("settings present");
        assert_eq!(s.data_dir.as_deref(), Some("/tmp/orx-moved"));
        assert_eq!(
            s.telemetry_disabled,
            Some(true),
            "opt-out survived data-dir write"
        );
        assert_eq!(
            s.install_id.as_deref(),
            Some(id.as_str()),
            "install id survived"
        );
        assert_eq!(persisted_data_dir().as_deref(), Some("/tmp/orx-moved"));

        // Clearing the data dir leaves the telemetry fields intact.
        set_persisted_data_dir(None).unwrap();
        let s = load_settings().expect("settings present");
        assert!(s.data_dir.is_none(), "data_dir cleared");
        assert_eq!(s.telemetry_disabled, Some(true), "opt-out still intact");
        assert!(persisted_data_dir().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compute_default_roundtrip_preserves_siblings() {
        // Same single-writer contract as data_dir: the Compute settings persist
        // in the telemetry-owned settings.json, so a default-target write must
        // preserve install_id/telemetry_disabled/data_dir and vice-versa.
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-compute-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        set_persisted_disabled(true).unwrap();
        let id = install_id().expect("install id");
        set_compute_default(Some("modal".into()), Some("a10g".into())).unwrap();

        let s = load_settings().expect("settings present");
        assert_eq!(s.default_backend.as_deref(), Some("modal"));
        assert_eq!(s.default_flavor.as_deref(), Some("a10g"));
        assert_eq!(s.telemetry_disabled, Some(true), "opt-out survived");
        assert_eq!(s.install_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            compute_default(),
            Some(("modal".to_string(), Some("a10g".to_string())))
        );

        // Backend without flavor; empty flavor is treated as absent.
        set_compute_default(Some("local".into()), Some(String::new())).unwrap();
        assert_eq!(compute_default(), Some(("local".to_string(), None)));

        // Clearing the backend clears the flavor too and leaves siblings intact.
        set_compute_default(None, Some("dangling".into())).unwrap();
        let s = load_settings().expect("settings present");
        assert!(s.default_backend.is_none());
        assert!(s.default_flavor.is_none(), "flavor cleared with backend");
        assert_eq!(s.telemetry_disabled, Some(true), "opt-out still intact");
        assert!(compute_default().is_none());

        // Older settings.json without the new keys parses and reads as no default.
        std::fs::write(
            settings_path(),
            r#"{"installId":"abc","telemetryDisabled":false}"#,
        )
        .unwrap();
        assert!(compute_default().is_none());
        assert_eq!(
            load_settings().and_then(|s| s.install_id).as_deref(),
            Some("abc")
        );

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
        // build_payload reads the machine context (env + settings), so isolate.
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-shape-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
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
        // No context configured → a human install.
        assert_eq!(props["install_kind"], "human");
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
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extra_cannot_overwrite_base_context() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-extra-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        // A caller passing base keys in `extra` must not corrupt identity/context.
        let payload = build_payload(
            "command",
            "did",
            json!({ "source": "EVIL", "ci": "EVIL", "install_kind": "EVIL", "command": "login" }),
        );
        let props = &payload["properties"];
        assert_eq!(props["source"], "cli", "base source must win");
        assert!(props["ci"].is_boolean(), "base ci must win");
        assert_eq!(props["install_kind"], "human", "base install_kind must win");
        // Non-colliding extra keys still land.
        assert_eq!(props["command"], "login");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_kind_resolution_env_over_persisted_over_default() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-kind-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        // Nothing configured → human.
        assert_eq!(install_kind(), "human");

        // Persisted context wins over the default and survives sibling writes.
        set_machine_context(Some("cloud-agent".into())).unwrap();
        assert_eq!(install_kind(), "cloud-agent");
        set_persisted_disabled(true).unwrap();
        let s = load_settings().expect("settings present");
        assert_eq!(
            s.machine_context.as_deref(),
            Some("cloud-agent"),
            "context survived a sibling mutation"
        );
        assert_eq!(s.telemetry_disabled, Some(true));
        set_persisted_disabled(false).unwrap();

        // Env var wins over the persisted value; whitespace-only is ignored.
        std::env::set_var("ORX_TELEMETRY_CONTEXT", "ci-fleet");
        assert_eq!(install_kind(), "ci-fleet");
        std::env::set_var("ORX_TELEMETRY_CONTEXT", "   ");
        assert_eq!(install_kind(), "cloud-agent");
        std::env::remove_var("ORX_TELEMETRY_CONTEXT");

        // Clearing restores the default; empty string means clear.
        set_machine_context(None).unwrap();
        assert_eq!(install_kind(), "human");
        set_machine_context(Some("  ".into())).unwrap();
        assert!(load_settings().unwrap().machine_context.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consent_identity_is_phantom_free() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-cid-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);

        // Decline: the fixed sentinel, never a fresh UUID, and no persisted id.
        assert_eq!(consent_distinct_id(false), CONSENT_SENTINEL_ID);
        assert_eq!(consent_distinct_id(false), CONSENT_SENTINEL_ID);
        assert!(
            load_settings().and_then(|s| s.install_id).is_none(),
            "a decline must not generate a persisted install id"
        );

        // Agree: the real (now persisted) install id, stable across calls, so
        // opt-ins join the active-install population instead of minting a
        // one-off person.
        let agreed_id = consent_distinct_id(true);
        assert_eq!(
            load_settings().and_then(|s| s.install_id).as_deref(),
            Some(agreed_id.as_str()),
            "an agree ties consent to the persisted install id"
        );
        assert_eq!(consent_distinct_id(true), agreed_id);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_event_name_is_cli_prefixed() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-prefix-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        // This PostHog project is shared with the website; CLI events must be
        // separable by name alone. `build_payload` prefixes unconditionally, so
        // the two base names map to their prefixed wire names and nothing can
        // emit an unprefixed event.
        for (bare, wire) in [
            ("command", "cli_command"),
            ("experiment_started", "cli_experiment_started"),
            ("telemetry_consent", "cli_telemetry_consent"),
        ] {
            let p = build_payload(bare, "did", json!({}));
            assert_eq!(p["event"], wire, "`{bare}` must serialize as `{wire}`");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consent_payload_carries_agreed_flag() {
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-agreed-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        for agreed in [true, false] {
            let p = build_payload("telemetry_consent", "did", json!({ "agreed": agreed }));
            assert_eq!(p["event"], "cli_telemetry_consent");
            assert_eq!(p["properties"]["agreed"], agreed);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn record_consent_sends_even_when_opted_out() {
        // The consent event is UNCONDITIONAL: a persisted opt-out must not
        // suppress it (otherwise rejections would be invisible). We can't easily
        // assert the wire send in-process, but we CAN assert record_consent does
        // not early-return on the opt-out path and returns promptly against a
        // dead endpoint — i.e. it neither hangs nor panics while disabled.
        let _g = EnvGuard::new(OPT_VARS);
        let dir = std::env::temp_dir().join(format!("orx-tel-consent-{}", uuid::Uuid::new_v4()));
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        std::env::set_var("ORX_TELEMETRY_HOST", "http://127.0.0.1:9");

        // Persist an opt-out; normal telemetry is now disabled.
        set_persisted_disabled(true).unwrap();
        assert!(matches!(
            disabled_reason(false),
            Some(DisabledReason::Persisted)
        ));

        // Still returns (bounded by the internal timeout) without panicking, and
        // does NOT create a persisted install id as a side effect.
        record_consent(false).await;
        assert!(
            load_settings().and_then(|s| s.install_id).is_none(),
            "consent must not generate a persisted install id"
        );

        std::env::remove_var("ORX_TELEMETRY_HOST");
        let _ = std::fs::remove_dir_all(&dir);
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
        // Start from a clean pending set (this is the only test that uses it,
        // but guard against ordering just in case).
        pending().lock().unwrap().clear();

        // Disabled (persisted opt-out) → nothing registered. Using the persisted
        // flag rather than the process-global `--no-telemetry` flag, which is a
        // write-once OnceLock another test may already have set.
        set_persisted_disabled(true).unwrap();
        capture("experiment_started", json!({ "kind": "run" }));
        assert!(
            pending().lock().unwrap().is_empty(),
            "disabled capture must register nothing"
        );
        set_persisted_disabled(false).unwrap();

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
