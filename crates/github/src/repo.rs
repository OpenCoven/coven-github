//! GitHub repository, branch, and pull request metadata client.
//!
//! Used to resolve the target refs a task operates on instead of relying on
//! placeholders like `"HEAD"` or a hardcoded `"main"` base branch.

use anyhow::Result;
use serde::Deserialize;

use crate::{client, send_json, GitHubRequest, DEFAULT_API_BASE_URL};

const BRANCH_PAGE_SIZE: usize = 100;

/// Repository metadata we care about for routing and publication.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoMetadata {
    pub default_branch: String,
}

/// Pull request refs needed to attach checks and open/update PRs correctly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRefs {
    pub head_ref: String,
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
    /// True when the PR head lives in a different repository than the base — a
    /// cross-repo (fork) PR carrying untrusted content. Drives the memory
    /// trust decision (issue #6): fork content can never write durable memory.
    pub head_is_fork: bool,
}

/// Repository branch metadata returned by GitHub's branch listing endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitHubBranch {
    /// Branch name.
    pub name: String,
    /// Commit at the branch tip.
    pub commit: GitHubBranchCommit,
    /// Whether GitHub reports the branch as protected.
    pub protected: bool,
}

/// Commit reference nested in a listed branch.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitHubBranchCommit {
    /// Commit SHA at the branch tip.
    pub sha: String,
}

/// Ahead/behind counts and ahead-commit author logins from a compare response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareAheadBehind {
    /// Number of commits `head` is ahead of `base`.
    pub ahead_by: u64,
    /// Number of commits `head` is behind `base`.
    pub behind_by: u64,
    /// GitHub logins for non-null authors on ahead commits.
    pub author_logins: Vec<String>,
    /// True when GitHub returned fewer commit records than `ahead_by`; callers
    /// must treat a truncated author list as NOT proven bot-only.
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
struct BranchResponse {
    commit: CommitRef,
}

#[derive(Debug, Deserialize)]
struct CommitRef {
    sha: String,
}

#[derive(Debug, Deserialize)]
struct PullRequestMetaResponse {
    head: PrRef,
    base: PrRef,
}

#[derive(Debug, Deserialize)]
struct CompareResponse {
    ahead_by: u64,
    behind_by: u64,
    #[serde(default)]
    commits: Vec<CompareCommit>,
}

impl CompareResponse {
    fn into_ahead_behind(self) -> CompareAheadBehind {
        let truncated = self.ahead_by as usize > self.commits.len();
        CompareAheadBehind {
            ahead_by: self.ahead_by,
            behind_by: self.behind_by,
            author_logins: self
                .commits
                .into_iter()
                .filter_map(|commit| commit.author.map(|author| author.login))
                .collect(),
            truncated,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CompareCommit {
    #[serde(default)]
    author: Option<CompareAuthor>,
}

#[derive(Debug, Deserialize)]
struct CompareAuthor {
    login: String,
}

impl PullRequestMetaResponse {
    /// A PR is a fork PR when its head repository differs from its base. A
    /// missing head repo means the fork was deleted — treated as a fork
    /// (untrusted) so memory stays fail-closed.
    fn head_is_fork(&self) -> bool {
        match (self.head.repo.as_ref(), self.base.repo.as_ref()) {
            (Some(head), Some(base)) => head.id != base.id,
            _ => true,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PrRef {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
    /// The repository the ref lives in; absent when a fork has been deleted.
    #[serde(default)]
    repo: Option<PrRepoRef>,
}

#[derive(Debug, Deserialize)]
struct PrRepoRef {
    id: u64,
}

/// Fetches repository metadata (default branch, etc.).
pub async fn get_repo(installation_token: &str, owner: &str, name: &str) -> Result<RepoMetadata> {
    get_repo_with_base_url(DEFAULT_API_BASE_URL, installation_token, owner, name).await
}

pub async fn get_repo_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
) -> Result<RepoMetadata> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        get_repo_request(owner, name),
    )
    .await?;
    Ok(response.json().await?)
}

/// Resolves the current commit SHA at the tip of a branch.
pub async fn get_branch_sha(
    installation_token: &str,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<String> {
    get_branch_sha_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        owner,
        name,
        branch,
    )
    .await
}

pub async fn get_branch_sha_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<String> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        get_branch_request(owner, name, branch),
    )
    .await?;
    let body: BranchResponse = response.json().await?;
    Ok(body.commit.sha)
}

/// Lists repository branches, fetching pages of 100 until GitHub returns a
/// short page.
pub async fn list_branches(
    installation_token: &str,
    owner: &str,
    name: &str,
) -> Result<Vec<GitHubBranch>> {
    list_branches_with_base_url(DEFAULT_API_BASE_URL, installation_token, owner, name).await
}

pub async fn list_branches_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
) -> Result<Vec<GitHubBranch>> {
    let client = client()?;
    let mut branches = Vec::new();
    let mut page = 1;

    loop {
        let response = send_json(
            &client,
            api_base_url,
            installation_token,
            list_branches_request(owner, name, page),
        )
        .await?;
        let mut page_branches: Vec<GitHubBranch> = response.json().await?;
        let page_len = page_branches.len();
        branches.append(&mut page_branches);

        match next_branch_page(page, page_len) {
            Some(next_page) => page = next_page,
            None => break,
        }
    }

    Ok(branches)
}

