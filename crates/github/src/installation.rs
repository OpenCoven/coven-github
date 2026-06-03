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

/// Generates a fresh installation access token for the given installation ID.
/// Tokens expire after 1 hour; callers should cache and refresh.
pub async fn get_token(app_id: u64, private_key_pem: &str, installation_id: u64) -> Result<String> {
    get_token_with_base_url(
        DEFAULT_API_BASE_URL,
        app_id,
        private_key_pem,
        installation_id,
    )
    .await
}

pub async fn get_token_with_base_url(
    api_base_url: &str,
    app_id: u64,
    private_key_pem: &str,
    installation_id: u64,
) -> Result<String> {
    tracing::info!(installation_id, "generating installation access token");
    let jwt = app_jwt(app_id, private_key_pem)?;
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        &jwt,
        access_token_request(installation_id),
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

fn access_token_request(installation_id: u64) -> GitHubRequest {
    GitHubRequest {
        method: "POST",
        path: format!("/app/installations/{installation_id}/access_tokens"),
        body: serde_json::json!({}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_request_targets_installation_endpoint() {
        let request = access_token_request(12345);

        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/app/installations/12345/access_tokens");
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
