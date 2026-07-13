//! Codex harness.
//!
//! Chat: one `codex exec --json` process per turn, JSONL events on stdout,
//! multi-turn via `codex exec resume <session>`. Uses the user's own
//! `codex login` (ChatGPT plan or API key). Codex has no system-prompt flag and
//! reads AGENTS.md (which the repo may own), so the playbook is injected as
//! tagged context on the first turn — the display transcript stores only the
//! user's text. The event parser accepts both JSONL shapes codex has shipped:
//! legacy `{"id", "msg": {"type": ...}}` and item-style `{"type":
//! "item.completed", "item": {...}}`.
//!
//! Detection: `~/.codex/auth.json` holds either an `OPENAI_API_KEY` or an OAuth
//! `id_token` JWT we decode (unverified) for the account email and plan.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use super::detect::{
    bin_version, find_on_path, jwt_payload, nonempty_str, read_json, resolve_symlinks, title_case,
    HarnessInfo,
};
use super::options::{HarnessOptions, PermissionMode};
use super::Harness;
use crate::error::{anyhow, Result};
use crate::local::chat::{prepare_env, TurnCtx, WirePart, WireToolState};
use crate::local::opencode::ensure_playbook;

// The 5.6 variants (Sol = frontier, Terra = balanced, Luna = fast) plus 5.5;
// ChatGPT-account codex rejects bare `gpt-5.6`. Verified against codex-cli
// 0.144 via `codex exec -m` (5.6 needs >= 0.143; older CLIs get a 400).
const CODEX_MODELS: [&str; 4] = ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"];

/// Codex's own reasoning vocabulary (id == the `model_reasoning_effort` config
/// value) — the common set across CODEX_MODELS (Sol/Terra also take max/ultra;
/// Luna and 5.5 don't). Reasoning is per-harness (see `options.rs`). Verified
/// against codex-cli 0.144.
const CODEX_REASONING_LEVELS: [(&str, &str); 4] = [
    ("low", "Low"),
    ("medium", "Medium"),
    ("high", "High"),
    ("xhigh", "XHigh"),
];

pub struct Codex;

/// `codex` on PATH, symlinks resolved (see `resolve_symlinks` — codex needs to
/// find its `codex-code-mode-host` helper next to the real binary).
pub fn find_codex() -> Option<PathBuf> {
    find_on_path("codex").map(resolve_symlinks)
}