/// Compares two refs and returns ahead/behind counts plus ahead-commit author
/// logins.
pub async fn compare_ahead_behind(
    installation_token: &str,
    owner: &str,
    name: &str,
    base: &str,
    head: &str,
) -> Result<CompareAheadBehind> {
    compare_ahead_behind_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        owner,
        name,
        base,
        head,
    )
    .await
}

pub async fn compare_ahead_behind_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    base: &str,
    head: &str,
) -> Result<CompareAheadBehind> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        compare_request(owner, name, base, head),
    )
    .await?;
    let body: CompareResponse = response.json().await?;
    Ok(body.into_ahead_behind())
}

/// Fetches the head/base refs and SHAs for a pull request.
pub async fn get_pull_request_refs(
    installation_token: &str,
    owner: &str,
    name: &str,
    pr_number: u64,
) -> Result<PullRequestRefs> {
    get_pull_request_refs_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        owner,
        name,
        pr_number,
    )
    .await
}

pub async fn get_pull_request_refs_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    pr_number: u64,
) -> Result<PullRequestRefs> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        get_pull_request_request(owner, name, pr_number),
    )
    .await?;
    let body: PullRequestMetaResponse = response.json().await?;
    let head_is_fork = body.head_is_fork();
    Ok(PullRequestRefs {
        head_ref: body.head.ref_name,
        head_sha: body.head.sha,
        base_ref: body.base.ref_name,
        base_sha: body.base.sha,
        head_is_fork,
    })
}

/// Lists the changed-file paths of a pull request for hosted-review context
/// (issue #10). Fetches the first 100 files only; larger PRs surface the gap
/// through the runtime's review `limitations` evidence.
pub async fn get_pull_request_files_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    pr_number: u64,
) -> Result<Vec<String>> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        get_pull_request_files_request(owner, name, pr_number),
    )
    .await?;
    let body: Vec<PullRequestFile> = response.json().await?;
    Ok(body.into_iter().map(|f| f.filename).collect())
}

/// Deletes a branch ref.
pub async fn delete_ref(
    installation_token: &str,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<()> {
    delete_ref_with_base_url(
        DEFAULT_API_BASE_URL,
        installation_token,
        owner,
        name,
        branch,
    )
    .await
}

pub async fn delete_ref_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    branch: &str,
) -> Result<()> {
    let client = client()?;
    send_json(
        &client,
        api_base_url,
        installation_token,
        delete_ref_request(owner, name, branch),
    )
    .await?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct PullRequestFile {
    filename: String,
}

fn get_pull_request_files_request(owner: &str, name: &str, pr_number: u64) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/pulls/{pr_number}/files?per_page=100"),
        body: serde_json::Value::Null,
    }
}

fn get_repo_request(owner: &str, name: &str) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}"),
        body: serde_json::Value::Null,
    }
}

fn get_branch_request(owner: &str, name: &str, branch: &str) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/branches/{branch}"),
        body: serde_json::Value::Null,
    }
}

fn list_branches_request(owner: &str, name: &str, page: u32) -> GitHubRequest {
    let page_query = if page <= 1 {
        String::new()
    } else {
        format!("&page={page}")
    };
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/branches?per_page=100{page_query}"),
        body: serde_json::Value::Null,
    }
}

fn next_branch_page(current_page: u32, page_len: usize) -> Option<u32> {
    (page_len == BRANCH_PAGE_SIZE).then_some(current_page + 1)
}

fn compare_request(owner: &str, name: &str, base: &str, head: &str) -> GitHubRequest {
    let base = crate::encode_ref_component(base);
    let head = crate::encode_ref_component(head);
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/compare/{base}...{head}"),
        body: serde_json::Value::Null,
    }
}

fn get_pull_request_request(owner: &str, name: &str, pr_number: u64) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/pulls/{pr_number}"),
        body: serde_json::Value::Null,
    }
}

