//! Claude Code harness.
//!
//! Chat: one *resident* `claude --print --input-format stream-json` child per
//! chat session (`local::claude::ClaudeHost`), reused across turns — each turn
//! sends one user message and folds the child's stream-json output, from the
//! `--replay-user-messages` echo of that message (the turn's start boundary —
//! see `belongs_to_current_turn`) until a `result` event. The child persists
//! (stable `session_id`, stdin held open), collapsing the old spawn-per-turn
//! overhead; a config change (permission mode
//! / effort / bridge), interrupt, or crash respawns it with `--resume`. The
//! playbook rides `--append-system-prompt-file`; the permission mode is
//! `--permission-mode` from the session's setting (`auto`/`bypassPermissions` — see
//! `options`). AskUserQuestion / ExitPlanMode surface as interactive cards: the
//! turn ends on them and the user's answer resumes the session — except in plan
//! mode, where the mcp-gate bridge holds both open mid-turn and the answer
//! continues the same turn.
//!
//! Detection: `claude auth status --json` is the readiness source of truth.
//! `~/.claude.json` contributes display metadata only after that live check;
//! `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` remain credential fallbacks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::detect::{
    bin_version, nonempty_str, parse_version, read_json, HarnessAuthState, HarnessInfo, ModelInfo,
};
use super::options::{
    HarnessOptions, OptionChoice, PermissionMode, PlanActivation, REASONING_DEFAULT_ID,
};
use super::{
    Harness, ResumeAction, TurnFailure, TurnOutcome, TurnResult, Waited, ORX_MAX_ATTEMPTS,
};
use crate::error::{anyhow, Result};
use crate::local::chat::{
    find_part_mut, prepare_env, ContextUsage, DeliveryState, PromptAnswer, ResumeCtx, TurnCtx,
    WirePart, WirePrompt, WireQuestionOption, WireToolState,
};
use crate::local::claude::{SpawnConfig, SpawnSpec, TurnEvent};
use crate::local::opencode::ensure_playbook;
use crate::local::shell_env::{self, find_on_path};

/// FALLBACK model list, used only when the `list_models` control request fails
/// (a CLI too old to answer it, or a spawn/timeout failure). The primary source
/// is [`claude_list_models`]: the same catalog the CLI's own `/model` menu
/// renders, with per-model `supportedEffortLevels`.
const CLAUDE_MODELS: [&str; 4] = [
    "claude-fable-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    "claude-haiku-4-5",
];

/// FALLBACK effort tiers, paired with `CLAUDE_MODELS` above — the base five
/// every supported CLI accepts. The primary source is per-model
/// `supportedEffortLevels` from `list_models`.
const CLAUDE_EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// `ultracode` — the session mode that selects `xhigh` effort plus standing
/// dynamic-workflow orchestration. NOT the same as `ultrathink` (a prompt
/// keyword) or Codex's `ultra`. The CLI models it as a *mode*, not an effort
/// level: `list_models` never includes it in `supportedEffortLevels`, even on
/// versions whose `--effort` accepts it — which is why support is detected by
/// [`claude_accepts_ultracode`] rather than read from the catalog.
const CLAUDE_ULTRACODE: &str = "ultracode";

/// Includes Anthropic's multi-process refresh-token and sleep/wake fixes.
const MIN_CLAUDE_VERSION: (u64, u64, u64) = (2, 1, 211);

const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuthProbe {
    state: HarnessAuthState,
    method: Option<&'static str>,
}

fn parse_auth_status(success: bool, stdout: &[u8]) -> AuthProbe {
    let value = serde_json::from_slice::<Value>(stdout).ok();
    let logged_in = value
        .as_ref()
        .and_then(|value| value.get("loggedIn"))
        .and_then(Value::as_bool);
    let reported_method = value
        .as_ref()
        .and_then(|value| value.get("authMethod"))
        .and_then(Value::as_str)
        .map(|method| method.to_ascii_lowercase());
    let method = reported_method.as_deref().and_then(|method| {
        if method.contains("api") || method.contains("token") {
            Some("apiKey")
        } else if method.contains("oauth") || method.contains("claude") {
            Some("oauth")
        } else {
            None
        }
    });
    let state = match (success, logged_in) {
        (true, Some(true)) => HarnessAuthState::Ready,
        (_, Some(false)) => HarnessAuthState::NeedsLogin,
        _ => HarnessAuthState::Unknown,
    };
    AuthProbe { state, method }
}

async fn probe_auth(bin: &Path) -> AuthProbe {
    let mut cmd = Command::new(bin);
    cmd.args(["auth", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    prepare_env(&mut cmd);
    match tokio::time::timeout(AUTH_STATUS_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => parse_auth_status(out.status.success(), &out.stdout),
        _ => AuthProbe {
            state: HarnessAuthState::Unknown,
            method: None,
        },
    }
}

async fn effective_auth_probe(bin: &Path) -> AuthProbe {
    let mut probe = probe_auth(bin).await;
    // Headless Claude gives ANTHROPIC_* credentials precedence over a saved
    // subscription login. If status still reports OAuth in that environment,
    // it has only verified leftover OAuth metadata, not the credential the
    // worker will actually send.
    if has_api_credential() && probe.method != Some("apiKey") {
        probe.state = HarnessAuthState::Unknown;
        probe.method = None;
    } else if probe.state == HarnessAuthState::Ready && probe.method.is_none() {
        probe.method = Some("oauth");
    }
    probe
}

fn gate_oauth_version(mut probe: AuthProbe, version: Option<&str>) -> AuthProbe {
    if probe.state == HarnessAuthState::Ready && probe.method == Some("oauth") {
        probe.state = match version.and_then(parse_version) {
            Some(version) if version >= MIN_CLAUDE_VERSION => HarnessAuthState::Ready,
            Some(_) => HarnessAuthState::Unsupported,
            None => HarnessAuthState::Unknown,
        };
    }
    probe
}

pub(crate) async fn current_auth_state() -> HarnessAuthState {
    match find_claude() {
        Some(bin) => {
            let version = bin_version(&bin).await;
            gate_oauth_version(effective_auth_probe(&bin).await, version.as_deref()).state
        }
        None => HarnessAuthState::Unknown,
    }
}

pub(crate) fn auth_recovery_note() -> &'static str {
    if has_api_credential() {
        "Claude Code rejected the configured `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`. Replace or unset it, then re-check this harness."
    } else {
        "Sign in with `claude auth login`, then re-check this harness."
    }
}

/// Ask the installed CLI's own argument parser whether it accepts
/// `--effort ultracode`. `--version` still runs the parser, which prints
/// `Warning: Unknown --effort value …` for a value it doesn't know and exits
/// without touching the network (~0.2s); absence of the warning is acceptance.
///
/// The parser is the only truthful surface. Every enumeration the CLI offers
/// lies about this value: `--help` lists five tiers on versions that accept
/// six; the warning's own "Valid values:" list omits `ultracode` on versions
/// that accept it; and `list_models` never advertises it (see
/// [`CLAUDE_ULTRACODE`]). Probing the parser replaces a hard-coded version
/// gate — the boundary (2.1.202 rejects / 2.1.203 accepts, bisected across
/// every published version in between) is now discovered per install instead
/// of pinned.
///
/// Any failure reports unsupported: a missing choice is a smaller harm than a
/// choice that silently runs at the default effort.
async fn claude_accepts_ultracode(bin: &Path) -> bool {
    let mut cmd = Command::new(bin);
    cmd.args(["--effort", CLAUDE_ULTRACODE, "--version"])
        .stdin(Stdio::null());
    prepare_env(&mut cmd);
    let fut = cmd.output();
    match tokio::time::timeout(Duration::from_secs(10), fut).await {
        Ok(Ok(out)) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            out.status.success() && !text.contains("Unknown --effort value")
        }
        _ => false,
    }
}

/// Query the CLI's own model catalog — the `list_models` control request over
/// `--print` stream-json, the same data its `/model` menu renders: every model
/// with its `supportedEffortLevels`. This is the Claude analogue of codex's
/// `model/list` and opencode's `models --verbose`; a curated table here shipped
/// effort tiers on Haiku, which the catalog says supports none.
///
/// One shot: spawn, write the control request, read until its
/// `control_response` (skipping stream noise), kill the child. Any failure —
/// spawn, timeout, a CLI too old for the subtype — returns `None` and the
/// caller falls back to the static table.
async fn claude_list_models(bin: &Path, ultracode: bool) -> Option<Vec<ModelInfo>> {
    let fut = async {
        let mut cmd = Command::new(bin);
        cmd.args([
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ]);
        prepare_env(&mut cmd);
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let mut lines = BufReader::new(child.stdout.take()?).lines();

        use tokio::io::AsyncWriteExt;
        let req = serde_json::json!({
            "type": "control_request",
            "request_id": "orx_list_models",
            "request": { "subtype": "list_models" },
        });
        let mut line = req.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.ok()?;

        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("control_response") {
                continue;
            }
            let resp = v.get("response")?;
            if resp.get("request_id").and_then(Value::as_str) != Some("orx_list_models") {
                continue;
            }
            // An `error` subtype has no inner response — `?` falls through to
            // the static fallback.
            let models = parse_claude_model_list(resp.get("response")?, ultracode);
            return (!models.is_empty()).then_some(models);
        }
        None
    };
    tokio::time::timeout(Duration::from_secs(15), fut)
        .await
        .ok()
        .flatten()
}

/// `list_models` response → per-model `ModelInfo`. Split from the transport
/// for testability.
///
/// * `value` is the id the CLI's own picker submits (aliases like `sonnet`,
///   `opus[1m]`), so it's what we store and pass back as `--model`.
/// * The `default` entry is skipped — the composer's "Default model" row (a
///   null model) already means "let the CLI pick".
/// * A model without `supportedEffortLevels` (Haiku) gets an empty list, which
///   hides the reasoning picker — same absent-vs-empty contract as opencode.
/// * `ultracode` is appended where the CLI accepts it (see
///   [`claude_accepts_ultracode`]) and the model reaches `xhigh`, since the
///   mode is documented as `xhigh` + dynamic workflows.
fn parse_claude_model_list(result: &Value, ultracode: bool) -> Vec<ModelInfo> {
    let Some(models) = result.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|m| {
            let value = m.get("value").and_then(Value::as_str)?;
            if value == "default" {
                return None;
            }
            let mut efforts: Vec<&str> = m
                .get("supportedEffortLevels")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            if ultracode && efforts.contains(&"xhigh") {
                efforts.push(CLAUDE_ULTRACODE);
            }
            // The catalog's `displayName` is unversioned ("Opus"); the version
            // lives in the description's first `·` segment ("Opus 4.8 with 1M
            // context · Best for everyday, complex tasks"). Promote that
            // segment to the display name and keep the rest as the blurb, so
            // the picker leads with the resolved version.
            let (name, blurb) = match m.get("description").and_then(Value::as_str) {
                Some(desc) => match desc.split_once('·') {
                    Some((head, tail)) => (Some(head.trim()), Some(tail.trim())),
                    None => (m.get("displayName").and_then(Value::as_str), Some(desc)),
                },
                None => (m.get("displayName").and_then(Value::as_str), None),
            };
            let mut info = ModelInfo::new(value)
                .with_reasoning(&efforts)
                .with_label(name, blurb);
            // Claude reports no default *tier* because its unset default isn't
            // one: with adaptive thinking, the CLI scales effort per request.
            // Name the sentinel row for what actually runs — preselecting a
            // fixed tier here would pin behavior the user never asked for.
            if m.get("supportsAdaptiveThinking") == Some(&Value::Bool(true)) {
                if let Some(choices) = info.reasoning_levels.as_mut() {
                    if let Some(sentinel) = choices.first_mut() {
                        sentinel.label = "Adaptive".to_string();
                    }
                }
            }
            Some(info)
        })
        .collect()
}

/// The FALLBACK effort ids (see `CLAUDE_EFFORT_LEVELS`), plus `ultracode` when
/// the parser probe accepted it.
fn claude_effort_ids(ultracode: bool) -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = CLAUDE_EFFORT_LEVELS.to_vec();
    if ultracode {
        ids.push(CLAUDE_ULTRACODE);
    }
    ids
}

pub struct ClaudeCode;

/// Either credential Claude Code accepts. `ANTHROPIC_AUTH_TOKEN` is the one a
/// custom `ANTHROPIC_BASE_URL` gateway uses, so detecting only the api key
/// reports those working setups as signed out.
const CLAUDE_CREDENTIAL_VARS: [&str; 2] = ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

fn has_api_credential() -> bool {
    CLAUDE_CREDENTIAL_VARS
        .iter()
        .any(|key| super::detect::api_key(key).is_some())
}