#[async_trait]
impl Harness for Codex {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn name(&self) -> &'static str {
        "Codex"
    }

    fn supports_chat(&self) -> bool {
        true
    }

    async fn detect(&self) -> Option<HarnessInfo> {
        let mut info = HarnessInfo::new(self.id(), self.name());
        if let Some(bin) = find_codex() {
            info.installed = true;
            info.version = bin_version(&bin).await;
            info.bin_path = Some(bin.to_string_lossy().into_owned());
        }
        if let Some(auth) =
            dirs::home_dir().and_then(|h| read_json(h.join(".codex").join("auth.json")))
        {
            if nonempty_str(&auth, "OPENAI_API_KEY").is_some() {
                info.authenticated = true;
                info.auth_method = Some("apiKey");
            }
            if let Some(claims) = auth
                .get("tokens")
                .and_then(|t| t.get("id_token"))
                .and_then(Value::as_str)
                .and_then(jwt_payload)
            {
                info.authenticated = true;
                info.auth_method = Some("oauth");
                info.account = nonempty_str(&claims, "email");
                if let Some(oa) = claims.get("https://api.openai.com/auth") {
                    info.plan = nonempty_str(oa, "chatgpt_plan_type").map(|p| title_case(&p));
                }
            }
        }

        info.agent_ready = info.installed && info.authenticated;
        if info.agent_ready {
            info = info.with_models(&CODEX_MODELS);
        } else {
            info.agent_note =
                Some("Install Codex and sign in (`codex login`) to chat with it here.".to_string());
        }
        Some(info)
    }

    async fn run_turn(&self, ctx: &mut TurnCtx) -> Result<()> {
        run_turn(ctx).await
    }

    fn options(&self) -> HarnessOptions {
        // `codex exec` is non-interactive — no approval channel to prompt over
        // (verified: on-request emits no approval event; the sandbox just allows
        // or denies), so permission modes map onto the *sandbox policy*. We offer
        // only Auto + Bypass, matching Claude. A `Plan`→`read-only` sandbox was
        // considered but dropped for the same reason plan mode was dropped for
        // Claude: read-only blocks the `orx` inspection the agent needs *and* the
        // launches that are the point. (Codex has a real first-class Plan mode,
        // but only over `app-server`/the TUI — `codex exec` doesn't honor it;
        // verified `-c collaboration_mode="plan"` is accepted but ignored.)
        //   * Auto  — workspace-write, plus network and the hub clone's
        //     `.git` (see `run_turn`), since exec can't approve its way past
        //     either denial the way the TUI can.
        //   * Bypass— full access (`--dangerously-bypass-approvals-and-sandbox`).
        HarnessOptions::none()
            .with_permission_modes(
                &[PermissionMode::Auto, PermissionMode::Bypass],
                PermissionMode::Auto,
            )
            // Codex's own reasoning tiers via `-c model_reasoning_effort`.
            .with_reasoning_levels(&CODEX_REASONING_LEVELS, "high")
    }

    fn config_home(&self) -> Option<PathBuf> {
        Some(dirs::home_dir()?.join(".codex"))
    }

    fn skill_target(&self) -> Option<PathBuf> {
        Some(self.config_home()?.join("prompts").join("orx.md"))
    }

    fn skill_shim(&self) -> Option<&'static str> {
        Some(super::CODEX_PROMPT)
    }
}

/// Session mode → Codex `exec` sandbox policy. `codex exec` can't prompt for
/// approval, so the sandbox *is* the permission boundary. `Bypass` is the one
/// mode that also drops the sandbox entirely (`--dangerously-...`); the rest run
/// sandboxed with approvals set to `never` (nothing to escalate to). Returns
/// `None` for `Bypass` to signal "use the bypass flag instead of `-s`".
fn codex_sandbox(mode: Option<PermissionMode>) -> Option<&'static str> {
    match mode.unwrap_or(PermissionMode::Auto) {
        PermissionMode::Plan => Some("read-only"),
        // AcceptEdits/Ask have no distinct exec semantics — treat as the
        // balanced default so a session that carries them still runs sanely.
        PermissionMode::Auto | PermissionMode::AcceptEdits | PermissionMode::Ask => {
            Some("workspace-write")
        }
        PermissionMode::Bypass => None,
    }
}

/// The workspace's git dir (`git rev-parse --git-common-dir`), canonicalized.
/// Codex's `workspace-write` sandbox denies every git metadata write: it marks
/// each writable root's `.git` read-only, and a *worktree's* real metadata
/// (refs, objects, `FETCH_HEAD` under `.git/worktrees/<id>/`) lives in the
/// parent clone, outside the workspace entirely — orx session repos are always
/// worktrees of the hub clone. Interactively both denials escalate to approval
/// prompts; `codex exec` has none, so `git fetch`/`commit` just dies with
/// "Operation not permitted". Declaring the common dir as an explicit writable
/// root fixes both shapes — an explicit root beats the built-in `.git`
/// protection (verified against codex-cli 0.144 via `codex sandbox`, plain
/// clone and worktree). Canonicalized because codex requires absolute roots
/// and seatbelt matches real paths (`/var` vs `/private/var`).
async fn shared_git_dir(workspace: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(workspace)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if line.is_empty() {
        return None;
    }
    absolute_git_dir(workspace, Path::new(&line))
}

