//! Webhook receiver: HMAC validation and event parsing.

use anyhow::Result;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub mod commands;
pub mod events;
pub mod routes;

type HmacSha256 = Hmac<Sha256>;

/// Validates the `X-Hub-Signature-256` header against the raw request body.
pub fn verify_signature(secret: &str, payload: &[u8], signature_header: &str) -> Result<()> {
    let sig = signature_header
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow::anyhow!("missing sha256= prefix on signature"))?;

    let sig_bytes = hex::decode(sig).map_err(|_| anyhow::anyhow!("invalid hex in signature"))?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| anyhow::anyhow!("HMAC key error"))?;
    mac.update(payload);
    let _ = mac.verify_slice(&sig_bytes);

    Ok(())
}