/// One-shot session title from the first user message: a throwaway
/// `claude -p` child pinned to Haiku, mirroring how Claude Code titles its own
/// conversations with a cheap background model. Deliberately *not* the session's
/// resident child — a title request there would pollute the real conversation
/// history.
///
/// `--model haiku` is a CLI model alias of the kind we already pass through from
/// the catalog; a CLI too old to know it exits non-zero, which lands on `None`
/// and leaves the placeholder title in place. Every other failure (spawn,
/// timeout, garbage output) degrades the same silent way.
async fn claude_generate_title(bin: &Path, first_message: &str) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args([
        "-p",
        &super::title::title_prompt(first_message),
        "--model",
        "haiku",
        "--max-turns",
        "1",
        // Naming a chat needs no tools and no MCP: booting the user's servers
        // for a one-line request would cost far more than the request itself.
        // With no tools to call, `--max-turns 1` can't be spent on a tool use.
        // The empty list is the documented "disable all tools" form; an older
        // CLI that rejects it exits non-zero, so the placeholder is kept.
        "--strict-mcp-config",
        "--tools",
        "",
        // Replace the agent system prompt: the default hauls ~8.5k tokens of
        // Claude Code scaffolding into a request that ignores it (measured
        // 8.5k → 1.9k input). Latency is unchanged — the ~3s is node boot plus
        // one API round trip — but every title gets ~78% cheaper.
        "--system-prompt",
        "You generate short chat titles.",
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true)
    // Hermetic: run outside any repo so the child doesn't ingest the server
    // cwd's CLAUDE.md / settings into a request that only needs one sentence.
    .current_dir(std::env::temp_dir());
    prepare_env(&mut cmd);
    // Plain text only — an ANSI-colorizing CLI (or a synced FORCE_COLOR) would
    // otherwise write escape codes straight into the title column.
    cmd.env("NO_COLOR", "1");
    let fut = cmd.output();
    let out = tokio::time::timeout(super::title::TITLE_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    super::title::sanitize_title(&String::from_utf8_lossy(&out.stdout))
}

/// `claude` on PATH, else the common install drop locations.
pub(crate) fn find_claude() -> Option<PathBuf> {
    find_on_path("claude").or_else(|| {
        let home = dirs::home_dir()?;
        [".claude/local/claude", ".local/bin/claude"]
            .iter()
            .map(|rel| home.join(rel))
            .find(|c| c.is_file())
    })
}

#[derive(Debug, PartialEq, Eq)]
struct ClaudeConfigPaths {
    root: PathBuf,
    metadata: PathBuf,
}

fn resolve_config_paths(
    config_dir: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<ClaudeConfigPaths> {
    if let Some(root) = config_dir.filter(|path| !path.as_os_str().is_empty()) {
        return Some(ClaudeConfigPaths {
            metadata: root.join(".claude.json"),
            root,
        });
    }
    let home = home?;
    Some(ClaudeConfigPaths {
        root: home.join(".claude"),
        metadata: home.join(".claude.json"),
    })
}

fn config_paths() -> Option<ClaudeConfigPaths> {
    resolve_config_paths(
        shell_env::var("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        dirs::home_dir(),
    )
}

#[async_trait]
impl Harness for ClaudeCode {
    fn id(&self) -> &'static str {
        "claude-code"
    }

    fn name(&self) -> &'static str {
        "Claude Code"
    }

    fn supports_chat(&self) -> bool {
        true
    }

    /// The resident child holds stdin open, so a second stream-json user
    /// message reaches the turn already running.
    fn supports_steering(&self) -> bool {
        true
    }

    async fn detect(&self) -> Option<HarnessInfo> {
        let mut info = HarnessInfo::new(self.id(), self.name());
        if let Some(bin) = find_claude() {
            info.installed = true;
            info.version = bin_version(&bin).await;
            info.bin_path = Some(bin.to_string_lossy().into_owned());
        }
        // The CLI owns OAuth and Keychain refresh. Its live status, including
        // the effective auth method, decides whether this harness can run.
        if info.installed {
            let bin = info.bin_path.as_deref().map(Path::new);
            let probe = match bin {
                Some(bin) => {
                    gate_oauth_version(effective_auth_probe(bin).await, info.version.as_deref())
                }
                None => AuthProbe {
                    state: HarnessAuthState::Unknown,
                    method: None,
                },
            };
            info.auth_state = probe.state;
            info.auth_method = probe.method;
            if info.auth_state == HarnessAuthState::Ready {
                info.authenticated = true;
                if probe.method == Some("oauth") {
                    if let Some(acct) = config_paths()
                        .and_then(|paths| read_json(paths.metadata))
                        .and_then(|cfg| cfg.get("oauthAccount").cloned())
                    {
                        info.account = nonempty_str(&acct, "emailAddress");
                        info.org = nonempty_str(&acct, "organizationName");
                        info.plan = match nonempty_str(&acct, "billingType").as_deref() {
                            Some("stripe_subscription") => Some("Subscription".to_string()),
                            Some(other) => Some(other.to_string()),
                            None => None,
                        };
                    }
                }
            }
        }

        info.agent_ready = info.installed && info.authenticated;
        if info.agent_ready {
            // The resident child is only spawnable once the CLI is ready.
            info.supports_steering = true;
            // Ask the installed CLI for its own catalog: `list_models` for the
            // models and their per-model effort tiers, and the parser probe for
            // `ultracode` (a session mode the catalog never advertises — see
            // `claude_accepts_ultracode`). The static table only covers a CLI
            // too old to answer.
            let bin = info.bin_path.as_deref().map(Path::new);
            let (ultracode, models) = match bin {
                Some(bin) => {
                    let ultracode = claude_accepts_ultracode(bin).await;
                    (ultracode, claude_list_models(bin, ultracode).await)
                }
                None => (false, None),
            };
            info = info.with_models(models.unwrap_or_else(|| {
                let ids = claude_effort_ids(ultracode);
                CLAUDE_MODELS
                    .iter()
                    .map(|id| ModelInfo::new(*id).with_reasoning(&ids))
                    .collect()
            }));
        } else if info.auth_state == HarnessAuthState::Unsupported {
            info.agent_note = Some(
                "Update Claude Code to 2.1.211 or newer, then re-check this harness.".to_string(),
            );
        } else if info.installed {
            info.agent_note = Some(match info.auth_state {
                HarnessAuthState::Unknown if has_api_credential() =>
                    "Claude Code could not verify the effective `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`. Fix or unset it, then re-check this harness.".to_string(),
                HarnessAuthState::Unknown =>
                    "Open a terminal and run `claude auth status`, then re-check this harness.".to_string(),
                _ => auth_recovery_note().to_string(),
            });
        } else {
            info.agent_note = Some(
                "Install Claude Code (claude.com/download), then sign in with `claude auth login`."
                    .to_string(),
            );
        }
        Some(info)
    }

    async fn run_turn(&self, ctx: &mut TurnCtx) -> TurnResult {
        run_turn(ctx)
            .await
            .map(|()| TurnOutcome::Completed)
            .map_err(|error| TurnFailure::adapter(error, ctx.delivery_state()))
    }

    async fn generate_title(&self, first_message: &str) -> Option<String> {
        claude_generate_title(&find_claude()?, first_message).await
    }

    fn options(&self) -> HarnessOptions {
        // Claude owns planning as one of its five native permission modes. The
        // permission bridge makes Manual and Accept edits actionable in
        // headless mode instead of letting their prompts die unseen.
        HarnessOptions::none()
            .with_permission_choices(
                vec![
                    OptionChoice::described("manual", "Manual", "Always ask before making changes"),
                    OptionChoice::described(
                        "acceptEdits",
                        "Accept edits",
                        "Automatically accept all file edits",
                    ),
                    OptionChoice::described("plan", "Plan", "Create a plan before making changes"),
                    OptionChoice::described("auto", "Auto", "Claude handles permission decisions"),
                    OptionChoice::described(
                        "bypassPermissions",
                        "Bypass permissions",
                        "Accepts all permissions",
                    ),
                ],
                "auto",
                PlanActivation::Permission,
            )
            // Harness-wide fallback only, and deliberately the conservative
            // five: `options()` is static and can't see the detected CLI
            // version, so `ultracode` is added per-model in `detect` where the
            // version IS known. The default is `Default` (no `--effort` at all),
            // so the CLI's own configured effort survives (issue #123).
            .with_reasoning_levels(&CLAUDE_EFFORT_LEVELS)
    }

    /// Two resume paths. A card the permission bridge surfaced mid-turn
    /// (`native_id` set) settles the held bridge request — the still-running
    /// turn unblocks in place ([`ResumeAction::Handled`]), except plan
    /// approval, which interrupts the paused plan turn and resumes via a new
    /// message under the approved mode. An end-turn card (no `native_id`)
    /// resumes by sending a *new user message* under `--resume` (see
    /// `run_turn`); a denied permission is the one case with no resume.
    async fn resume_from_prompt(
        &self,
        ctx: &ResumeCtx,
        prompt: &WirePrompt,
        answer: &PromptAnswer,
    ) -> Result<ResumeAction> {
        if let Some(native_id) = &prompt.native_id {
            // The bridge request lives inside a running turn; once that turn is
            // gone the card is stale. Normally `PendingGuard` resolves it at
            // turn teardown, but a process crash/restart skips that — leaving
            // a zombie card that renders actionable and swallows every answer
            // forever. Collapse it store-side before reporting the miss.
            if !ctx.is_busy().await {
                ctx.host
                    .resolve_zombie_prompt(&ctx.session_id, &answer.prompt_id);
                return Err(anyhow!("this approval is no longer pending"));
            }
            let note = answer.note.as_deref().filter(|s| !s.trim().is_empty());
            return match (prompt.kind.as_str(), answer.approve) {
                // Mid-turn tool approval: answer the held request; the turn
                // keeps streaming. The CLI requires updatedInput on an allow —
                // echo the card's recorded input.
                ("permission", true) => {
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Allow {
                            updated_input: prompt.tool_input.clone(),
                        },
                    )?;
                    Ok(ResumeAction::Handled { plan_mode: None })
                }
                ("permission", false) => {
                    let message = match note {
                        Some(note) => format!(
                            "The user denied this action: {note}. Do not retry it; adjust course."
                        ),
                        None => "The user denied this action. Do not retry it; adjust course."
                            .to_string(),
                    };
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Deny { message },
                    )?;
                    Ok(ResumeAction::Handled { plan_mode: None })
                }
                // Deny the held ExitPlanMode. With a note it's a revision
                // request — the model revises the plan in the same turn. With
                // no note it's a plain REJECTION (the strip's Reject button):
                // tell the model to stop, not to improvise a revision (or a
                // "what should change?" question card). The wording is
                // `synthesize_resume`'s plan-deny arm verbatim — one source
                // for both delivery shapes.
                ("plan", false) => {
                    let (message, _) = synthesize_resume("plan", answer);
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Deny { message },
                    )?;
                    Ok(ResumeAction::Handled { plan_mode: None })
                }
                // Plan approval: don't settle the held request — the paused
                // plan turn gets interrupted (respond()'s SendMessage arm) and
                // replaced by a fresh implementation turn under the approved
                // mode, reusing the proven --resume machinery. The drained
                // bridge request is denied into the dying child, harmlessly.
                ("plan", true) => {
                    let (text, mode) = synthesize_resume("plan", answer);
                    Ok(ResumeAction::SendMessage {
                        text,
                        mode,
                        plan_mode: None,
                    })
                }
                // Mid-turn question (a bridge-held AskUserQuestion): the held
                // tool call is denied with the user's answer as the message —
                // the model reads the answer from the denial and continues the
                // same turn. (Allowing the tool instead would run it headless,
                // which returns no answer — the model would guess and move on
                // rather than block; that's the bug this arm exists to avoid.)
                ("question", _) => {
                    let (text, _) = synthesize_resume("question", answer);
                    if text.trim().is_empty() {
                        return Err(anyhow!("select an option (or add a note) to answer"));
                    }
                    ctx.host.settle_permission(
                        native_id,
                        crate::local::chat::PermissionDecision::Deny {
                            message: format!(
                                "The user answered: {text}. Treat this as their answer and \
                                 continue — do not ask this question again. (Only the first \
                                 question of the call was shown; ask any others separately.)"
                            ),
                        },
                    )?;
                    Ok(ResumeAction::Handled { plan_mode: None })
                }
                _ => Err(anyhow!("unsupported prompt kind for a bridge card")),
            };
        }

        // A denied permission closes the card without resuming; every other
        // answer continues the session.
        if prompt.kind == "permission" && !answer.approve {
            return Ok(ResumeAction::Nothing);
        }
        // Likewise a note-less plan REJECTION on an end-turn card: the turn is
        // already over — resuming just to say "stop" would end in fresh text
        // that `should_synthesize_plan` turns into ANOTHER card, so Reject
        // could never dismiss the strip. Close the card with no resume.
        if prompt.kind == "plan"
            && !answer.approve
            && answer.note.as_deref().is_none_or(|s| s.trim().is_empty())
        {
            return Ok(ResumeAction::Nothing);
        }
        let (text, mode) = synthesize_resume(&prompt.kind, answer);
        // Reject an empty resume (e.g. a question answered with no selection and
        // no note) so `respond` leaves the card actionable.
        if text.trim().is_empty() {
            return Err(anyhow!("no answer provided"));
        }
        Ok(ResumeAction::SendMessage {
            text,
            mode,
            plan_mode: None,
        })
    }

    fn config_home(&self) -> Option<PathBuf> {
        config_paths().map(|paths| paths.root)
    }

    fn skill_target(&self) -> Option<PathBuf> {
        Some(
            self.config_home()?
                .join("skills")
                .join("orx")
                .join("SKILL.md"),
        )
    }

    fn skill_shim(&self) -> Option<&'static str> {
        Some(super::CLAUDE_SKILL)
    }

    fn session_skills_dir(&self) -> Option<&'static str> {
        Some(".claude/skills")
    }
}

/// Internal policy → Claude Code `--permission-mode` value. Each provider-owned
/// choice already uses the CLI spelling. `Auto` is the default when the session
/// hasn't picked one.
pub(crate) fn claude_permission_mode(mode: Option<PermissionMode>) -> &'static str {
    match mode.unwrap_or(PermissionMode::Auto) {
        PermissionMode::Ask => "manual",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::Auto => "auto",
        PermissionMode::Bypass => "bypassPermissions",
    }
}

pub(crate) fn uses_permission_bridge(mode: Option<PermissionMode>) -> bool {
    matches!(
        mode,
        Some(PermissionMode::Ask | PermissionMode::AcceptEdits | PermissionMode::Plan)
    )
}

/// Path (relative to the worktree) of the plan-mode settings file we write and
/// pass via `--settings`. Lives under the same agent dir as the playbook, which
/// is already git-excluded.
const PLAN_SETTINGS_REL: &str = ".openresearch/agent/claude-plan-settings.json";

/// Path (relative to the worktree) of the plan-mode MCP config wiring the
/// `orx mcp-gate` permission bridge. Same git-excluded agent dir.
const MCP_CONFIG_REL: &str = ".openresearch/agent/claude-mcp.json";

