//! Harness autodetection for `orx up` — which coding-agent CLIs are installed
//! on this machine (Claude Code, Codex, OpenCode) and what account each is
//! signed into. Detection is read-only and best-effort: missing files or
//! unparseable JSON just mean "not detected", never an error.
//!
//! Each detected harness is a first-class chat backend (local::chat runs its
//! CLI directly); `agent_ready` marks the ones the composer can select.

use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use serde::Serialize;
use serde_json::Value;

use crate::local::opencode::find_opencode;

const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessInfo {
    pub id: &'static str,
    pub name: &'static str,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bin_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// A signed-in setup was found (auth file / OAuth account).
    pub authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<&'static str>, // "oauth" | "apiKey"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Usable as a chat backend right now (installed + signed in).
    pub agent_ready: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_note: Option<String>,
    pub models: Vec<ModelInfo>,
}

impl HarnessInfo {
    fn new(id: &'static str, name: &'static str) -> Self {
        Self {
            id,
            name,
            installed: false,
            bin_path: None,
            version: None,
            authenticated: false,
            auth_method: None,
            account: None,
            org: None,
            plan: None,
            agent_ready: false,
            agent_note: None,
            models: Vec::new(),
        }
    }
}

fn find_on_path(bin: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(bin))
        .find(|c| c.is_file())
}

/// `<bin> --version`, first line, with a timeout (node CLIs can be slow).
async fn bin_version(bin: &PathBuf) -> Option<String> {
    let fut = tokio::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output();
    let out = tokio::time::timeout(VERSION_TIMEOUT, fut)
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!line.is_empty()).then_some(line)
}

fn read_json(path: PathBuf) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn nonempty_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// --- Claude Code ------------------------------------------------------------

async fn detect_claude_code() -> HarnessInfo {
    let mut info = HarnessInfo::new("claude-code", "Claude Code");
    let bin = find_on_path("claude").or_else(|| {
        let home = dirs::home_dir()?;
        [".claude/local/claude", ".local/bin/claude"]
            .iter()
            .map(|rel| home.join(rel))
            .find(|c| c.is_file())
    });
    if let Some(bin) = bin {
        info.installed = true;
        info.version = bin_version(&bin).await;
        info.bin_path = Some(bin.to_string_lossy().into_owned());
    }
    // ~/.claude.json carries the signed-in OAuth account (no secrets read).
    if let Some(cfg) = dirs::home_dir().and_then(|h| read_json(h.join(".claude.json"))) {
        if let Some(acct) = cfg.get("oauthAccount") {
            info.authenticated = true;
            info.auth_method = Some("oauth");
            info.account = nonempty_str(acct, "emailAddress");
            info.org = nonempty_str(acct, "organizationName");
            info.plan = match nonempty_str(acct, "billingType").as_deref() {
                Some("stripe_subscription") => Some("Subscription".to_string()),
                Some(other) => Some(other.to_string()),
                None => None,
            };
        }
    }
    if !info.authenticated && std::env::var("ANTHROPIC_API_KEY").is_ok_and(|v| !v.is_empty()) {
        info.authenticated = true;
        info.auth_method = Some("apiKey");
    }
    info
}

// --- Codex -------------------------------------------------------------------

/// Decode a JWT's payload without verifying — we only surface the account
/// email and plan the user is already signed in as, locally.
fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

async fn detect_codex() -> HarnessInfo {
    let mut info = HarnessInfo::new("codex", "Codex");
    if let Some(bin) = find_on_path("codex") {
        info.installed = true;
        info.version = bin_version(&bin).await;
        info.bin_path = Some(bin.to_string_lossy().into_owned());
    }
    let Some(auth) = dirs::home_dir().and_then(|h| read_json(h.join(".codex").join("auth.json")))
    else {
        return info;
    };
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
    info
}

// --- OpenCode ----------------------------------------------------------------

fn opencode_auth_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local").join("share")))?;
    Some(base.join("opencode").join("auth.json"))
}

/// Providers opencode is signed into (its auth.json is `{provider: {type}}`).
fn opencode_providers() -> Vec<String> {
    let Some(auth) = opencode_auth_path().and_then(read_json) else {
        return Vec::new();
    };
    match auth.as_object() {
        Some(map) => map.keys().cloned().collect(),
        None => Vec::new(),
    }
}

/// `opencode models` — the ground truth for what the agent can actually run.
async fn opencode_models(bin: &PathBuf) -> Vec<String> {
    let fut = tokio::process::Command::new(bin)
        .arg("models")
        .current_dir(dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .stdin(std::process::Stdio::null())
        .output();
    let Ok(Ok(out)) = tokio::time::timeout(Duration::from_secs(20), fut).await else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && l.contains('/'))
        .map(str::to_string)
        .collect()
}

async fn detect_opencode() -> (HarnessInfo, Vec<String>) {
    let mut info = HarnessInfo::new("opencode", "OpenCode");
    let mut models = Vec::new();
    if let Ok(bin) = find_opencode() {
        info.installed = true;
        info.version = bin_version(&bin).await;
        models = opencode_models(&bin).await;
        info.bin_path = Some(bin.to_string_lossy().into_owned());
    }
    let providers = opencode_providers();
    if !providers.is_empty() {
        info.authenticated = true;
        info.auth_method = Some("oauth");
        info.account = Some(providers.join(", "));
    }
    (info, models)
}

// --- assembly ----------------------------------------------------------------

/// Each harness runs directly (its own CLI, the user's own login), so its
/// model list is its own: static ids for the Claude Code / Codex CLIs,
/// `opencode models` for opencode.
const CLAUDE_MODELS: [&str; 4] = [
    "claude-fable-5",
    "claude-sonnet-5",
    "claude-opus-4-8",
    "claude-haiku-4-5",
];

// ChatGPT-account codex rejects the -codex/-fast variants; these two are
// what `codex exec -m` accepts (verified against codex-cli 0.142).
const CODEX_MODELS: [&str; 2] = ["gpt-5.5", "gpt-5.4"];

pub async fn detect_harnesses() -> Vec<HarnessInfo> {
    let (mut claude, mut codex, (mut opencode, models)) =
        tokio::join!(detect_claude_code(), detect_codex(), detect_opencode());

    claude.agent_ready = claude.installed && claude.authenticated;
    if claude.agent_ready {
        claude.models = CLAUDE_MODELS
            .iter()
            .map(|id| ModelInfo { id: id.to_string() })
            .collect();
    } else {
        claude.agent_note =
            Some("Install Claude Code and sign in (`claude`) to chat with it here.".to_string());
    }

    codex.agent_ready = codex.installed && codex.authenticated;
    if codex.agent_ready {
        codex.models = CODEX_MODELS
            .iter()
            .map(|id| ModelInfo { id: id.to_string() })
            .collect();
    } else {
        codex.agent_note =
            Some("Install Codex and sign in (`codex login`) to chat with it here.".to_string());
    }

    opencode.agent_ready = opencode.installed;
    if opencode.agent_ready {
        opencode.models = models.into_iter().map(|id| ModelInfo { id }).collect();
    } else {
        opencode.agent_note = Some(
            "Install opencode (curl -fsSL https://opencode.ai/install | bash) to chat with it here."
                .to_string(),
        );
    }

    vec![claude, codex, opencode]
}