/// `dir` as an absolute, symlink-free path; `git rev-parse` answers relative
/// to the workspace for a regular clone (`.git`) and absolute for a worktree.
fn absolute_git_dir(workspace: &Path, dir: &Path) -> Option<PathBuf> {
    workspace.join(dir).canonicalize().ok()
}

/// The orx data dir as a sandbox writable root. The `orx` CLI the agent
/// drives opens the SQLite store read-write (plus journal/WAL sidecars)
/// directly at `store::data_dir()`, which sits under `~/.local/share` —
/// outside every workspace — so `workspace-write` denies the open and every
/// store-touching command dies with "unable to open database file". Created
/// here (host side, unsandboxed) so canonicalize can't fail before first use;
/// canonicalized for the same reason as `shared_git_dir`.
fn orx_data_dir() -> Option<PathBuf> {
    let dir = crate::store::data_dir();
    std::fs::create_dir_all(&dir).ok()?;
    dir.canonicalize().ok()
}

/// The `-c` value granting `roots` as sandbox writable roots, e.g.
/// `sandbox_workspace_write.writable_roots=["/a", "/b"]`. `None` when there
/// are no roots (omit the flag: `-c ...=[]` would still *replace* the user's
/// configured roots with nothing).
fn writable_roots_override(roots: &[PathBuf]) -> Option<String> {
    if roots.is_empty() {
        return None;
    }
    let list: Vec<String> = roots.iter().map(|p| toml_string(p)).collect();
    Some(format!(
        "sandbox_workspace_write.writable_roots=[{}]",
        list.join(", ")
    ))
}

/// A path as a TOML basic-string literal, for `-c key="value"` overrides.
/// serde_json's escaping emits only sequences TOML also accepts (`\"`, `\\`,
/// control chars as `\uXXXX`) and leaves `/` literal — except DEL (0x7F),
/// which serde_json passes through and TOML forbids unescaped.
fn toml_string(path: &Path) -> String {
    serde_json::to_string(&path.to_string_lossy())
        .unwrap_or_else(|_| String::from("\"\""))
        .replace('\u{7f}', "\\u007F")
}

/// Session reasoning id → Codex `model_reasoning_effort` value. The composer only
/// offers ids from `CODEX_REASONING_LEVELS`; an unrecognized/absent value omits
/// the override and lets Codex apply its configured default.
fn codex_reasoning(level: Option<&str>) -> Option<&str> {
    let level = level?;
    CODEX_REASONING_LEVELS
        .iter()
        .any(|(id, _)| *id == level)
        .then_some(level)
}

fn command_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

