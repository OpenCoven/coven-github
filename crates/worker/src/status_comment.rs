//! Marker-backed status comments (issue #13).
//!
//! Each familiar keeps ONE mutable status comment per issue/PR, identified by
//! a hidden HTML marker and edited in place through the task lifecycle —
//! repeated runs update the surface instead of stacking new comments.

use anyhow::Result;
use coven_github_api::pr;

/// Hidden marker identifying a familiar's status comment on a target.
pub fn marker(familiar_id: &str, repo_owner: &str, repo_name: &str, number: u64) -> String {
    format!("<!-- coven:{familiar_id}:{repo_owner}/{repo_name}#{number} -->")
}

/// Creates or edits-in-place the marker-backed status comment.
///
/// Searches the first 100 conversation comments; status comments are posted at
/// task start so they land early. Threads that exceed the page before the
/// familiar ever commented fall back to posting fresh rather than paging.
pub async fn upsert(
    api_base_url: &str,
    token: &str,
    repo_owner: &str,
    repo_name: &str,
    number: u64,
    marker: &str,
    body: &str,
) -> Result<()> {
    let full = format!("{marker}\n{body}");
    let comments =
        pr::list_comments_with_base_url(api_base_url, token, repo_owner, repo_name, number)
            .await?;
    if let Some(existing) = comments.iter().find(|c| c.body.contains(marker)) {
        pr::update_comment_with_base_url(
            api_base_url,
            token,
            repo_owner,
            repo_name,
            existing.id,
            &full,
        )
        .await
    } else {
        pr::post_comment_with_base_url(api_base_url, token, repo_owner, repo_name, number, &full)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_scoped_to_familiar_and_target() {
        assert_eq!(
            marker("cody", "OpenCoven", "coven-code", 42),
            "<!-- coven:cody:OpenCoven/coven-code#42 -->"
        );
        // Distinct targets and familiars must never collide.
        assert_ne!(
            marker("cody", "OpenCoven", "coven-code", 42),
            marker("cody", "OpenCoven", "coven-code", 43)
        );
        assert_ne!(
            marker("cody", "OpenCoven", "coven-code", 42),
            marker("nova", "OpenCoven", "coven-code", 42)
        );
    }
}