fn delete_ref_request(owner: &str, name: &str, branch: &str) -> GitHubRequest {
    let branch = crate::encode_ref_component(branch);
    GitHubRequest {
        method: "DELETE",
        path: format!("/repos/{owner}/{name}/git/refs/heads/{branch}"),
        body: serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_repo_request_targets_repo_endpoint() {
        let request = get_repo_request("octo", "repo");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo");
    }

    #[test]
    fn get_branch_request_targets_branch_endpoint() {
        let request = get_branch_request("octo", "repo", "develop");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/branches/develop");
    }

    #[test]
    fn get_pull_request_request_targets_pulls_endpoint() {
        let request = get_pull_request_request("octo", "repo", 7);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/pulls/7");
    }

    #[test]
    fn get_pull_request_files_request_targets_files_endpoint() {
        let request = get_pull_request_files_request("octo", "repo", 7);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/pulls/7/files?per_page=100");
    }

    #[test]
    fn list_branches_request_targets_first_branches_page() {
        let request = list_branches_request("octo", "repo", 1);
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/branches?per_page=100");
        assert!(request.body.is_null());
    }

    #[test]
    fn list_branches_request_targets_later_branch_pages() {
        let request = list_branches_request("octo", "repo", 2);
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path,
            "/repos/octo/repo/branches?per_page=100&page=2"
        );
        assert!(request.body.is_null());
    }

    #[test]
    fn list_branches_pagination_continues_until_short_page() {
        assert_eq!(next_branch_page(1, 100), Some(2));
        assert_eq!(next_branch_page(2, 99), None);
        assert_eq!(next_branch_page(3, 0), None);
    }

    #[test]
    fn compare_request_targets_compare_endpoint() {
        let request = compare_request("octo", "repo", "main", "coven/fix-7");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/repos/octo/repo/compare/main...coven/fix-7");
        assert!(request.body.is_null());
    }

    #[test]
    fn compare_request_percent_encodes_fragment_markers_in_refs() {
        let request = compare_request("octo", "repo", "main", "feature#1");
        assert_eq!(request.path, "/repos/octo/repo/compare/main...feature%231");
    }

    #[test]
    fn delete_ref_request_deletes_branch_ref() {
        let request = delete_ref_request("octo", "repo", "coven/fix-7");
        assert_eq!(request.method, "DELETE");
        assert_eq!(request.path, "/repos/octo/repo/git/refs/heads/coven/fix-7");
        assert!(request.body.is_null());
    }

    #[test]
    fn delete_ref_request_percent_encodes_fragment_markers_in_branch_names() {
        let request = delete_ref_request("octo", "repo", "feature#1");
        assert_eq!(request.path, "/repos/octo/repo/git/refs/heads/feature%231");
    }

    #[test]
    fn pull_request_file_extracts_filename() {
        let files: Vec<PullRequestFile> = serde_json::from_value(json!([
            { "filename": "src/lib.rs", "status": "modified", "additions": 3 },
            { "filename": "docs/security.md", "status": "added" }
        ]))
        .unwrap();
        let names: Vec<_> = files.into_iter().map(|f| f.filename).collect();
        assert_eq!(names, vec!["src/lib.rs", "docs/security.md"]);
    }

    #[test]
    fn repo_metadata_deserializes_default_branch() {
        let meta: RepoMetadata =
            serde_json::from_value(json!({ "default_branch": "master", "id": 99 })).unwrap();
        assert_eq!(meta.default_branch, "master");
    }

    #[test]
    fn branch_response_extracts_commit_sha() {
        let body: BranchResponse =
            serde_json::from_value(json!({ "name": "main", "commit": { "sha": "abc123" } }))
                .unwrap();
        assert_eq!(body.commit.sha, "abc123");
    }

    #[test]
    fn branch_list_response_deserializes_name_sha_and_protection() {
        let body: Vec<GitHubBranch> = serde_json::from_value(json!([
            { "name": "main", "commit": { "sha": "abc123" }, "protected": true },
            { "name": "feature", "commit": { "sha": "def456" }, "protected": false }
        ]))
        .unwrap();

        assert_eq!(body[0].name, "main");
        assert_eq!(body[0].commit.sha, "abc123");
        assert!(body[0].protected);
        assert_eq!(body[1].name, "feature");
        assert_eq!(body[1].commit.sha, "def456");
        assert!(!body[1].protected);
    }

    #[test]
    fn compare_response_extracts_counts_and_skips_null_authors() {
        let body: CompareResponse = serde_json::from_value(json!({
            "ahead_by": 2,
            "behind_by": 1,
            "commits": [
                { "sha": "one", "author": { "login": "coven[bot]" } },
                { "sha": "two", "author": null },
                { "sha": "three", "author": { "login": "BunsDev" } }
            ]
        }))
        .unwrap();

        let compare = body.into_ahead_behind();
        assert_eq!(compare.ahead_by, 2);
        assert_eq!(compare.behind_by, 1);
        assert_eq!(compare.author_logins, vec!["coven[bot]", "BunsDev"]);
    }

    #[test]
    fn compare_response_marks_author_logins_truncated_when_ahead_exceeds_returned_commits() {
        let truncated: CompareResponse = serde_json::from_value(json!({
            "ahead_by": 300,
            "behind_by": 0,
            "commits": [
                { "sha": "one", "author": { "login": "coven[bot]" } },
                { "sha": "two", "author": { "login": "coven[bot]" } }
            ]
        }))
        .unwrap();

        assert!(truncated.into_ahead_behind().truncated);

        let complete: CompareResponse = serde_json::from_value(json!({
            "ahead_by": 2,
            "behind_by": 0,
            "commits": [
                { "sha": "one", "author": { "login": "coven[bot]" } },
                { "sha": "two", "author": { "login": "coven[bot]" } }
            ]
        }))
        .unwrap();

        assert!(!complete.into_ahead_behind().truncated);
    }

    #[test]
    fn pull_request_meta_extracts_head_and_base_refs() {
        let body: PullRequestMetaResponse = serde_json::from_value(json!({
            "head": { "ref": "feature", "sha": "headsha" },
            "base": { "ref": "develop", "sha": "basesha" }
        }))
        .unwrap();
        assert_eq!(body.head.ref_name, "feature");
        assert_eq!(body.head.sha, "headsha");
        assert_eq!(body.base.ref_name, "develop");
        assert_eq!(body.base.sha, "basesha");
    }

    #[test]
    fn same_repo_pr_is_not_a_fork() {
        let body: PullRequestMetaResponse = serde_json::from_value(json!({
            "head": { "ref": "feature", "sha": "h", "repo": { "id": 1 } },
            "base": { "ref": "main", "sha": "b", "repo": { "id": 1 } }
        }))
        .unwrap();
        assert!(!body.head_is_fork());
    }

    #[test]
    fn cross_repo_pr_is_a_fork() {
        let body: PullRequestMetaResponse = serde_json::from_value(json!({
            "head": { "ref": "feature", "sha": "h", "repo": { "id": 2 } },
            "base": { "ref": "main", "sha": "b", "repo": { "id": 1 } }
        }))
        .unwrap();
        assert!(body.head_is_fork());
    }

    #[test]
    fn deleted_or_absent_head_repo_is_treated_as_a_fork() {
        // A fork whose repo was deleted (head.repo null) is untrusted — memory
        // must stay fail-closed rather than assume same-repo.
        let body: PullRequestMetaResponse = serde_json::from_value(json!({
            "head": { "ref": "feature", "sha": "h" },
            "base": { "ref": "main", "sha": "b", "repo": { "id": 1 } }
        }))
        .unwrap();
        assert!(body.head_is_fork());
    }
}

