//! Minimal GitHub REST calls for local mode — create a repo on the signed-in
//! user's account or in an organization, check push access, fork-by-copy.
//! Token from `GITHUB_TOKEN` or `gh auth token`, same resolution the clone path
//! uses.

use serde_json::{json, Value};

use super::git::resolve_github_token;
use crate::error::{anyhow, Result};

const UA: &str = concat!("orx/", env!("CARGO_PKG_VERSION"));

/// Create a blank private repo with an auto-init commit so the clone/branch
/// flow works immediately. An organization target is optional; without one,
/// GitHub creates the repo under the token's user.
pub async fn create_repo(
    repo: &str,
    organization: Option<&str>,
) -> Result<(String, String, String)> {
    create_repo_api(repo, true, organization).await
}

/// Whether the token can push to `owner/repo`. `None` means "could not
/// determine" (no token, network error, auth trouble) — callers should treat
/// that as access rather than surprise-forking on a transient failure.
pub async fn has_push_access(owner: &str, repo: &str) -> Option<bool> {
    let token = resolve_github_token()?;
    let res = reqwest::Client::new()
        .get(format!("https://api.github.com/repos/{owner}/{repo}"))
        .bearer_auth(&token)
        .header("user-agent", UA)
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    match res.status() {
        // Not visible with this token: definitely can't push.
        reqwest::StatusCode::NOT_FOUND => Some(false),
        s if s.is_success() => {
            let body: Value = res.json().await.ok()?;
            Some(
                body.pointer("/permissions/push")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )
        }
        _ => None,
    }
}

/// Fork-by-copy: snapshot `src_owner/src_repo` (at `src_branch`, or its
/// default) into a fresh private repo `<slug>-<hash>` on the token's user —
/// the same import convention the platform uses — so the project always ends
/// up on a repo the user can push to. Returns (owner, repo, default_branch).
pub async fn fork_copy_repo(
    src_owner: &str,
    src_repo: &str,
    src_branch: Option<String>,
    destination_organization: Option<&str>,
) -> Result<(String, String, String)> {
    let hash = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let name = format!("{}-{hash}", crate::local::slugify(src_repo));
    let (owner, name, _) = create_repo_api(&name, false, destination_organization).await?;
    let (src_owner, src_repo) = (src_owner.to_string(), src_repo.to_string());
    let (dst_owner, dst_repo) = (owner.clone(), name.clone());
    tokio::task::spawn_blocking(move || {
        super::git::seed_copy(
            &src_owner,
            &src_repo,
            src_branch.as_deref(),
            &dst_owner,
            &dst_repo,
        )
    })
    .await
    .map_err(|e| anyhow!("seed task failed: {e}"))??;
    Ok((owner, name, "main".to_string()))
}

fn create_repo_endpoint(organization: Option<&str>) -> Result<String> {
    let Some(organization) = organization
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok("https://api.github.com/user/repos".to_string());
    };
    let valid = organization.len() <= 39
        && !organization.starts_with('-')
        && !organization.ends_with('-')
        && !organization.contains("--")
        && organization
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-');
    if !valid {
        return Err(anyhow!("Invalid GitHub organization login: {organization}"));
    }
    Ok(format!("https://api.github.com/orgs/{organization}/repos"))
}

async fn create_repo_api(
    repo: &str,
    auto_init: bool,
    organization: Option<&str>,
) -> Result<(String, String, String)> {
    let token = resolve_github_token().ok_or_else(|| {
        anyhow!(
            "Creating a GitHub repo needs credentials — run `gh auth login` or set GITHUB_TOKEN."
        )
    })?;
    let res = reqwest::Client::new()
        .post(create_repo_endpoint(organization)?)
        .bearer_auth(&token)
        .header("user-agent", UA)
        .header("accept", "application/vnd.github+json")
        .json(&json!({ "name": repo, "private": true, "auto_init": auto_init }))
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

#[cfg(test)]
mod tests {
    use super::create_repo_endpoint;

    #[test]
    fn repository_endpoint_targets_user_or_organization() {
        assert_eq!(
            create_repo_endpoint(None).unwrap(),
            "https://api.github.com/user/repos"
        );
        assert_eq!(
            create_repo_endpoint(Some("alphaXiv")).unwrap(),
            "https://api.github.com/orgs/alphaXiv/repos"
        );
        assert!(create_repo_endpoint(Some("not/an/org")).is_err());
    }
}