async fn run_turn(ctx: &mut TurnCtx) -> Result<()> {
    let bin = find_codex().ok_or_else(|| {
        anyhow!("codex not found on PATH — install Codex and run `codex login` first")
    })?;
    let project = ctx.project.clone();
    let session_id = ctx.session_id.clone();
    let (repo, playbook) =
        tokio::task::spawn_blocking(move || ensure_playbook(&project, &session_id))
            .await
            .map_err(|e| anyhow!("playbook task failed: {e}"))??;

    let mut cmd = Command::new(&bin);
    match &ctx.native_session_id {
        Some(native_id) => {
            cmd.args(["exec", "resume", native_id]);
        }
        None => {
            cmd.arg("exec");
        }
    }
    cmd.args(["--json", "--skip-git-repo-check"])
        .current_dir(&repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(crate::local::chat::harness_log("codex")?))
        .kill_on_drop(true);
    // Permission mode → sandbox policy. `codex exec` can't prompt, so the
    // sandbox is the approval boundary: non-bypass modes run sandboxed with
    // approvals disabled (nothing to escalate to), `Bypass` drops both.
    //
    // Set the policy via `-c sandbox_mode=` rather than `-s`: the `exec resume`
    // subcommand rejects `-s` ("unexpected argument"), but accepts `-c` on both
    // the fresh and resume paths (verified against codex-cli 0.143), so one form
    // works for the whole session lifecycle.
    match codex_sandbox(ctx.permission_mode) {
        Some(policy) => {
            cmd.args([
                "-c",
                &format!("sandbox_mode=\"{policy}\""),
                "-c",
                "approval_policy=\"never\"",
            ]);
            // workspace-write out of the box is too tight for the orx
            // workflow in three ways (all verified via `codex sandbox` against
            // codex-cli 0.144; in the TUI these denials escalate to approval
            // prompts, which `codex exec` doesn't have):
            //   * Network is blocked by default — DNS doesn't even resolve, so
            //     `git fetch`/`push`, package installs, and the `orx` CLI's
            //     localhost API calls all die. The agent's job is launching
            //     experiments over that API; Auto must keep the network open.
            //   * The orx store isn't writable — the SQLite DB lives in the
            //     data dir under `~/.local/share`, outside the workspace, so
            //     every `orx` command that touches it fails with "unable to
            //     open database file"; grant the data dir (see `orx_data_dir`).
            //   * Git metadata isn't writable — codex protects `.git` inside
            //     the workspace, and a worktree's real metadata (the hub
            //     clone's `.git`) sits outside it — so `git fetch`/`commit`
            //     fail outright; grant the common dir (see `shared_git_dir`).
            //     Note `-c` *replaces* any `writable_roots` from the user's
            //     config.toml for the turn (there is no append form; `exec
            //     --add-dir` is unverified on the resume path).
            if policy == "workspace-write" {
                cmd.args(["-c", "sandbox_workspace_write.network_access=true"]);
                let mut roots = Vec::new();
                roots.extend(orx_data_dir());
                roots.extend(shared_git_dir(&repo).await);
                if let Some(override_arg) = writable_roots_override(&roots) {
                    cmd.args(["-c", &override_arg]);
                }
            }
        }
        None => {
            cmd.arg("--dangerously-bypass-approvals-and-sandbox");
        }
    }
    // Reasoning level → Codex's own `model_reasoning_effort` config override.
    if let Some(effort) = codex_reasoning(ctx.reasoning_level.as_deref()) {
        cmd.args(["-c", &format!("model_reasoning_effort=\"{effort}\"")]);
    }
    if let Some(model) = &ctx.model {
        cmd.args(["-m", model]);
    }
    let prompt = if ctx.native_session_id.is_none() {
        let playbook_md = std::fs::read_to_string(&playbook).unwrap_or_default();
        format!(
            "<system-context>\n{playbook_md}\n</system-context>\n\n{}",
            ctx.text
        )
    } else {
        ctx.text.clone()
    };
    cmd.arg(prompt);
    prepare_env(&mut cmd);
    // The sandbox blocks the keyring `gh` keeps its token in ("stored token is
    // invalid" from inside the workspace), so resolve it out here and pass it
    // down; both `gh` and its git credential helper prefer these env vars.
    if let Some(token) = crate::local::git::resolve_github_token() {
        cmd.env("GH_TOKEN", &token);
        cmd.env("GITHUB_TOKEN", token);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Could not spawn {}: {}", bin.display(), e))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let mut lines = BufReader::new(stdout).lines();
    let mut counter = 0usize;
    let mut next_id = |prefix: &str| {
        counter += 1;
        format!("{prefix}-{counter}")
    };
    // Streaming deltas accumulate into one part until the complete event.
    let mut open_text: Option<String> = None;
    let mut open_reasoning: Option<String> = None;

    while let Some(line) = lines.next_line().await? {
        let Ok(event) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Legacy events nest under "msg"; item-style events are flat.
        let msg = event.get("msg").unwrap_or(&event);
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");

        // Session/thread id, wherever this version put it.
        for key in ["session_id", "thread_id", "conversation_id"] {
            if let Some(sid) = msg
                .get(key)
                .or_else(|| event.get(key))
                .and_then(Value::as_str)
            {
                ctx.set_native_session_id(sid);
            }
        }

        match kind {
            "agent_message_delta" => {
                let delta = msg.get("delta").and_then(Value::as_str).unwrap_or("");
                let id = open_text.get_or_insert_with(|| next_id("text")).clone();
                if ctx.assistant.parts.iter().all(|p| p.id != id) {
                    ctx.upsert_part(WirePart::text(id.clone(), ""));
                }
                ctx.append_part_text(&id, delta);
            }
            "agent_message" => {
                let text = msg.get("message").and_then(Value::as_str).unwrap_or("");
                let id = open_text.take().unwrap_or_else(|| next_id("text"));
                ctx.upsert_part(WirePart::text(id, text));
            }
            "agent_reasoning_delta" => {
                let delta = msg.get("delta").and_then(Value::as_str).unwrap_or("");
                let id = open_reasoning
                    .get_or_insert_with(|| next_id("think"))
                    .clone();
                if ctx.assistant.parts.iter().all(|p| p.id != id) {
                    ctx.upsert_part(WirePart::reasoning(id.clone(), ""));
                }
                ctx.append_part_text(&id, delta);
            }
            "agent_reasoning" => {
                let text = msg.get("text").and_then(Value::as_str).unwrap_or("");
                let id = open_reasoning.take().unwrap_or_else(|| next_id("think"));
                ctx.upsert_part(WirePart::reasoning(id, text));
            }
            "exec_command_begin" => {
                let id = msg
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| next_id("cmd"));
                let command = msg.get("command").map(command_string).unwrap_or_default();
                ctx.upsert_part(WirePart {
                    id,
                    kind: "tool".into(),
                    text: None,
                    tool: Some("bash".into()),
                    state: Some(WireToolState {
                        status: "running".into(),
                        input: Some(serde_json::json!({ "command": command })),
                        output: None,
                        error: None,
                        title: None,
                    }),
                    prompt: None,
                });
            }
            "exec_command_end" => {
                let call_id = msg.get("call_id").and_then(Value::as_str).unwrap_or("");
                let exit_ok = msg
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_none_or(|c| c == 0);
                let output = msg
                    .get("aggregated_output")
                    .or_else(|| msg.get("stdout"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(part) = ctx.assistant.parts.iter_mut().find(|p| p.id == call_id) {
                    if let Some(state) = part.state.as_mut() {
                        state.status = if exit_ok { "completed" } else { "error" }.into();
                        state.output = Some(output);
                    }
                }
            }
            "error" | "stream_error" | "turn.failed" => {
                let detail = msg
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("codex reported an error")
                    .to_string();
                ctx.push_error(detail);
            }
            // Item-style shape: everything interesting is under "item".
            "item.completed" | "item.updated" => {
                if let Some(item) = msg.get("item") {
                    handle_item(ctx, item, &mut next_id);
                }
            }
            _ => {}
        }
        ctx.maybe_flush();
    }

    let status = child.wait().await?;
    if !status.success() {
        return Err(anyhow!(
            "codex exited with {status}; see {}",
            crate::store::data_dir().join("agent-codex.log").display()
        ));
    }
    Ok(())
}

fn handle_item(ctx: &mut TurnCtx, item: &Value, next_id: &mut impl FnMut(&str) -> String) {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| next_id("item"));
    match item.get("type").and_then(Value::as_str) {
        Some("agent_message") => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            ctx.upsert_part(WirePart::text(id, text));
        }
        Some("reasoning") => {
            let text = item.get("text").and_then(Value::as_str).unwrap_or("");
            ctx.upsert_part(WirePart::reasoning(id, text));
        }
        Some("command_execution") => {
            let failed = item.get("status").and_then(Value::as_str) == Some("failed")
                || item
                    .get("exit_code")
                    .and_then(Value::as_i64)
                    .is_some_and(|c| c != 0);
            ctx.upsert_part(WirePart {
                id,
                kind: "tool".into(),
                text: None,
                tool: Some("bash".into()),
                state: Some(WireToolState {
                    status: if failed { "error" } else { "completed" }.into(),
                    input: Some(serde_json::json!({
                        "command": item.get("command").map(command_string).unwrap_or_default(),
                    })),
                    output: item
                        .get("aggregated_output")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    error: None,
                    title: None,
                }),
                prompt: None,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_maps_modes_to_exec_policies() {
        // Plan is the only read-only mode; the interactive-only modes collapse to
        // the balanced default (exec can't tell them apart); Bypass drops the
        // sandbox (None → the `--dangerously-...` flag).
        assert_eq!(codex_sandbox(Some(PermissionMode::Plan)), Some("read-only"));
        assert_eq!(
            codex_sandbox(Some(PermissionMode::Auto)),
            Some("workspace-write")
        );
        assert_eq!(
            codex_sandbox(Some(PermissionMode::AcceptEdits)),
            Some("workspace-write")
        );
        assert_eq!(
            codex_sandbox(Some(PermissionMode::Ask)),
            Some("workspace-write")
        );
        assert_eq!(codex_sandbox(Some(PermissionMode::Bypass)), None);
        // No mode set → the balanced default, never an accidental full-access.
        assert_eq!(codex_sandbox(None), Some("workspace-write"));
    }

    #[test]
    fn git_dir_resolves_relative_and_absolute_rev_parse_answers() {
        let base = std::env::temp_dir().join(format!("orx-codex-test-{}", std::process::id()));
        let workspace = base.join("worktree");
        let hub_git = base.join("hub").join(".git");
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::create_dir_all(&hub_git).unwrap();

        // Worktree: rev-parse answers with the hub clone's absolute path.
        assert_eq!(
            absolute_git_dir(&workspace, &hub_git),
            Some(hub_git.canonicalize().unwrap())
        );
        // Regular clone: rev-parse answers `.git`, relative to the workspace.
        assert_eq!(
            absolute_git_dir(&workspace, Path::new(".git")),
            Some(workspace.join(".git").canonicalize().unwrap())
        );
        // No git dir at all → no writable root (flag omitted, fail-safe).
        assert_eq!(absolute_git_dir(&workspace, Path::new("missing")), None);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn toml_string_quotes_and_escapes_paths() {
        assert_eq!(
            toml_string(Path::new("/a/with space")),
            r#""/a/with space""#
        );
        assert_eq!(toml_string(Path::new(r#"/a/"q""#)), r#""/a/\"q\"""#);
        // DEL is the one char serde_json leaves raw that TOML rejects.
        assert_eq!(toml_string(Path::new("/a/\u{7f}b")), r#""/a/\u007Fb""#);
    }

    #[test]
    fn writable_roots_override_joins_and_omits_empty() {
        assert_eq!(
            writable_roots_override(&[PathBuf::from("/data dir"), PathBuf::from("/hub/.git")]),
            Some(r#"sandbox_workspace_write.writable_roots=["/data dir", "/hub/.git"]"#.into())
        );
        // No roots → no flag at all; `=[]` would clobber the user's own
        // config.toml roots for the turn.
        assert_eq!(writable_roots_override(&[]), None);
    }

    #[test]
    fn reasoning_accepts_only_codex_ids() {
        assert_eq!(codex_reasoning(Some("low")), Some("low"));
        assert_eq!(codex_reasoning(Some("high")), Some("high"));
        assert_eq!(codex_reasoning(Some("xhigh")), Some("xhigh"));
        // Tiers outside the common set and junk are dropped (the flag is
        // omitted → CLI default), never forwarded as an invalid
        // `model_reasoning_effort`.
        assert_eq!(codex_reasoning(Some("max")), None);
        assert_eq!(codex_reasoning(None), None);
    }
}
