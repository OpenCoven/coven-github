//! GitHub App installation token management.

use anyhow::Result;

/// Generates a fresh installation access token for the given installation ID.
/// Tokens expire after 1 hour; callers should cache and refresh.
pub async fn get_token(
    app_id: u64,
    private_key_pem: &str,
    installation_id: u64,
) -> Result<String> {
    // TODO: implement JWT signing + POST /app/installations/{id}/access_tokens
    tracing::info!(installation_id, "generating installation access token");
    Ok(String::new()) // placeholder
}