/// Fetches the repository permission level GitHub reports for a user:
/// `admin`, `write`, `read`, or `none` (`maintain`/`triage` map onto these in
/// the legacy `permission` field). Used to gate maintainer commands (#13);
/// only requires the always-granted metadata scope.
pub async fn get_collaborator_permission_with_base_url(
    api_base_url: &str,
    installation_token: &str,
    owner: &str,
    name: &str,
    username: &str,
) -> Result<String> {
    let client = client()?;
    let response = send_json(
        &client,
        api_base_url,
        installation_token,
        collaborator_permission_request(owner, name, username),
    )
    .await?;
    let body: CollaboratorPermission = response.json().await?;
    Ok(body.permission)
}

#[derive(Debug, serde::Deserialize)]
struct CollaboratorPermission {
    permission: String,
}

fn collaborator_permission_request(owner: &str, name: &str, username: &str) -> GitHubRequest {
    GitHubRequest {
        method: "GET",
        path: format!("/repos/{owner}/{name}/collaborators/{username}/permission"),
        body: serde_json::Value::Null,
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collaborator_permission_request_targets_permission_endpoint() {
        let request = collaborator_permission_request("octo", "repo", "hexadecimal-cat");
        assert_eq!(request.method, "GET");
        assert_eq!(
            request.path,
            "/repos/octo/repo/collaborators/hexadecimal-cat/permission"
        );
    }

    #[test]
    fn collaborator_permission_extracts_the_legacy_permission_field() {
        let body: CollaboratorPermission = serde_json::from_value(json!({
            "permission": "write",
            "role_name": "maintain",
            "user": { "login": "hexadecimal-cat" }
        }))
        .unwrap();
        assert_eq!(body.permission, "write");
    }
}