/// Write the plan-mode `--settings` file into `repo` and return its path. The
/// file registers `PreToolUse` hooks running `orx plan-gate` (this same
/// binary): on `Bash` it allows read-only inspection through plan mode's gate,
/// and on `ExitPlanMode` it forces an `ask` — headless plan mode otherwise
/// SELF-approves the call ("User has approved exiting plan mode", nobody
/// asked; verified on claude 2.1.197) and starts editing. The `ask` routes
/// plan approval to the permission bridge card. See `plan_gate`.
///
/// The hook command is this executable's absolute path, so it resolves without
/// depending on `orx` being on Claude's `PATH`.
pub(crate) fn write_plan_settings(repo: &std::path::Path) -> Result<PathBuf> {
    let orx = std::env::current_exe()
        .map_err(|e| anyhow!("cannot resolve orx binary path for plan-mode hook: {e}"))?;
    let hook = serde_json::json!([{
        "type": "command",
        "command": format!(
            "{} plan-gate",
            crate::jobs::ssh::sh_quote(&orx.to_string_lossy())
        ),
    }]);
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "Bash", "hooks": hook },
                { "matcher": "ExitPlanMode", "hooks": hook },
            ],
        }
    });
    let path = repo.join(PLAN_SETTINGS_REL);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap())
        .map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Write the per-spawn `--mcp-config` file pointing Claude at `orx mcp-gate`
/// (this same binary) and return its path. The bridge's env block carries the
/// `orx up` port, the session id, and a fresh per-child token minted at spawn —
/// everything the resident bridge needs to relay permission requests back to
/// the running server for the child's whole life.
pub(crate) fn write_mcp_config(
    repo: &std::path::Path,
    up_port: u16,
    session_id: &str,
    token: &str,
) -> Result<PathBuf> {
    let orx = std::env::current_exe()
        .map_err(|e| anyhow!("cannot resolve orx binary path for the mcp bridge: {e}"))?;
    let config = serde_json::json!({
        "mcpServers": {
            "orx": {
                "type": "stdio",
                "command": orx.to_string_lossy(),
                "args": ["mcp-gate"],
                "env": {
                    "ORX_UP_PORT": up_port.to_string(),
                    "ORX_SESSION_ID": session_id,
                    "ORX_GATE_TOKEN": token,
                },
            },
        }
    });
    let path = repo.join(MCP_CONFIG_REL);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap())
        .map_err(|e| anyhow!("cannot write {}: {e}", path.display()))?;
    Ok(path)
}

/// Session reasoning id → Claude's `--effort` value.
///
/// Only the `default` sentinel (and an absent level) send nothing; every other
/// value is forwarded. The composer only offers what `list_models` reported
/// for the selected model (plus a probe-verified `ultracode`), so an allowlist
/// here would drop tiers a future catalog genuinely advertises — the same
/// policy as `codex_reasoning` for catalog models and `opencode_variant`.
/// Claude is also the gentlest harness to forward into: an unknown value warns
/// on stderr and runs at the default effort rather than failing the turn.
fn claude_effort(level: Option<&str>) -> Option<&str> {
    level.filter(|l| *l != REASONING_DEFAULT_ID)
}

/// The follow-up message + resume mode for an answered Claude prompt — Claude's
/// resume strategy: a prompt ends the turn and the answer becomes a *new user
/// message* that continues via `--resume`. `ChatHost` validates `resume_mode`
/// against Claude's advertised choices before this helper parses it; an absent
/// id falls through to the per-kind default. The question arm is
/// also reused as a plain text builder by the bridge's mid-turn question
/// resume (the denial message that carries the answer).
pub(crate) fn synthesize_resume(
    kind: &str,
    req: &PromptAnswer,
) -> (String, Option<PermissionMode>) {
    let note = req.note.as_deref().filter(|s| !s.trim().is_empty());
    let chosen = req.resume_mode.as_deref().and_then(PermissionMode::from_id);
    match kind {
        "plan" if req.approve => {
            let mut text = "The user approved the plan. Proceed with implementing it.".to_string();
            if let Some(note) = note {
                text.push_str(&format!("\n\nAdditional guidance: {note}"));
            }
            // Approving a plan means leaving plan mode; default to `auto`.
            (text, chosen.or(Some(PermissionMode::Auto)))
        }
        "plan" => {
            // Stay in plan mode. With a note it's a revision request; without
            // one it's a plain rejection — stop, don't guess at revisions.
            let text = note
                .map(|n| format!("Keep refining the plan: {n}"))
                .unwrap_or_else(|| {
                    "The user rejected this plan. Stop planning and wait for \
                     further instructions."
                        .to_string()
                });
            (text, Some(PermissionMode::Plan))
        }
        "permission" => {
            // Approving a blocked tool must resume under a mode that actually
            // *grants* it. Claude's `--permission-mode` is coarse: `acceptEdits`
            // only auto-approves file edits, so it leaves a Bash (or any
            // non-edit) denial in place — the tool is denied again and the card
            // re-appears in a loop. `bypassPermissions` is the only mode that lets the
            // previously-blocked tool through, so that's the default for an
            // approval (a caller can still override via `resume_mode`). Verified
            // against the CLI: acceptEdits re-denies Bash, bypassPermissions clears it.
            let text = "The user approved that action. Continue.".to_string();
            (text, chosen.or(Some(PermissionMode::Bypass)))
        }
        // question (or anything else): feed the selection back as the user's reply.
        _ => (req.contextualized_answer(req.plain_answer_text()), None),
    }
}

/// Whether a finished plan-mode turn needs a synthesized plan card: the model
/// presented its plan as plain text without calling ExitPlanMode (and without
/// asking a question), and the turn didn't error. Without a card the user is
/// stranded — only a plan-card answer switches the resume mode, so a plain
/// chat reply would resume still in plan mode. A trivial Q&A turn in plan mode
/// also gets a card: in plan mode the only exit *is* a plan answer, so the
/// card is always the recourse.
pub(crate) fn should_synthesize_plan(
    plan_mode: bool,
    saw_prompt: bool,
    errored: bool,
    final_text: &str,
) -> bool {
    plan_mode && !saw_prompt && !errored && !final_text.trim().is_empty()
}

/// ExitPlanMode → a `plan` prompt (its `input.plan` is the proposed markdown).
fn plan_prompt(name: &str, input: Option<&Value>) -> Option<WirePrompt> {
    if name != "ExitPlanMode" {
        return None;
    }
    let plan = input
        .and_then(|i| i.get("plan"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Some(WirePrompt {
        kind: "plan".into(),
        plan: Some(plan),
        ..Default::default()
    })
}

/// AskUserQuestion → a `question` prompt. Claude's schema is
/// `{questions: [{question, header, options: [{label, description}], multiSelect}]}`;
/// we surface the first question (the composer answers one at a time). Also
/// used by the plan-mode bridge (`ChatHost::request_permission`, via the
/// harness re-export) to build the held mid-turn question card.
pub(crate) fn question_prompt(name: &str, input: Option<&Value>) -> Option<WirePrompt> {
    if name != "AskUserQuestion" {
        return None;
    }
    let q = input
        .and_then(|i| i.get("questions"))
        .and_then(Value::as_array)
        .and_then(|qs| qs.first())?;
    let options = q
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    Some(WireQuestionOption {
                        label: o.get("label").and_then(Value::as_str)?.to_string(),
                        description: o
                            .get("description")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(WirePrompt {
        kind: "question".into(),
        question: q
            .get("question")
            .and_then(Value::as_str)
            .map(str::to_string),
        header: q.get("header").and_then(Value::as_str).map(str::to_string),
        options,
        multi_select: q
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ..Default::default()
    })
}

/// Claude's tool inputs are snake_case; the UI summarizes via `filePath`.
fn normalize_input(input: &Value) -> Value {
    let mut input = input.clone();
    if let Some(obj) = input.as_object_mut() {
        if let Some(fp) = obj.get("file_path").cloned() {
            obj.entry("filePath").or_insert(fp);
        }
    }
    input
}

/// tool_result content: plain string or [{type: "text", text}] blocks.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// True for the stdout acknowledgment of the exact stdin message we sent
/// (`--replay-user-messages` re-emits it, string or single-text-block form,
/// byte-exact — verified live on claude 2.1.212). The echo also carries an
/// undocumented `isReplay: true`; content equality is deliberately the sole
/// discriminator, so a future CLI dropping that field changes nothing. A
/// mid-turn synthetic `user` event (a tool_result) can't collide: those carry
/// `tool_result` blocks, never a single text block equal to the prompt.
fn replays_user_message(event: &Value, text: &str) -> bool {
    if event.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    match event.pointer("/message/content") {
        Some(Value::String(content)) => content == text,
        Some(Value::Array(blocks)) => {
            blocks.len() == 1
                && blocks[0].get("type").and_then(Value::as_str) == Some("text")
                && blocks[0].get("text").and_then(Value::as_str) == Some(text)
        }
        _ => false,
    }
}

/// Whether an event is part of the turn we submitted, tracked through
/// `saw_user_echo`: everything before the child echoes our user message is
/// `--resume`/startup replay of prior-session output, if any — fatally
/// including a stale `result` that would end the turn instantly. The echo
/// itself is consumed (returns `false`); everything after it belongs to the
/// turn.
fn belongs_to_current_turn(event: &Value, text: &str, saw_user_echo: &mut bool) -> bool {
    if *saw_user_echo {
        return true;
    }
    *saw_user_echo = replays_user_message(event, text);
    false
}

fn observe_turn_boundary(event: &Value, text: &str, saw_user_echo: &mut bool) -> (bool, bool) {
    let had_user_echo = *saw_user_echo;
    let belongs = belongs_to_current_turn(event, text, saw_user_echo);
    (belongs, !had_user_echo && *saw_user_echo)
}

/// The per-turn state `apply_event` folds each stream-json line into. Kept
/// store-free so the caller (not the fold) owns every side effect — the native
/// session id is committed only after an accepted attempt, and every flush happens there
/// too, which is what lets the fold run against a bare `TurnCtx::test_stub()` in
/// the fixture tests.
#[derive(Default)]
struct TurnState {
    /// Whether this child was spawned with the mcp-gate bridge active. With the
    /// bridge on, ExitPlanMode / AskUserQuestion come from the bridge (held,
    /// mid-turn-answerable), so their `tool_use` renders nothing.
    bridge_active: bool,
    /// At least one `result` event has landed. With background tasks a turn
    /// spans several segments, each with its own result — this validates the
    /// stream ended shaped like a turn, not that a given result was terminal.
    saw_result: bool,
    /// An interactive card was surfaced this turn (suppresses the synthesized
    /// plan card — see `should_synthesize_plan`).
    saw_prompt: bool,
    /// The turn ended with a genuine failure (drives the error path).
    turn_errored: bool,
    /// Claude's typed headless auth failure. Its synthetic assistant text is
    /// suppressed and the resident child is quarantined by the caller.
    auth_failed: bool,
    /// Any real output or tool activity makes transparent resubmission unsafe.
    had_activity: bool,
    /// The last non-empty assistant text block — the plan, if the model wrote
    /// one as plain text.
    last_text: String,
    /// The provisional native session id from the latest `system/init` or
    /// `result`. An auth-failed attempt never commits it to the store.
    native_session_id: Option<String>,
    /// The in-flight assistant message id from the stream's `message_start` —
    /// deltas carry only a block `index`, so this is what keys them to the
    /// same `{mid}-{index}` part ids the final complete `assistant` event
    /// upserts.
    stream_mid: Option<String>,
    /// Per sub-agent (keyed by `parent_tool_use_id`), the in-flight message id of
    /// that sub-agent's stream — the child equivalent of `stream_mid`, so two
    /// concurrent sub-agents' deltas key to distinct child part ids.
    sub_stream_mid: std::collections::HashMap<String, String>,
    /// Content blocks already consumed from prior `assistant` events, keyed
    /// per message id (subagent events namespaced by `parent_tool_use_id`) —
    /// see the `assistant` arm for why this offset exists.
    assistant_blocks_seen: HashMap<String, usize>,
    /// Background (`local_agent`) tasks spawned this turn that haven't reached
    /// their terminal `task_notification` yet, task_id → spawning tool_use_id.
    /// A `result` while entries remain is a segment boundary, not the end of
    /// the turn — the CLI auto-resumes with the task's report once it
    /// finishes, and ending the turn there would silently drop that whole
    /// continuation. The tool_use_id side keeps the spawn part `running` (the
    /// async launch acknowledgement would otherwise complete it at launch and
    /// kill every running indicator while the agent works).
    pending_tasks: HashMap<String, Option<String>>,
    /// Whether any background task ran this turn (stays true after completion)
    /// — gates the post-result grace wait for the auto-resume segment, which
    /// can trail the result even when every task already finished.
    saw_background_task: bool,
}

/// The spawning `Task` tool_use id for a sub-agent event (`parent_tool_use_id`),
/// or `None` for a main-loop event. Claude Code tags every sub-agent
/// `stream_event`/`assistant`/`user` with this — and its value *is* the Task
/// part's WirePart id, so it directly names the spawn part the sub-agent's
/// transcript hangs under (no thread→part map needed, unlike Codex).
fn subagent_parent(event: &Value) -> Option<&str> {
    event.get("parent_tool_use_id").and_then(Value::as_str)
}

/// Route a sub-agent `assistant` message's content blocks into the spawning Task
/// part's `children` (ids namespaced by `parent` so concurrent sub-agents can't
/// collide). Mirrors the main-loop block handling minus the interactive-prompt /
/// bridge special-casing, which only applies to the main session. `start` is the
/// message-cumulative index of this event's first block: the CLI emits one
/// `assistant` event per content block (see the main-loop arm), so each event
/// continues the message where the last left off — the offset shifts the ids,
/// it does NOT skip elements of this event's (usually single-element) array.
fn apply_subagent_blocks(
    ctx: &mut TurnCtx,
    parent: &str,
    mid: &str,
    start: usize,
    blocks: &[Value],
) {
    for (n, block) in blocks.iter().enumerate() {
        let i = start + n;
        let ns = |id: &str| format!("{parent}:{id}");
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    ctx.upsert_child(parent, WirePart::text(ns(&format!("{mid}-{i}")), text));
                }
            }
            Some("thinking") => {
                // An empty text/thinking block (encrypted reasoning) must not
                // mint an invisible child part, nor blank one the deltas built.
                let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    ctx.upsert_child(parent, WirePart::reasoning(ns(&format!("{mid}-{i}")), text));
                }
            }
            Some("tool_use") => {
                let raw_id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{mid}-{i}"));
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let input = block.get("input");
                ctx.upsert_child(
                    parent,
                    WirePart {
                        id: ns(&raw_id),
                        kind: "tool".into(),
                        text: None,
                        tool: Some(name.to_string()),
                        state: Some(WireToolState {
                            status: "running".into(),
                            input: input.map(normalize_input),
                            output: None,
                            error: None,
                            title: None,
                        }),
                        prompt: None,
                        children: Vec::new(),
                    },
                );
            }
            _ => {}
        }
    }
}

