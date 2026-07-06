//! GitHub App installation token management.

use anyhow::Result;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};

use crate::{client, send_json, GitHubRequest, DEFAULT_API_BASE_URL};

#[derive(Debug, Serialize)]
struct JwtClaims {
    iss: String,
    iat: u64,
    exp: u64,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: String,
}

/// Authority role a scoped installation token is minted for (issue #4).
///
/// Each role maps to the minimum GitHub App permission set for one phase of a
/// task's lifecycle, constrained to the single target repository. The agent
/// process only ever receives an `AgentGit` token; `Publication` is minted
/// after the result envelope has passed contract validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenRole {
    /// Adapter-held: resolve refs, drive Check Runs, post progress comments.
    Orchestration,
    /// Agent-held (`COVEN_GIT_TOKEN`): clone/fetch and push of the working
    /// branch. No issues, pull-request, or checks authority.
    AgentGit,
    /// Adapter-held, minted post-validation: open the draft PR and post the
    /// PR-opened comment.
    Publication,
}

impl TokenRole {
    fn permissions(self) -> serde_json::Value {
        match self {
            TokenRole::Orchestration => serde_json::json!({
                "contents": "read",
                "checks": "write",
                "issues": "write",
                "pull_requests": "read",
            }),
            TokenRole::AgentGit => serde_json::json!({ "contents": "write" }),
            TokenRole::Publication => serde_json::json!({
                "contents": "read",
                "issues": "write",
                "pull_requests": "write",
            }),
        }
    }
}

/// Generates a fresh installation access token constrained to one repository
/// and one [`TokenRole`]'s permission set. Tokens expire after 1 hour.
pub async fn get_scoped_token(
    app_id: u64,
    private_key_pem: &str,
    installation_id: u64,
    repo_name: &str,
    role: TokenRole,
) -> Result<String> {
    get_scoped_token_with_base_url(
        DEFAULT_API_BASE_URL,
        app_id,
        private_key_pem,
        installation_id,
        repo_name,
        role,
    )
    .await
}

pub async fn get_scoped_token_with_base_url(
    api_base_url: &str,
    app_id: u64,
    private_key_pem: &str,
    installation_id: u64,
    repo_name: &str,
    role: TokenRole,
) -> Result<String> {
    tracing::info!(
        installation_id,
        ?role,
        "generating scoped installation access token"
    );
    let jwt = app_jwt(app_id, private_key_pem)?;
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        &jwt,
        scoped_access_token_request(installation_id, repo_name, role),
    )
    .await?;
    let body: TokenResponse = response.json().await?;
    Ok(body.token)
}

fn app_jwt(app_id: u64, private_key_pem: &str) -> Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())?;
    Ok(encode(
        &Header::new(Algorithm::RS256),
        &jwt_claims(app_id, now),
        &key,
    )?)
}

fn jwt_claims(app_id: u64, now: u64) -> JwtClaims {
    JwtClaims {
        iss: app_id.to_string(),
        iat: now.saturating_sub(60),
        exp: now + 540,
    }
}

fn scoped_access_token_request(
    installation_id: u64,
    repo_name: &str,
    role: TokenRole,
) -> GitHubRequest {
    GitHubRequest {
        method: "POST",
        path: format!("/app/installations/{installation_id}/access_tokens"),
        // `repositories` takes bare repo names (not owner/name). Requesting a
        // permission the App was not installed with fails with 422; the App
        // manifest grants contents/checks/issues/pull_requests, all superset.
        body: serde_json::json!({
            "repositories": [repo_name],
            "permissions": role.permissions(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn scoped_token_request_targets_installation_endpoint() {
        let request = scoped_access_token_request(12345, "coven-code", TokenRole::Orchestration);

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/app/installations/12345/access_tokens");
    }

    #[test]
    fn agent_git_scope_grants_contents_write_only() {
        let request = scoped_access_token_request(1, "coven-code", TokenRole::AgentGit);

        assert_eq!(request.body["repositories"], json!(["coven-code"]));
        assert_eq!(request.body["permissions"], json!({ "contents": "write" }));
    }

    #[test]
    fn orchestration_scope_cannot_write_contents_or_pulls() {
        let request = scoped_access_token_request(1, "coven-code", TokenRole::Orchestration);

        assert_eq!(request.body["repositories"], json!(["coven-code"]));
        assert_eq!(
            request.body["permissions"],
            json!({
                "contents": "read",
                "checks": "write",
                "issues": "write",
                "pull_requests": "read",
            })
        );
    }

    #[test]
    fn publication_scope_grants_pr_write_without_checks_or_contents_write() {
        let request = scoped_access_token_request(1, "coven-code", TokenRole::Publication);

        assert_eq!(request.body["repositories"], json!(["coven-code"]));
        assert_eq!(
            request.body["permissions"],
            json!({
                "contents": "read",
                "issues": "write",
                "pull_requests": "write",
            })
        );
    }

    #[test]
    fn jwt_claims_use_app_id_as_issuer_and_short_expiry() {
        let now = 1_700_000_000;
        let claims = jwt_claims(99, now);

        assert_eq!(claims.iss, "99");
        assert_eq!(claims.iat, now - 60);
        assert_eq!(claims.exp, now + 540);
    }
}
