//! Minimal GitHub REST call for local mode — create a repo on the signed-in
//! user's account. Token from `GITHUB_TOKEN` or `gh auth token`, same
//! resolution the clone path uses.

use serde_json::{json, Value};

use super::git::resolve_github_token;
use crate::error::{anyhow, Result};

const UA: &str = concat!("orx/", env!("CARGO_PKG_VERSION"));

/// Create a private repo named `repo` under the token's user. `auto_init`
/// seeds the first commit so the clone/branch flow works immediately on an
/// otherwise blank repo. Returns (owner, repo, default_branch).
pub async fn create_user_repo(repo: &str) -> Result<(String, String, String)> {
    let token = resolve_github_token().ok_or_else(|| {
        anyhow!(
            "Creating a GitHub repo needs credentials — run `gh auth login` or set GITHUB_TOKEN."
        )
    })?;
    let res = reqwest::Client::new()
        .post("https://api.github.com/user/repos")
        .bearer_auth(&token)
        .header("user-agent", UA)
        .header("accept", "application/vnd.github+json")
        .json(&json!({ "name": repo, "private": true, "auto_init": true }))
        .send()
        .await
        .map_err(|e| anyhow!("GitHub API unreachable: {e}"))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
        // Typically "name already exists on this account".
        let detail = body
            .pointer("/errors/0/message")
            .and_then(Value::as_str)
            .unwrap_or("invalid repository name");
        return Err(anyhow!("Could not create '{repo}': {detail}."));
    }
    if !status.is_success() {
        let msg = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(anyhow!("GitHub repo create failed ({status}): {msg}"));
    }
    let owner = body
        .pointer("/owner/login")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("GitHub response missing owner login"))?
        .to_string();
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(repo)
        .to_string();
    let default_branch = body
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or("main")
        .to_string();
    Ok((owner, name, default_branch))
}