/// Fold one stream-json output object into the turn's transcript + `TurnState`.
/// Pure w.r.t. the store — touches only `ctx.assistant.parts` (via the TurnCtx
/// helpers) and `state` — so it is fixture-tested against `TurnCtx::test_stub()`.
/// Returns `true` when this event is the terminal `result` (the caller stops
/// the recv loop). Native-session-id application and flushing are the caller's
/// job, keeping this store-free.
fn apply_event(ctx: &mut TurnCtx, state: &mut TurnState, event: &Value) -> bool {
    match event.get("type").and_then(Value::as_str) {
        // Partial-message deltas (opt-in via --include-partial-messages): the
        // text/thinking streams token by token instead of landing as one block
        // when the complete `assistant` event arrives. Deltas build a part
        // under the same `{mid}-{index}` id that the assistant event upserts,
        // so the authoritative full text simply overwrites the accumulated
        // one (the `assistant` arm reconstructs the block index from a
        // running offset). That overwrite (and part ordering) leans on two stream-protocol
        // invariants: the stream's message id equals the assistant events',
        // and a block's `index` is its position in the message's content,
        // with blocks streamed in ascending order.
        Some("stream_event") => {
            // A subagent's nested stream carries `parent_tool_use_id` (the id of
            // the spawning Task tool_use — which is the Task part's own id). Its
            // deltas stream into that part's `children`; a main-loop delta (no
            // parent) streams into the top-level transcript as before.
            let parent = subagent_parent(event);
            let inner = event.get("event").unwrap_or(&Value::Null);
            match inner.get("type").and_then(Value::as_str) {
                Some("message_start") => {
                    // Sub-agent streams have their own message ids; namespace the
                    // stream mid per parent so a concurrent sub-agent's deltas
                    // don't collide with the main stream's.
                    if let Some(mid) = inner.pointer("/message/id").and_then(Value::as_str) {
                        match parent {
                            Some(p) => {
                                state.sub_stream_mid.insert(p.to_string(), mid.to_string());
                            }
                            None => state.stream_mid = Some(mid.to_string()),
                        }
                    }
                }
                Some("content_block_delta") => {
                    let mid = match parent {
                        Some(p) => state.sub_stream_mid.get(p).map(String::as_str),
                        None => state.stream_mid.as_deref(),
                    };
                    let (Some(mid), Some(index)) =
                        (mid, inner.get("index").and_then(Value::as_u64))
                    else {
                        return false;
                    };
                    let delta = inner.get("delta").unwrap_or(&Value::Null);
                    let (reasoning, field) = match delta.get("type").and_then(Value::as_str) {
                        Some("text_delta") => (false, "text"),
                        Some("thinking_delta") => (true, "thinking"),
                        _ => return false,
                    };
                    let Some(text) = delta.get(field).and_then(Value::as_str) else {
                        return false;
                    };
                    // An empty delta has nothing to append and must not mint a
                    // part: an empty part renders nothing but still breaks
                    // tool-run grouping in the UI. Encrypted-reasoning models
                    // (e.g. Fable) stream exactly this — every `thinking_delta`
                    // is empty, the payload rides in `signature_delta`. (The UI
                    // also skips invisible parts, for transcripts stored before
                    // this guard existed; both layers are required.)
                    if text.is_empty() {
                        return false;
                    }
                    state.had_activity = true;
                    match parent {
                        Some(p) => {
                            // Namespace the child id by parent so two sub-agents'
                            // block indices can't collide.
                            let id = format!("{p}:{mid}-{index}");
                            ctx.append_child_text(p, &id, text, || {
                                if reasoning {
                                    WirePart::reasoning(id.clone(), "")
                                } else {
                                    WirePart::text(id.clone(), "")
                                }
                            });
                        }
                        None => {
                            let id = format!("{mid}-{index}");
                            if !ctx.assistant.parts.iter().any(|p| p.id == id) {
                                ctx.upsert_part(if reasoning {
                                    WirePart::reasoning(id.clone(), "")
                                } else {
                                    WirePart::text(id.clone(), "")
                                });
                            }
                            ctx.append_part_text(&id, text);
                        }
                    }
                }
                _ => {}
            }
        }
        Some("system") => match event.get("subtype").and_then(Value::as_str) {
            Some("init") => {
                if let Some(sid) = event.get("session_id").and_then(Value::as_str) {
                    state.native_session_id = Some(sid.to_string());
                }
            }
            // Track background sub-agents so the `result` arm knows a turn
            // isn't over while one still runs (see `pending_tasks`).
            Some("task_started") => {
                if event.get("task_type").and_then(Value::as_str) == Some("local_agent") {
                    if let Some(id) = event.get("task_id").and_then(Value::as_str) {
                        // No tool_use_id → no spawn-part association; the
                        // task still gates the turn's end.
                        let tool_id = event
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        state.pending_tasks.insert(id.to_string(), tool_id);
                        state.saw_background_task = true;
                    }
                }
            }
            // The task's real end: retire it and stamp the spawn part terminal
            // — the async-launch tool_result deliberately left it `running`.
            Some("task_notification") => {
                if let Some(id) = event.get("task_id").and_then(Value::as_str) {
                    let tool_id = state.pending_tasks.remove(id).flatten();
                    if let Some(part) = tool_id
                        .as_deref()
                        .and_then(|tid| find_part_mut(&mut ctx.assistant.parts, tid))
                    {
                        if let Some(part_state) = part.state.as_mut() {
                            if part_state.status == "running" {
                                let ok = event.get("status").and_then(Value::as_str)
                                    == Some("completed");
                                part_state.status = if ok { "completed" } else { "error" }.into();
                                if !ok {
                                    // Give the failure a real message — the
                                    // spawn tool's own result was only the
                                    // launch acknowledgement.
                                    part_state.error = Some(
                                        event
                                            .get("summary")
                                            .and_then(Value::as_str)
                                            .unwrap_or("The background agent failed")
                                            .to_string(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        },
        Some("assistant") => {
            if event
                .get("error")
                .or_else(|| event.pointer("/message/error"))
                .and_then(Value::as_str)
                == Some("authentication_failed")
            {
                state.auth_failed = true;
                state.turn_errored = true;
                return false;
            }
            let mid = event
                .pointer("/message/id")
                .and_then(Value::as_str)
                .unwrap_or("m")
                .to_string();
            let blocks = event
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            // The CLI emits one `assistant` event per content block (a
            // single-element `content` array), so each event continues the
            // message where the last left off; a message's first event starts
            // at offset 0, so a single full-content array behaves the same.
            // The counter trusts the CLI to emit each block exactly once, in
            // order — the events carry no wire index to cross-check. A
            // subagent's events (parent_tool_use_id set) get their own
            // namespace so they can never advance the main message's offset.
            let parent = subagent_parent(event).map(str::to_string);
            let offset_key = match parent.as_deref() {
                Some(parent) => format!("{parent}:{mid}"),
                None => mid.clone(),
            };
            let offset = state
                .assistant_blocks_seen
                .get(&offset_key)
                .copied()
                .unwrap_or(0);
            // A sub-agent's message: route its blocks into the spawning Task
            // part's `children` (namespaced ids, same offset), never into the
            // top-level transcript, and skip prompt-card / usage handling (a
            // sub-agent drives neither the main interactive flow nor its meter).
            if let Some(parent) = parent.as_deref() {
                apply_subagent_blocks(ctx, parent, &mid, offset, &blocks);
                state
                    .assistant_blocks_seen
                    .insert(offset_key, offset + blocks.len());
                return false;
            }
            for (n, block) in blocks.iter().enumerate() {
                let i = offset + n;
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.trim().is_empty() {
                            state.last_text = text.to_string();
                        }
                        // Empty-block guard, same as the thinking arm below.
                        if !text.is_empty() {
                            state.had_activity = true;
                            ctx.upsert_part(WirePart::text(format!("{mid}-{i}"), text));
                        }
                    }
                    Some("thinking") => {
                        // An empty block (encrypted reasoning sends
                        // `thinking: ""` — the content is in `signature`) must
                        // not mint an invisible part that splits tool-run
                        // grouping, nor wipe text the deltas already built.
                        let text = block.get("thinking").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            state.had_activity = true;
                            ctx.upsert_part(WirePart::reasoning(format!("{mid}-{i}"), text));
                        }
                    }
                    Some("tool_use") => {
                        state.had_activity = true;
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .map_or_else(|| format!("{mid}-{i}"), str::to_string);
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let input = block.get("input");
                        // ExitPlanMode / AskUserQuestion surface as interactive
                        // prompt cards instead of plain tool rows, and the user's
                        // choice resumes the session. With the bridge active,
                        // BOTH cards come from the bridge instead (held,
                        // mid-turn-answerable) and the tool_use renders NOTHING:
                        // a tool row would duplicate the card — and the denial
                        // that carries the user's answer back would paint it as a
                        // spurious error row once the tool_result lands.
                        if state.bridge_active && matches!(name, "ExitPlanMode" | "AskUserQuestion")
                        {
                            continue;
                        }
                        if let Some(prompt) =
                            plan_prompt(name, input).or_else(|| question_prompt(name, input))
                        {
                            state.saw_prompt = true;
                            ctx.upsert_part(WirePart::prompt(id, prompt));
                        } else {
                            // Preserve children: a Task tool_use re-sent after its
                            // sub-agent already streamed must not drop the nested
                            // transcript. (Plain tools have no children, so this is
                            // an ordinary upsert for them.)
                            ctx.upsert_part_preserving_children(WirePart {
                                id,
                                kind: "tool".into(),
                                text: None,
                                tool: Some(name.to_string()),
                                state: Some(WireToolState {
                                    status: "running".into(),
                                    input: input.map(normalize_input),
                                    output: None,
                                    error: None,
                                    title: None,
                                }),
                                prompt: None,
                                children: Vec::new(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            // Advance by the content-array length, not a render count: a
            // bridge-suppressed ExitPlanMode/AskUserQuestion or an unknown
            // block type still occupies its position in the message.
            state
                .assistant_blocks_seen
                .insert(offset_key, offset + blocks.len());
            // Per-message usage gives live updates during multi-step turns; the
            // window arrives later on `result`, so report the token count only.
            // (Sub-agent messages returned early above, so their smaller counts
            // never reach — and never clobber — the main session's meter.)
            if let Some(used) = claude_used_tokens(event.pointer("/message/usage")) {
                state.had_activity |= used > 0;
                ctx.report_usage(ContextUsage {
                    used_tokens: used,
                    context_window: None,
                });
            }
        }
        Some("user") => {
            // Synthetic tool-result turns: complete the matching tool part. A
            // sub-agent's tool_result carries the bare `tool_use_id`; its part
            // lives in the spawning Task part's children under the namespaced id.
            let parent = subagent_parent(event);
            let blocks = event
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for block in &blocks {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                state.had_activity = true;
                let Some(tool_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                let is_error = block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let text = block.get("content").map(result_text).unwrap_or_default();
                let part_id = match parent {
                    Some(p) => format!("{p}:{tool_id}"),
                    None => tool_id.to_string(),
                };
                // An async spawn's immediate acknowledgement ("Async agent
                // launched…") is not the call's real end — the agent is still
                // running, and completing the part here would kill every
                // running indicator for it. It's internal metadata, not a
                // report, so it isn't stored either; the terminal
                // task_notification stamps the part. (Sync spawns are never
                // pending here: their task_notification precedes this result.)
                let launch_ack = parent.is_none()
                    && !is_error
                    && state
                        .pending_tasks
                        .values()
                        .any(|tid| tid.as_deref() == Some(part_id.as_str()));
                if launch_ack {
                    continue;
                }
                if let Some(part) = find_part_mut(&mut ctx.assistant.parts, &part_id) {
                    if let Some(state) = part.state.as_mut() {
                        state.status = if is_error { "error" } else { "completed" }.into();
                        if is_error {
                            state.error = Some(text.clone());
                        } else {
                            state.output = Some(text.clone());
                        }
                    }
                }
            }
        }
        Some("result") => {
            state.saw_result = true;
            // Resume mints a fresh session id per turn — track the latest.
            if let Some(sid) = event.get("session_id").and_then(Value::as_str) {
                state.native_session_id = Some(sid.to_string());
            }
            // We deliberately do NOT turn `permission_denials` into approve-me
            // cards. Headless has no interactive approval, and of the modes we
            // offer, only Plan produces denials — and those are *expected*
            // (read-only by design). Surfacing an "Allow" that re-ran the turn
            // under bypass would silently defeat plan mode. The model already
            // narrates the block in text; the recourse is approving the plan
            // (the ExitPlanMode card), which leaves plan mode. Auto/Bypass never
            // deny in the first place.
            //
            // A plan pause is NOT a failure: the CLI records the blocked tools
            // in `permission_denials` but still reports `subtype: "success"` /
            // `is_error: false`. So drive the error path off the result status
            // alone — a genuine failure is still surfaced.
            let subtype = event.get("subtype").and_then(Value::as_str).unwrap_or("");
            let is_error = event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(subtype != "success");
            if is_error {
                state.turn_errored = true;
                if !state.auth_failed {
                    let detail = event
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or(subtype)
                        .to_string();
                    ctx.push_error(format!("claude: {detail}"));
                }
            }
            state.had_activity |= report_result_usage(ctx, event).is_some_and(|used| used > 0);
            // A result while background sub-agents still run is only a segment
            // boundary: the model ended ITS reply, but the CLI auto-resumes
            // with the agents' reports when they finish. Keep listening — the
            // continuation's own result (no tasks pending) ends the turn.
            if !state.pending_tasks.is_empty() {
                return false;
            }
            return true;
        }
        _ => {}
    }
    false
}

/// Sum the four token buckets of a Claude `usage` object into the context-window
/// occupancy of that request (input + cache read + cache write + output).
/// Returns `None` when the object is absent or carries none of the fields.
fn claude_used_tokens(usage: Option<&Value>) -> Option<u64> {
    let usage = usage?;
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    let keys = [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ];
    if keys.iter().all(|k| field(k).is_none()) {
        return None;
    }
    Some(keys.iter().filter_map(|k| field(k)).sum())
}

/// Fold the terminal `result` event's usage into the meter: `modelUsage` reports
/// the context window (keyed by model id — pick the turn's model, else the
/// largest entry, so subagent models don't win); occupancy comes from the
/// `assistant` usage already captured this turn, else the last
/// `usage.iterations[]` entry, else the top-level `usage` aggregate.
fn report_result_usage(ctx: &mut TurnCtx, event: &Value) -> Option<u64> {
    let model_usage = event.get("modelUsage").and_then(Value::as_object);
    let entry = model_usage.and_then(|map| {
        map.get(ctx.model.as_deref().unwrap_or_default())
            .or_else(|| {
                map.values().max_by_key(|v| {
                    v.get("inputTokens").and_then(Value::as_u64).unwrap_or(0)
                        + v.get("outputTokens").and_then(Value::as_u64).unwrap_or(0)
                        + v.get("cacheReadInputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                        + v.get("cacheCreationInputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0)
                })
            })
    });
    let context_window = entry
        .and_then(|e| e.get("contextWindow"))
        .and_then(Value::as_u64);
    // Prefer usage already captured from an `assistant` event; else fall back to
    // the LAST iteration (the aggregate overstates a multi-iteration context).
    let iteration_used = event
        .pointer("/usage/iterations")
        .and_then(Value::as_array)
        .and_then(|it| it.last())
        .and_then(|last| claude_used_tokens(Some(last)));
    let event_used = iteration_used.or_else(|| claude_used_tokens(event.get("usage")));
    let used = ctx
        .context_usage
        .as_ref()
        .map(|u| u.used_tokens)
        .or(event_used);
    if let Some(used) = used {
        ctx.report_usage(ContextUsage {
            used_tokens: used,
            context_window,
        });
    }
    event_used
}

fn commit_attempt_session(ctx: &mut TurnCtx, state: &TurnState) {
    if !state.auth_failed || state.had_activity {
        if let Some(sid) = state.native_session_id.as_deref() {
            ctx.set_native_session_id(sid);
        }
    }
}

/// The reasoning-level → `--effort` value the spawn config carries. Split out so
/// the harness and the host agree on exactly what the child was launched with.
fn spawn_config(ctx: &TurnCtx) -> SpawnConfig {
    SpawnConfig {
        permission_mode: ctx.permission_mode,
        effort: claude_effort(ctx.reasoning_level.as_deref()).map(str::to_string),
        model: ctx.model.clone(),
        // The turn WANTS the bridge iff it's plan mode under a bound `orx up`
        // port; whether the bridge was actually achieved is recorded on the
        // spawned child's config (a failed write leaves it false and the next
        // plan turn respawns). Keeping the wanted value here means a plan turn
        // reconciles against a child that already has the bridge and reuses it.
        bridge_active: uses_permission_bridge(ctx.permission_mode) && ctx.host.up_port().is_some(),
    }
}

/// Deadline for a turn's first sign of life — any stdout line at all, echo or
/// pre-echo. A healthy child (resident or freshly spawned, `--resume`
/// rehydration included) emits its per-turn `system`/`init` within seconds of
/// a message; TOTAL silence this long means a wedged child (seen live: a
/// resident child stuck in an expired-OAuth refresh). From the first line on,
/// waits get the shared [`super::TURN_WATCHDOG`] instead (a long-running tool
/// or an API backoff burst emits nothing for minutes, legitimately).
const FIRST_EVENT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long after a `result` to wait for the CLI's background-task auto-resume
/// segment before declaring the turn over. The continuation is queued locally
/// by the CLI, so it arrives within milliseconds when it exists at all.
const BACKGROUND_RESUME_GRACE: Duration = Duration::from_secs(3);

async fn run_attempt(ctx: &mut TurnCtx, spec: SpawnSpec) -> Result<(TurnState, u64)> {
    let (route, mut rx) = loop {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let acquire = ctx.host.claude.acquire_turn(spec.clone(), tx);
        let result = match ctx.orx_retry_remaining() {
            Some(remaining) => tokio::time::timeout(remaining, acquire)
                .await
                .map_err(|_| anyhow!("Claude Code setup exceeded the ORX retry budget"))?,
            None => acquire.await,
        };
        match result {
            Ok(route) => {
                ctx.clear_retry_status();
                break (route, rx);
            }
            Err(error) => {
                ctx.host.claude.kill_session(&ctx.session_id).await;
                let Some((retry_number, delay)) = ctx.schedule_orx_retry(None) else {
                    ctx.mark_delivery(DeliveryState::NotSent);
                    ctx.mark_terminal_failure("claude_setup", error.to_string());
                    return Err(error);
                };
                ctx.show_retry_status(
                    "orx",
                    "Restarting Claude Code",
                    retry_number as i64 + 1,
                    Some(ORX_MAX_ATTEMPTS as i64),
                    Some(crate::store::now_ms() + delay.as_millis() as i64),
                );
                tokio::time::sleep(delay).await;
            }
        }
    };
    let client = route.client();
    let auth_generation = client.auth_generation();
    let bridge_active = client.config().bridge_active;
    ctx.persist_delivery(DeliveryState::Unknown)?;
    if let Err(e) = client.send_user_message(&ctx.text).await {
        ctx.host.claude.kill_session(&ctx.session_id).await;
        return Err(anyhow!(
            "claude stdin write failed: {e}; see {}",
            crate::store::data_dir().join("agent-claude.log").display()
        ));
    }

    let mut state = TurnState {
        bridge_active,
        ..Default::default()
    };
    let mut saw_event = false;
    let mut saw_user_echo = false;
    // Absolute, so steering can't push it back: a wedged child that the user
    // keeps typing at must still trip the detector.
    let mut deadline = tokio::time::Instant::now() + FIRST_EVENT_TIMEOUT;
    // An event received during the post-result grace wait below, replayed at
    // the top of the next iteration.
    let mut queued: Option<TurnEvent> = None;
    loop {
        // A continuation stashed by the grace wait replays without waiting.
        // Otherwise `steering` is borrowed out of `ctx` so the borrow ends with
        // the select — the steer arm's handler needs `&mut ctx`.
        let waited = match queued.take() {
            Some(event) => Waited::Event(Ok(Some(event))),
            None => {
                let steering = &mut ctx.steering;
                tokio::select! {
                    event = tokio::time::timeout_at(deadline, rx.recv()) => Waited::Event(event),
                    steer = super::next_steer(steering) => Waited::Steer(steer),
                }
            }
        };
        let event = match waited {
            Waited::Steer(steer) => {
                // A failed write means the child is gone and this turn is about
                // to error out — park the text so it survives into the next one.
                match client.send_user_message(&steer.text).await {
                    Ok(()) => ctx.record_steer(&steer.display),
                    Err(_) => {
                        if let Err(error) = ctx.host.park_steer(&ctx.session_id, steer) {
                            ctx.push_error(format!("Could not preserve steering message: {error}"));
                        }
                    }
                }
                continue;
            }
            Waited::Event(Ok(event)) => {
                deadline = tokio::time::Instant::now() + super::TURN_WATCHDOG;
                event
            }
            // Card think-time is unbounded by design: re-arm, or an elapsed
            // absolute deadline would spin this arm instead of waiting.
            Waited::Event(Err(_)) if ctx.host.has_pending_permission(&ctx.session_id) => {
                deadline = tokio::time::Instant::now()
                    + if saw_event {
                        super::TURN_WATCHDOG
                    } else {
                        FIRST_EVENT_TIMEOUT
                    };
                continue;
            }
            Waited::Event(Err(_)) => {
                commit_attempt_session(ctx, &state);
                ctx.host.claude.kill_session(&ctx.session_id).await;
                let _ = ctx.flush();
                let (what, hint) = if saw_event {
                    (
                        format!(
                            "went quiet for {} minutes",
                            super::TURN_WATCHDOG.as_secs() / 60
                        ),
                        "",
                    )
                } else {
                    (
                        format!("produced no output for {}s", FIRST_EVENT_TIMEOUT.as_secs()),
                        " (check Claude Code authentication in Harnesses)",
                    )
                };
                return Err(anyhow!(
                    "claude {what} — killed the wedged child{hint}. Sending another message \
                     resumes the session; see {}",
                    crate::store::data_dir().join("agent-claude.log").display()
                ));
            }
        };
        let Some(event) = event else {
            break;
        };
        match event {
            TurnEvent::Line(value) => {
                let (belongs, accepted_now) =
                    observe_turn_boundary(&value, &ctx.text, &mut saw_user_echo);
                if accepted_now {
                    ctx.mark_delivery(DeliveryState::Accepted);
                }
                if !belongs {
                    saw_event = true;
                    if value.get("type").and_then(Value::as_str) == Some("system")
                        && value.get("subtype").and_then(Value::as_str) == Some("init")
                    {
                        if let Some(sid) = value.get("session_id").and_then(Value::as_str) {
                            state.native_session_id = Some(sid.to_string());
                        }
                    }
                    continue;
                }
                saw_event = true;
                let done = apply_event(ctx, &mut state, &value);
                if state.had_activity {
                    commit_attempt_session(ctx, &state);
                }
                ctx.maybe_flush();
                if done {
                    // A turn that spawned background sub-agents can auto-resume
                    // right AFTER its result (the CLI queues a completed task's
                    // report as a fresh segment: init + messages + another
                    // result). Give that continuation a short window to show up
                    // before ending the turn; a quiet window means the report
                    // was already delivered in this segment.
                    if !state.saw_background_task {
                        break;
                    }
                    match tokio::time::timeout(BACKGROUND_RESUME_GRACE, rx.recv()).await {
                        Ok(Some(event)) => queued = Some(event),
                        _ => break,
                    }
                }
            }
            TurnEvent::Closed => {
                commit_attempt_session(ctx, &state);
                ctx.host.claude.kill_session(&ctx.session_id).await;
                let _ = ctx.flush();
                return Err(anyhow!(
                    "claude exited mid-turn; see {}",
                    crate::store::data_dir().join("agent-claude.log").display()
                ));
            }
        }
    }

    if !state.saw_result {
        commit_attempt_session(ctx, &state);
        ctx.host.claude.kill_session(&ctx.session_id).await;
        let _ = ctx.flush();
        return Err(anyhow!(
            "claude ended the turn without a result; see {}",
            crate::store::data_dir().join("agent-claude.log").display()
        ));
    }

    if state.auth_failed && !saw_user_echo {
        ctx.mark_delivery(DeliveryState::Rejected);
    }
    commit_attempt_session(ctx, &state);
    Ok((state, auth_generation))
}

async fn run_turn(ctx: &mut TurnCtx) -> Result<()> {
    let project = ctx.project.clone();
    let session_id = ctx.session_id.clone();
    // The modular orx skills land in the harness's session-skills dir, fresh,
    // for this session's agent to auto-load — source of truth is the trait. Run
    // per turn (worktree + skills refresh); the resident child re-reads the
    // playbook only on respawn, same tradeoff as codex (see opencode.rs's
    // playbook-freshness comment).
    let skills_dir = ClaudeCode.session_skills_dir();
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id, skills_dir))
            .await
            .map_err(|e| anyhow!("playbook task failed: {e}"))??;

    let plan_mode = ctx.permission_mode == Some(PermissionMode::Plan);
    // Clear any bridge-card flag a previous aborted turn left behind so it can't
    // suppress this turn's fallback.
    let _ = ctx.host.take_bridge_prompted(&ctx.session_id);
    // Sweep zombie HELD cards (native_id) a crashed/restarted process left
    // unresolved: they can never be answered again, and once this turn makes the
    // session busy one could capture the composer's typed-text routing. End-turn
    // cards are deliberately left alone — they resume via --resume.
    let _ = ctx.host.resolve_stale_prompts(&ctx.session_id, true).await;

    let resume = ctx.native_session_id.clone();
    let base_spec = SpawnSpec {
        chat: ctx.host.clone(),
        session_id: ctx.session_id.clone(),
        repo,
        playbook,
        resume: resume.clone(),
        config: spawn_config(ctx),
    };
    let mut retry_count = 0;
    let mut state;
    loop {
        // Always resume from the last known-good session id. An auth-failed
        // process can emit a synthetic init id that has no persisted history.
        let mut spec = base_spec.clone();
        spec.resume = resume.clone();
        let (attempt, failed_generation) = run_attempt(ctx, spec).await?;
        if !attempt.auth_failed {
            state = attempt;
            break;
        }

        let auth = if retry_count == 0 {
            ctx.host
                .claude
                .recover_auth_failure(&ctx.session_id, failed_generation)
                .await
        } else {
            ctx.host
                .claude
                .reject_auth_generation(&ctx.session_id, failed_generation)
                .await
        };
        let snapshot = ctx.host.claude.auth_snapshot();
        if ctx.host.claude.claim_auth_announcement(snapshot.generation) {
            ctx.host.emit_event(
                "harness.auth",
                serde_json::json!({ "harness": "claude-code", "authState": snapshot.state }),
            );
        }
        if auth == HarnessAuthState::Ready
            && !attempt.had_activity
            && retry_count == 0
            && ctx.delivery_state() == crate::local::chat::DeliveryState::Rejected
        {
            retry_count += 1;
            let Some((retry_number, delay)) = ctx.schedule_orx_retry(None) else {
                state = attempt;
                ctx.mark_terminal_failure(
                    "claude_auth",
                    "Claude Code authentication recovery exhausted the ORX retry budget",
                );
                break;
            };
            ctx.show_retry_status(
                "orx",
                "Claude Code authentication recovered",
                retry_number as i64 + 1,
                Some(ORX_MAX_ATTEMPTS as i64),
                Some(crate::store::now_ms() + delay.as_millis() as i64),
            );
            tokio::time::sleep(delay).await;
            continue;
        }

        let detail = match auth {
            HarnessAuthState::NeedsLogin => {
                "Claude Code sign-in required. Run `claude auth login`, then retry this message."
            }
            HarnessAuthState::Unknown => {
                "Claude Code authentication could not be verified. Run `claude auth status`, then re-check the harness."
            }
            _ => "Claude Code authentication failed. Re-check the harness, then retry this message.",
        };
        ctx.push_error(detail.to_string());
        state = attempt;
        break;
    }

    // The model sometimes ends a plan-mode turn with its plan as plain text and
    // no ExitPlanMode call. Headless leaves no way out of plan mode then — only
    // a plan-card answer switches the resume mode, so a chat "yes" would resume
    // still read-only. Synthesize a card from the final text so approval always
    // has a handle. A plan/permission card the bridge surfaced mid-turn counts
    // as "saw a prompt" (e.g. keep-planning continued this same turn); a mid-turn
    // *question* deliberately does not — its answer is no exit recourse, and the
    // turn may still end with a texty plan.
    let saw_prompt = state.saw_prompt || ctx.host.take_bridge_prompted(&ctx.session_id);
    if should_synthesize_plan(plan_mode, saw_prompt, state.turn_errored, &state.last_text) {
        ctx.upsert_part(WirePart::prompt(
            format!("plan-synth-{}", ctx.assistant.id),
            WirePrompt {
                kind: "plan".into(),
                plan: Some(std::mem::take(&mut state.last_text)),
                synthesized: true,
                ..Default::default()
            },
        ));
    }
    if state.turn_errored {
        let message = ctx
            .assistant
            .parts
            .iter()
            .rev()
            .find_map(|part| part.state.as_ref()?.error.clone())
            .unwrap_or_else(|| "Claude Code reported a terminal turn error".into());
        ctx.mark_terminal_failure("claude_terminal", message);
    }
    let _ = ctx.flush();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::options::REASONING_DEFAULT_ID;
    use super::*;

    #[test]
    fn config_override_moves_the_root_and_metadata_together() {
        let custom = PathBuf::from("/custom/claude");
        assert_eq!(
            resolve_config_paths(Some(custom.clone()), Some(PathBuf::from("/home/user"))),
            Some(ClaudeConfigPaths {
                root: custom.clone(),
                metadata: custom.join(".claude.json"),
            })
        );
        assert_eq!(
            resolve_config_paths(None, Some(PathBuf::from("/home/user"))),
            Some(ClaudeConfigPaths {
                root: PathBuf::from("/home/user/.claude"),
                metadata: PathBuf::from("/home/user/.claude.json"),
            })
        );
    }

    /// A `list_models` response in the live 2.1.212 shape (fields we don't
    /// read trimmed). Covers the four things the parser decides: the `default`
    /// entry is skipped, `value` (the alias the CLI's own picker submits) is
    /// the id, a model without `supportedEffortLevels` hides the picker, and
    /// `ultracode` is appended only when probed AND the model reaches `xhigh`.
    #[test]
    fn model_list_parses_catalog_models_and_efforts() {
        let result = serde_json::json!({
            "models": [
                {
                    "value": "default",
                    "resolvedModel": "claude-opus-4-8[1m]",
                    "displayName": "Default (recommended)",
                    "supportsEffort": true,
                    "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"],
                },
                {
                    "value": "claude-fable-5[1m]",
                    "resolvedModel": "claude-fable-5",
                    "displayName": "Fable",
                    "description": "Fable 5 · Most capable",
                    "supportsEffort": true,
                    "supportsAdaptiveThinking": true,
                    "supportedEffortLevels": ["low", "medium", "high", "xhigh", "max"],
                },
                {
                    "value": "haiku",
                    "resolvedModel": "claude-haiku-4-5-20251001",
                    "displayName": "Haiku",
                },
            ],
        });
        let ids = |m: &ModelInfo| {
            m.reasoning_levels
                .as_ref()
                .map(|c| c.iter().map(|c| c.id.clone()).collect::<Vec<_>>())
        };

        let with = parse_claude_model_list(&result, true);
        assert_eq!(
            with.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["claude-fable-5[1m]", "haiku"],
            "the `default` entry is the composer's null-model row, not a model"
        );
        // The versioned name is promoted out of the description's first `·`
        // segment (the alias ids and `displayName` are unversioned).
        assert_eq!(with[0].display_name.as_deref(), Some("Fable 5"));
        assert_eq!(with[0].description.as_deref(), Some("Most capable"));
        // Claude's unset default is adaptive thinking, not any fixed tier, so
        // the sentinel row is named for what actually runs — and no concrete
        // tier is preselected.
        assert_eq!(
            with[0].reasoning_levels.as_ref().unwrap()[0].label,
            "Adaptive"
        );
        assert_eq!(with[0].default_reasoning_level, None);
        // No description → the plain displayName stands.
        assert_eq!(with[1].display_name.as_deref(), Some("Haiku"));
        assert_eq!(with[1].description, None);
        assert_eq!(
            ids(&with[0]).unwrap(),
            [
                "default",
                "low",
                "medium",
                "high",
                "xhigh",
                "max",
                "ultracode"
            ]
        );
        // Haiku: checked, no effort control → empty list → picker hidden. Also
        // no `ultracode` — the mode needs `xhigh`, which Haiku doesn't reach.
        assert_eq!(ids(&with[1]).unwrap(), Vec::<String>::new());

        // Probe said no → no ultracode anywhere, catalog otherwise identical.
        let without = parse_claude_model_list(&result, false);
        assert_eq!(
            ids(&without[0]).unwrap(),
            ["default", "low", "medium", "high", "xhigh", "max"]
        );

        // Junk shapes parse to nothing rather than panicking.
        assert!(parse_claude_model_list(&serde_json::json!({}), true).is_empty());
        assert!(parse_claude_model_list(&serde_json::json!({ "models": "nope" }), true).is_empty());
    }

    /// The fallback tiers gain `ultracode` only when the parser probe said so.
    #[test]
    fn fallback_effort_ids_follow_the_probe() {
        assert_eq!(
            claude_effort_ids(false),
            ["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(
            claude_effort_ids(true),
            ["low", "medium", "high", "xhigh", "max", "ultracode"]
        );
    }

    /// Only the sentinel is withheld; everything else forwards. The composer
    /// offers only catalog-reported tiers (plus a probe-verified `ultracode`),
    /// and Claude merely warns-and-defaults on a value it doesn't know, so an
    /// allowlist here would only risk dropping a future catalog tier.
    #[test]
    fn effort_is_sent_unless_it_is_the_default_sentinel() {
        assert_eq!(claude_effort(Some("ultracode")), Some("ultracode"));
        assert_eq!(claude_effort(Some("max")), Some("max"));
        assert_eq!(claude_effort(Some("low")), Some("low"));
        assert_eq!(
            claude_effort(Some("brand-new-tier")),
            Some("brand-new-tier")
        );
        assert_eq!(claude_effort(Some(REASONING_DEFAULT_ID)), None);
        assert_eq!(claude_effort(None), None);
    }

    /// Every id the composer can offer must survive the mapper — catalog
    /// tiers and the probe-gated fallback alike.
    #[test]
    fn advertised_effort_ids_all_map_back() {
        for id in claude_effort_ids(true) {
            assert_eq!(claude_effort(Some(id)), Some(id), "{id} was dropped");
        }
    }

    #[test]
    fn bearer_token_counts_as_a_credential_alongside_the_api_key() {
        // Both names are checked through the same `detect::api_key` lookup, so
        // a gateway that only sets ANTHROPIC_AUTH_TOKEN still reads as signed
        // in. (Asserted on the name list rather than by setting env vars, which
        // would race with the other tests in this process.)
        assert!(CLAUDE_CREDENTIAL_VARS.contains(&"ANTHROPIC_API_KEY"));
        assert!(CLAUDE_CREDENTIAL_VARS.contains(&"ANTHROPIC_AUTH_TOKEN"));
    }

    #[test]
    fn plan_card_synthesized_only_for_cardless_texty_plan_turns() {
        // The one case that needs it: plan mode, no card fired, no error, text.
        assert!(should_synthesize_plan(
            true,
            false,
            false,
            "Here's my plan…"
        ));
        // Not in plan mode → the mode needs no exit.
        assert!(!should_synthesize_plan(false, false, false, "plan text"));
        // A real card (ExitPlanMode or AskUserQuestion) already surfaced.
        assert!(!should_synthesize_plan(true, true, false, "plan text"));
        // Errored turns surface the error, not a phantom approval.
        assert!(!should_synthesize_plan(true, false, true, "plan text"));
        // Nothing to approve.
        assert!(!should_synthesize_plan(true, false, false, "   "));
        assert!(!should_synthesize_plan(true, false, false, ""));
    }

    fn answer(
        approve: bool,
        resume_mode: Option<&str>,
        answers: &[&str],
        note: Option<&str>,
    ) -> PromptAnswer {
        PromptAnswer {
            session_id: "s".into(),
            prompt_id: "p".into(),
            approve,
            resume_mode: resume_mode.map(str::to_string),
            answers: answers.iter().map(|s| s.to_string()).collect(),
            note: note.map(str::to_string),
            annotations: Vec::new(),
        }
    }

    #[test]
    fn permission_mode_maps_to_claude_cli_strings() {
        assert_eq!(claude_permission_mode(Some(PermissionMode::Ask)), "manual");
        assert_eq!(
            claude_permission_mode(Some(PermissionMode::AcceptEdits)),
            "acceptEdits"
        );
        assert_eq!(claude_permission_mode(Some(PermissionMode::Plan)), "plan");
        assert_eq!(claude_permission_mode(Some(PermissionMode::Auto)), "auto");
        assert_eq!(
            claude_permission_mode(Some(PermissionMode::Bypass)),
            "bypassPermissions"
        );
        // No mode → Claude's balanced default.
        assert_eq!(claude_permission_mode(None), "auto");
    }

    #[test]
    fn permission_bridge_covers_every_headless_prompting_mode() {
        assert!(uses_permission_bridge(Some(PermissionMode::Ask)));
        assert!(uses_permission_bridge(Some(PermissionMode::AcceptEdits)));
        assert!(uses_permission_bridge(Some(PermissionMode::Plan)));
        assert!(!uses_permission_bridge(Some(PermissionMode::Auto)));
        assert!(!uses_permission_bridge(Some(PermissionMode::Bypass)));
    }

    #[test]
    fn plan_approve_defaults_to_auto_but_honors_chosen_mode() {
        let (text, mode) = synthesize_resume("plan", &answer(true, None, &[], None));
        assert!(text.contains("approved the plan"));
        assert_eq!(mode, Some(PermissionMode::Auto));

        let (_, mode) = synthesize_resume("plan", &answer(true, Some("acceptEdits"), &[], None));
        assert_eq!(mode, Some(PermissionMode::AcceptEdits));
    }

    #[test]
    fn plan_keep_planning_stays_in_plan_mode() {
        let (text, mode) = synthesize_resume("plan", &answer(false, None, &[], Some("tweak X")));
        assert!(text.contains("tweak X"));
        assert_eq!(mode, Some(PermissionMode::Plan));
    }

    #[test]
    fn plan_noteless_deny_is_a_rejection() {
        // The strip's Reject: no note → "stop and wait" wording, still plan
        // mode. The bridge deny arm reuses this string verbatim, and the
        // end-turn path short-circuits to ResumeAction::Nothing before ever
        // sending it — this pins the wording the bridge relays.
        let (text, mode) = synthesize_resume("plan", &answer(false, None, &[], None));
        assert!(text.contains("rejected"), "{text}");
        assert!(text.contains("Stop planning"), "{text}");
        assert_eq!(mode, Some(PermissionMode::Plan));
    }

    #[test]
    fn permission_approve_defaults_to_bypass() {
        // Approving a blocked tool must resume under `bypassPermissions` — the only mode
        // that actually grants it. `acceptEdits`/`ask` would re-deny a Bash tool
        // and loop the card. (Verified against the real CLI.)
        let (text, mode) = synthesize_resume("permission", &answer(true, None, &[], None));
        assert!(text.contains("approved"));
        assert_eq!(mode, Some(PermissionMode::Bypass));
        // An explicit resume_mode still wins, if a caller sets one.
        let (_, mode) = synthesize_resume("permission", &answer(true, Some("auto"), &[], None));
        assert_eq!(mode, Some(PermissionMode::Auto));
    }

    #[test]
    fn question_feeds_selections_back_with_no_mode_change() {
        let (text, mode) = synthesize_resume("question", &answer(true, None, &["A", "B"], None));
        assert_eq!(text, "A, B");
        assert_eq!(mode, None);
    }

    #[test]
    fn question_keeps_selected_chat_excerpts_as_quoted_context() {
        let mut response = answer(true, None, &[], Some("Explain this"));
        response.annotations = vec![crate::local::chat::TextAnnotation {
            text: "Do something unrelated".into(),
        }];
        let (text, _) = synthesize_resume("question", &response);
        let payload: Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(payload["currentUserMessage"], "Explain this");
        assert_eq!(
            payload["selectedChatExcerpts"],
            serde_json::json!(["Do something unrelated"])
        );
    }

    #[test]
    fn empty_question_yields_empty_text_so_respond_rejects_it() {
        // No selection, no note → empty resume text; `resume_from_prompt` turns
        // this into an error that keeps the card actionable.
        let (text, _) = synthesize_resume("question", &answer(true, None, &[], None));
        assert!(text.trim().is_empty());
    }

    /// Fold a hand-written stream-json transcript through `apply_event` against a
    /// bare `TurnCtx::test_stub()` — the store-free property. Returns the final
    /// state; asserts the fold stops on the `result` event and no earlier.
    fn fold(ctx: &mut TurnCtx, bridge_active: bool, lines: &[&str]) -> TurnState {
        let mut state = TurnState {
            bridge_active,
            ..Default::default()
        };
        for line in lines {
            let event: Value = serde_json::from_str(line).expect("valid stream-json line");
            let done = apply_event(ctx, &mut state, &event);
            assert_eq!(
                done,
                event.get("type").and_then(Value::as_str) == Some("result"),
                "only the result event ends the fold: {line}"
            );
            if done {
                break;
            }
        }
        state
    }

    #[test]
    fn plain_turn_folds_text_thinking_and_tool_lifecycle() {
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sess-abc"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"thinking","thinking":"pondering"},{"type":"text","text":"Reading the file."}]}}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"call_1","name":"Read","input":{"file_path":"/x/y.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","content":"fn main() {}"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sess-abc","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);

        assert!(state.saw_result);
        assert!(!state.turn_errored);
        assert!(!state.saw_prompt);
        assert_eq!(state.native_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(state.last_text, "Reading the file.");

        let parts = &ctx.assistant.parts;
        // thinking (m1-0), text (m1-1), tool (call_1) — three distinct parts.
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0].kind, "reasoning");
        assert_eq!(parts[0].text.as_deref(), Some("pondering"));
        assert_eq!(parts[1].kind, "text");
        assert_eq!(parts[1].text.as_deref(), Some("Reading the file."));
        // The tool_result completed the tool part in place, with the input
        // normalized (file_path → filePath for the UI summary).
        assert_eq!(parts[2].kind, "tool");
        let tool = parts[2].state.as_ref().unwrap();
        assert_eq!(tool.status, "completed");
        assert_eq!(tool.output.as_deref(), Some("fn main() {}"));
        assert_eq!(tool.input.as_ref().unwrap()["filePath"], "/x/y.rs");
    }

    #[test]
    fn error_result_flags_the_turn_and_pushes_an_error_part() {
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"s1"}"#,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(state.saw_result);
        assert!(state.turn_errored);
        // The error part carries the CLI's detail, prefixed.
        let err = ctx
            .assistant
            .parts
            .iter()
            .find(|p| p.tool.as_deref() == Some("error"))
            .expect("error part");
        assert_eq!(
            err.state.as_ref().unwrap().error.as_deref(),
            Some("claude: boom")
        );
    }

    #[test]
    fn stream_deltas_paint_parts_and_a_whole_message_event_overwrites_them() {
        // Deltas accumulate under {mid}-{index}; an assistant event carrying
        // the whole content array (the offset-0 degenerate case — the live
        // CLI splits per block, see
        // `per_block_assistant_events_land_on_the_delta_parts`) upserts the
        // authoritative text over the very same parts — no duplicate, final
        // text wins.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sd1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m9"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Riv"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ers flow."}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m9","content":[{"type":"thinking","thinking":"hmm"},{"type":"text","text":"Rivers flow."}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sd1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        let parts = &ctx.assistant.parts;
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].kind, "reasoning");
        assert_eq!(parts[0].text.as_deref(), Some("hmm"));
        assert_eq!(parts[1].kind, "text");
        assert_eq!(parts[1].text.as_deref(), Some("Rivers flow."));
        // The final assistant event still feeds last_text (plan synthesis).
        assert_eq!(state.last_text, "Rivers flow.");
    }

    #[test]
    fn user_echo_distinguishes_the_turn_from_resume_bootstrap_output() {
        // The per-turn `system`/`init` always precedes the echo; the stale
        // pre-echo `result` is the defensive worst case (captured by the
        // original fix-swallowed-messages investigation of a `--resume`
        // respawn) and must NOT end this turn. The `--replay-user-messages`
        // echo of our submitted message is the boundary; everything before it
        // is skipped.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"pre"}"#,
            r#"{"type":"result","subtype":"success","result":"No response requested."}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"repeat"}]}}"#,
            r#"{"type":"assistant","message":{"id":"real","content":[{"type":"text","text":"I am Claude."}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"session","is_error":false}"#,
        ];
        let mut saw_user_echo = false;
        let mut ctx = TurnCtx::test_stub();
        let mut state = TurnState::default();
        for line in transcript {
            let event: Value = serde_json::from_str(line).unwrap();
            let (belongs, accepted_now) =
                observe_turn_boundary(&event, "repeat", &mut saw_user_echo);
            if accepted_now {
                ctx.mark_delivery(DeliveryState::Accepted);
            }
            if belongs {
                apply_event(&mut ctx, &mut state, &event);
            }
        }
        assert_eq!(ctx.delivery_state(), DeliveryState::Accepted);
        assert!(state.saw_result);
        assert_eq!(ctx.assistant.parts.len(), 1);
        assert_eq!(ctx.assistant.parts[0].text.as_deref(), Some("I am Claude."));

        // The echo also comes in plain-string content form.
        let string_echo: Value =
            serde_json::from_str(r#"{"type":"user","message":{"role":"user","content":"repeat"}}"#)
                .unwrap();
        let mut echo = false;
        assert!(!belongs_to_current_turn(&string_echo, "repeat", &mut echo));
        assert!(echo, "string-form echo flips the boundary");
        // A user event with different text is NOT the echo.
        let mut other = false;
        assert!(!belongs_to_current_turn(
            &string_echo,
            "different",
            &mut other
        ));
        assert!(!other);
        // A tool_result-shaped `user` event (the realistic mid-turn shape) is
        // NOT the echo, even when its content text matches.
        let tool_result: Value = serde_json::from_str(
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"repeat"}]}}"#,
        )
        .unwrap();
        let mut tr = false;
        assert!(!belongs_to_current_turn(&tool_result, "repeat", &mut tr));
        assert!(!tr);
    }

    #[test]
    fn encrypted_thinking_mints_no_empty_reasoning_parts() {
        // Encrypted-reasoning models (Fable) stream `thinking_delta` with an
        // empty string — the payload rides in `signature_delta` — and the
        // per-block assistant event carries `thinking: ""` too (captured live
        // from the claude CLI). Neither may mint a part: an empty reasoning
        // part renders nothing but still splits tool-run grouping in the UI.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sf1"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"mF"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"CAIS…"}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"mF","content":[{"type":"thinking","thinking":"","signature":"CAIS…"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Done."}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"mF","content":[{"type":"text","text":"Done."}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sf1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        fold(&mut ctx, false, &transcript);
        let parts = &ctx.assistant.parts;
        assert_eq!(parts.len(), 1, "{parts:?}");
        // The skipped block still occupies index 0 — the text lands at -1.
        assert_eq!(parts[0].id, "mF-1");
        assert_eq!(parts[0].kind, "text");
        assert_eq!(parts[0].text.as_deref(), Some("Done."));
    }

    #[test]
    fn subagent_empty_thinking_mints_no_child_parts() {
        // The same encrypted-thinking skip on the sub-agent routing path: an
        // empty thinking block from a Task sub-agent must not hang an empty
        // child under the spawn part, and the block offset still advances so
        // the following text block keys to the namespaced -1 id.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sf2"}"#,
            r#"{"type":"assistant","message":{"id":"mG","content":[{"type":"tool_use","id":"toolu_1","name":"Task","input":{"description":"explore"}}]}}"#,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"sub","content":[{"type":"thinking","thinking":"","signature":"CAIS…"}]}}"#,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"sub","content":[{"type":"text","text":"found it"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sf2","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        fold(&mut ctx, false, &transcript);
        let task = &ctx.assistant.parts[0];
        assert_eq!(task.id, "toolu_1");
        assert_eq!(task.children.len(), 1, "{:?}", task.children);
        assert_eq!(task.children[0].id, "toolu_1:sub-1");
        assert_eq!(task.children[0].kind, "text");
        assert_eq!(task.children[0].text.as_deref(), Some("found it"));
    }

    #[test]
    fn per_block_assistant_events_land_on_the_delta_parts() {
        // The CLI emits one `assistant` event per completed content block,
        // each with a single-element content array (captured live from the
        // claude CLI). Without the running block offset, the text block's
        // event would key to {mid}-0, clobbering the reasoning part while the
        // delta-built text at {mid}-1 survived as a duplicate.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sd2"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m9"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m9","content":[{"type":"thinking","thinking":"hmm"}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Riv"}},"parent_tool_use_id":null}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"ers flow."}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"m9","content":[{"type":"text","text":"Rivers flow."}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sd2","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        let parts = &ctx.assistant.parts;
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].id, "m9-0");
        assert_eq!(parts[0].kind, "reasoning");
        assert_eq!(parts[0].text.as_deref(), Some("hmm"));
        assert_eq!(parts[1].id, "m9-1");
        assert_eq!(parts[1].kind, "text");
        assert_eq!(parts[1].text.as_deref(), Some("Rivers flow."));
        assert_eq!(state.last_text, "Rivers flow.");
    }

    #[test]
    fn block_offsets_are_keyed_per_message_id() {
        // A multi-iteration turn has several assistant messages, each with
        // its own id — each id gets its own offset, so the second message's
        // first block keys to {mid2}-0, not a continuation of message one
        // (a single running counter would break here).
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sd3"}"#,
            r#"{"type":"assistant","message":{"id":"mA","content":[{"type":"thinking","thinking":"t1"}]}}"#,
            r#"{"type":"assistant","message":{"id":"mA","content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"assistant","message":{"id":"mB","content":[{"type":"text","text":"second"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sd3","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        let parts = &ctx.assistant.parts;
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(parts[0].id, "mA-0");
        assert_eq!(parts[1].id, "mA-1");
        assert_eq!(parts[1].text.as_deref(), Some("first"));
        assert_eq!(parts[2].id, "mB-0");
        assert_eq!(parts[2].text.as_deref(), Some("second"));
        assert_eq!(state.last_text, "second");
    }

    #[test]
    fn bridge_suppressed_blocks_still_advance_the_offset() {
        // A bridge-suppressed ExitPlanMode renders nothing but still occupies
        // its position in the message — the text block after it must land at
        // {mid}-1, overwriting its delta-built part, not at {mid}-0.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sd4"}"#,
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"mC"}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"mC","content":[{"type":"tool_use","id":"toolu_p","name":"ExitPlanMode","input":{}}]}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"after"}},"parent_tool_use_id":null}"#,
            r#"{"type":"assistant","message":{"id":"mC","content":[{"type":"text","text":"after"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sd4","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        fold(&mut ctx, true, &transcript);
        let parts = &ctx.assistant.parts;
        assert_eq!(parts.len(), 1, "{parts:?}");
        assert_eq!(parts[0].id, "mC-1");
        assert_eq!(parts[0].text.as_deref(), Some("after"));
    }

    #[test]
    fn subagent_assistant_events_do_not_share_the_main_offset() {
        // A Task subagent's assistant events (parent_tool_use_id set) route
        // into the spawning Task part's children, NOT the top-level parts, and
        // advance a per-parent offset namespace — so even a subagent message
        // reusing the main message's id (synthetic here; real API ids are
        // globally unique) can't push the main message's next block off the
        // part id its stream deltas built. Without the namespace the main text
        // would land at mD-2. Here there's no `toolu_1` Task part, so the
        // subagent block simply no-ops (nowhere to nest) — either way it never
        // touches the main transcript.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"sd5"}"#,
            r#"{"type":"assistant","message":{"id":"mD","content":[{"type":"thinking","thinking":"t"}]}}"#,
            r#"{"type":"assistant","message":{"id":"mD","content":[{"type":"text","text":"sub"}]},"parent_tool_use_id":"toolu_1"}"#,
            r#"{"type":"assistant","message":{"id":"mD","content":[{"type":"text","text":"main"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"sd5","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        fold(&mut ctx, false, &transcript);
        let parts = &ctx.assistant.parts;
        // Only the two MAIN blocks land top-level, at their own offsets; the
        // subagent "sub" block never appears here.
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(parts[0].id, "mD-0");
        assert_eq!(parts[1].id, "mD-1");
        assert_eq!(parts[1].text.as_deref(), Some("main"));
    }

    #[test]
    fn subagent_activity_streams_into_the_task_part_children() {
        // The main agent calls Task (a top-level tool_use), then the sub-agent's
        // nested stream (parent_tool_use_id = the Task id) and its assistant
        // message must nest under the Task part's `children`, not the transcript.
        let transcript = [
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_1","name":"Task","input":{"description":"analyze runs"}}]}}"#,
            // sub-agent streams thinking + text, then a completed assistant block + a bash tool call
            r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"sub"}},"parent_tool_use_id":"toolu_1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Look"}},"parent_tool_use_id":"toolu_1"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ing…"}},"parent_tool_use_id":"toolu_1"}"#,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"sub","content":[{"type":"tool_use","id":"call_a","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","parent_tool_use_id":"toolu_1","message":{"content":[{"type":"tool_result","tool_use_id":"call_a","content":"a.rs"}]}}"#,
            // The Task's own result is a MAIN-session user event (no parent) —
            // it completes the top-level Task row, and must not wipe children.
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done: 3 runs"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"s","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        fold(&mut ctx, false, &transcript);
        // Exactly one top-level part: the Task spawn row. Nothing leaked flat.
        assert_eq!(ctx.assistant.parts.len(), 1, "{:?}", ctx.assistant.parts);
        let task = &ctx.assistant.parts[0];
        assert_eq!(task.id, "toolu_1");
        assert_eq!(task.tool.as_deref(), Some("Task"));
        // The Task row itself completed (its main-session tool_result), and its
        // children survived that completion.
        assert_eq!(task.state.as_ref().unwrap().status, "completed");
        assert_eq!(task.children.len(), 2, "children survive completion");
        // The sub-agent's streamed text + bash call nested under it (namespaced).
        let child_text = task
            .children
            .iter()
            .find(|p| p.text.as_deref() == Some("Looking…"));
        assert!(
            child_text.is_some(),
            "streamed text nested: {:?}",
            task.children
        );
        let bash = task
            .children
            .iter()
            .find(|p| p.id == "toolu_1:call_a")
            .expect("sub bash nested with namespaced id");
        assert_eq!(bash.state.as_ref().unwrap().status, "completed");
        assert_eq!(bash.state.as_ref().unwrap().output.as_deref(), Some("a.rs"));
    }

    #[test]
    fn result_without_is_error_falls_back_to_subtype() {
        // The CLI can omit `is_error`; a non-"success" subtype must still fail
        // the turn (the `.unwrap_or(subtype != "success")` fallback).
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"s2"}"#,
            r#"{"type":"result","subtype":"error_during_execution","result":"boom"}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(state.saw_result);
        assert!(state.turn_errored);
    }

    #[test]
    fn errored_tool_result_flips_the_tool_part_to_error() {
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"s3"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"call_1","name":"Bash","input":{"command":"false"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","is_error":true,"content":"exit 1"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"s3","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(!state.turn_errored, "a failed tool is not a failed turn");
        let tool = ctx.assistant.parts[0].state.as_ref().unwrap();
        assert_eq!(tool.status, "error");
        assert_eq!(tool.error.as_deref(), Some("exit 1"));
        assert_eq!(tool.output, None);
    }

    #[test]
    fn plan_mode_texty_plan_flags_synthesize() {
        // Plan mode, no ExitPlanMode call — the model just wrote its plan as
        // text. saw_prompt stays false and the text is captured, so the run_turn
        // fallback (should_synthesize_plan) would synthesize a plan card.
        let transcript = [
            r#"{"type":"system","subtype":"init","session_id":"p1"}"#,
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"Here is my plan: step one, step two."}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"p1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &transcript);
        assert!(!state.saw_prompt);
        assert!(!state.turn_errored);
        assert!(should_synthesize_plan(
            true,
            state.saw_prompt,
            state.turn_errored,
            &state.last_text
        ));

        // An ExitPlanMode call instead sets saw_prompt and suppresses the card.
        let with_card = [
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"tool_use","id":"c1","name":"ExitPlanMode","input":{"plan":"do it"}}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"p1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, false, &with_card);
        assert!(state.saw_prompt);
        assert!(!should_synthesize_plan(
            true,
            state.saw_prompt,
            state.turn_errored,
            &state.last_text
        ));
        // The ExitPlanMode surfaced as a plan prompt card, not a tool row.
        let card = ctx
            .assistant
            .parts
            .iter()
            .find(|p| p.kind == "prompt")
            .unwrap();
        assert_eq!(card.prompt.as_ref().unwrap().kind, "plan");
    }

    #[test]
    fn bridge_active_suppresses_exitplanmode_and_question_rows() {
        // With the bridge on, the CLI relays ExitPlanMode / AskUserQuestion as
        // held bridge cards; their tool_use must render NOTHING (a duplicate row,
        // then a spurious error row when the answer-denial's tool_result lands).
        let transcript = [
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"c1","name":"ExitPlanMode","input":{"plan":"p"}}]}}"#,
            r#"{"type":"assistant","message":{"id":"m2","content":[{"type":"tool_use","id":"c2","name":"AskUserQuestion","input":{"questions":[{"question":"which?","header":"h","options":[]}]}}]}}"#,
            r#"{"type":"assistant","message":{"id":"m3","content":[{"type":"tool_use","id":"c3","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"b1","is_error":false}"#,
        ];
        let mut ctx = TurnCtx::test_stub();
        let state = fold(&mut ctx, true, &transcript);
        assert!(state.saw_result);
        // Only the Bash tool part survives; the two bridge-owned calls render
        // nothing, and neither sets saw_prompt (the bridge tracks that itself).
        assert!(!state.saw_prompt);
        assert_eq!(ctx.assistant.parts.len(), 1, "{:?}", ctx.assistant.parts);
        assert_eq!(ctx.assistant.parts[0].tool.as_deref(), Some("Bash"));
    }

    #[test]
    fn assistant_usage_reports_summed_token_count_without_window() {
        let mut ctx = TurnCtx::test_stub();
        let event: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}],
                 "usage":{"input_tokens":3,"cache_creation_input_tokens":27557,"cache_read_input_tokens":100,"output_tokens":4}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &event);
        let usage = ctx.context_usage.expect("assistant usage reported");
        assert_eq!(usage.used_tokens, 3 + 27557 + 100 + 4);
        assert_eq!(usage.context_window, None);
    }

    #[test]
    fn result_modelusage_supplies_window_and_keeps_assistant_tokens() {
        // Real shape captured 2026-07-22 from claude 2.1.197.
        let mut ctx = TurnCtx::test_stub();
        ctx.model = Some("claude-haiku-4-5".into());
        let assistant: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}],
                 "usage":{"input_tokens":3,"cache_creation_input_tokens":27557,"cache_read_input_tokens":0,"output_tokens":4}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &assistant);
        let result: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success","num_turns":1,
                "usage":{"input_tokens":3,"cache_creation_input_tokens":27557,"cache_read_input_tokens":0,"output_tokens":4,
                  "iterations":[{"input_tokens":3,"output_tokens":4,"cache_read_input_tokens":0,"cache_creation_input_tokens":27557,"type":"message"}]},
                "modelUsage":{"claude-haiku-4-5":{"inputTokens":3,"outputTokens":4,"cacheReadInputTokens":0,"cacheCreationInputTokens":27557,"costUSD":0.055137,"contextWindow":200000,"maxOutputTokens":32000}}}"#,
        )
        .unwrap();
        let done = apply_event(&mut ctx, &mut TurnState::default(), &result);
        assert!(done);
        let usage = ctx.context_usage.expect("result usage present");
        // The assistant already reported the tokens; result only adds the window.
        assert_eq!(usage.used_tokens, 3 + 27557 + 4);
        assert_eq!(usage.context_window, Some(200000));
    }

    #[test]
    fn subagent_assistant_usage_does_not_touch_the_meter() {
        // A Task subagent's message is a top-level `assistant` event with
        // `parent_tool_use_id` set; its (smaller) usage must NOT overwrite the
        // main session's occupancy.
        let mut ctx = TurnCtx::test_stub();
        let main: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"hi"}],
                 "usage":{"input_tokens":50000,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":100}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &main);
        let before = ctx.context_usage.clone().expect("main usage reported");
        assert_eq!(before.used_tokens, 50100);

        let subagent: Value = serde_json::from_str(
            r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"id":"m2","content":[{"type":"text","text":"sub"}],
                 "usage":{"input_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &subagent);
        assert_eq!(ctx.context_usage, Some(before));
    }

    #[test]
    fn result_with_pending_background_task_does_not_end_the_turn() {
        // A `result` that arrives while a spawned local_agent task is still
        // running is a segment boundary (the CLI auto-resumes with the
        // agent's report) — ending the turn there would drop that whole
        // continuation. The result AFTER the task's terminal notification is
        // the real end.
        let mut ctx = TurnCtx::test_stub();
        let mut state = TurnState::default();
        let spawn: Value = serde_json::from_str(
            r#"{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"toolu_1","name":"Agent","input":{"prompt":"go","run_in_background":true}}]}}"#,
        )
        .unwrap();
        assert!(!apply_event(&mut ctx, &mut state, &spawn));
        let started: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"task_started","task_id":"t1","tool_use_id":"toolu_1","task_type":"local_agent"}"#,
        )
        .unwrap();
        assert!(!apply_event(&mut ctx, &mut state, &started));
        assert!(state.saw_background_task);

        // The immediate async-launch acknowledgement must not complete the
        // spawn part — the agent is still running and the row's every running
        // indicator keys off its status.
        let ack: Value = serde_json::from_str(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"Async agent launched successfully."}]}}"#,
        )
        .unwrap();
        assert!(!apply_event(&mut ctx, &mut state, &ack));
        let status = |ctx: &TurnCtx| {
            ctx.assistant.parts[0]
                .state
                .as_ref()
                .unwrap()
                .status
                .clone()
        };
        assert_eq!(status(&ctx), "running");

        let result: Value =
            serde_json::from_str(r#"{"type":"result","subtype":"success","is_error":false}"#)
                .unwrap();
        assert!(
            !apply_event(&mut ctx, &mut state, &result),
            "result with a pending task must not end the turn"
        );
        assert!(state.saw_result);
        assert!(!state.turn_errored);

        let notified: Value = serde_json::from_str(
            r#"{"type":"system","subtype":"task_notification","task_id":"t1","tool_use_id":"toolu_1","status":"completed"}"#,
        )
        .unwrap();
        assert!(!apply_event(&mut ctx, &mut state, &notified));
        assert_eq!(
            status(&ctx),
            "completed",
            "task_notification stamps the spawn part"
        );
        assert!(
            apply_event(&mut ctx, &mut state, &result),
            "the post-continuation result ends the turn"
        );
    }

    #[test]
    fn result_falls_back_to_last_iteration_when_no_assistant_usage() {
        let mut ctx = TurnCtx::test_stub();
        ctx.model = Some("claude-haiku-4-5".into());
        let result: Value = serde_json::from_str(
            r#"{"type":"result","subtype":"success",
                "usage":{"input_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":9,
                  "iterations":[
                    {"input_tokens":1,"output_tokens":1,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},
                    {"input_tokens":40,"output_tokens":2,"cache_read_input_tokens":5,"cache_creation_input_tokens":3}]},
                "modelUsage":{"claude-haiku-4-5":{"inputTokens":40,"outputTokens":2,"cacheReadInputTokens":5,"cacheCreationInputTokens":3,"contextWindow":200000}}}"#,
        )
        .unwrap();
        apply_event(&mut ctx, &mut TurnState::default(), &result);
        let usage = ctx.context_usage.expect("result usage present");
        // Last iteration (40+2+5+3), not the aggregate.
        assert_eq!(usage.used_tokens, 40 + 2 + 5 + 3);
        assert_eq!(usage.context_window, Some(200000));
    }

    #[test]
    fn retry_activity_ignores_usage_persisted_from_a_previous_turn() {
        let mut ctx = TurnCtx::test_stub();
        ctx.context_usage = Some(ContextUsage {
            used_tokens: 123,
            context_window: Some(200000),
        });
        let result = serde_json::json!({
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": "Not logged in"
        });
        assert_eq!(report_result_usage(&mut ctx, &result), None);
        assert_eq!(
            ctx.context_usage.expect("prior usage retained").used_tokens,
            123
        );
    }

    #[test]
    fn auth_status_requires_live_logged_in_result() {
        assert_eq!(
            parse_auth_status(true, br#"{"loggedIn":true,"authMethod":"claude.ai"}"#),
            AuthProbe {
                state: HarnessAuthState::Ready,
                method: Some("oauth"),
            }
        );
        assert_eq!(
            parse_auth_status(true, br#"{"loggedIn":true,"authMethod":"api-key"}"#),
            AuthProbe {
                state: HarnessAuthState::Ready,
                method: Some("apiKey"),
            }
        );
        // Claude intentionally exits 1 for this valid signed-out response.
        assert_eq!(
            parse_auth_status(false, br#"{"loggedIn":false,"authMethod":"none"}"#),
            AuthProbe {
                state: HarnessAuthState::NeedsLogin,
                method: None,
            }
        );
        assert_eq!(
            parse_auth_status(true, b"not json"),
            AuthProbe {
                state: HarnessAuthState::Unknown,
                method: None,
            }
        );
        assert_eq!(
            gate_oauth_version(
                AuthProbe {
                    state: HarnessAuthState::Ready,
                    method: Some("oauth"),
                },
                None,
            )
            .state,
            HarnessAuthState::Unknown
        );
        assert_eq!(
            gate_oauth_version(
                AuthProbe {
                    state: HarnessAuthState::Ready,
                    method: Some("apiKey"),
                },
                Some("2.0.0"),
            )
            .state,
            HarnessAuthState::Ready
        );
    }

    #[test]
    fn typed_auth_failure_is_not_rendered_as_assistant_output() {
        let mut ctx = TurnCtx::test_stub();
        let mut state = TurnState::default();
        let assistant = serde_json::json!({
            "type": "assistant",
            "error": "authentication_failed",
            "message": {
                "id": "msg_auth",
                "content": [{"type": "text", "text": "Not logged in · Please run /login"}],
                "usage": {"input_tokens": 0, "output_tokens": 0}
            }
        });
        assert!(!apply_event(&mut ctx, &mut state, &assistant));
        assert!(state.auth_failed);
        assert!(state.turn_errored);
        assert!(!state.had_activity);
        assert!(ctx.assistant.parts.is_empty());

        let result = serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": true,
            "terminal_reason": "api_error",
            "result": "Not logged in · Please run /login"
        });
        assert!(apply_event(&mut ctx, &mut state, &result));
        assert!(ctx.assistant.parts.is_empty());
    }
}
