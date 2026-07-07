//! Worker execution backends (issue #5).
//!
//! Self-hosted development runs `coven-code` directly on the host; hosted
//! OpenCoven must not — tasks clone private repositories and execute project
//! commands. The container backend runs each task attempt in a fresh
//! container with resource limits and a hardened profile, torn down
//! unconditionally by `--rm` plus an explicit kill on timeout.
//!
//! Git authority is injected by environment variable *name* (`-e NAME`
//! inherits the value from the docker CLI's own environment), so the token
//! never appears in any argv, container inspect output, or shell history.

use std::path::{Path, PathBuf};
use std::time::Duration;

use coven_github_config::{ContainerConfig, WorkerBackendKind, WorkerConfig};
use tokio::process::Command;
use tracing::warn;

/// Filename of the session brief inside the task workspace.
pub const BRIEF_FILE: &str = "session-brief.json";
/// Filename of the result envelope inside the task workspace.
pub const RESULT_FILE: &str = "result.json";
/// Workspace mount point inside the container.
const CONTAINER_WORKSPACE: &str = "/workspace";

/// How one `coven-code` launch ended, before contract classification.
#[derive(Debug)]
pub enum LaunchOutcome {
    /// The process (or container) exited; `None` = killed by signal.
    Exited(Option<i32>),
    /// The wall-clock limit expired; the session was killed.
    TimedOut,
    /// The backend could not launch or await the session.
    Failed(String),
}

/// Execution backend, chosen by `worker.backend`.
pub enum Backend {
    Host,
    Container(ContainerConfig),
}

impl Backend {
    pub fn from_config(worker: &WorkerConfig) -> Self {
        match worker.backend {
            WorkerBackendKind::Host => Backend::Host,
            WorkerBackendKind::Container => Backend::Container(worker.container.clone()),
        }
    }

    /// The workspace path as the runtime will see it: the host path for host
    /// execution, the container mount point for container execution. The
    /// session brief must reference this view (the brief travels into the
    /// sandbox; host paths would be meaningless there).
    pub fn workspace_view(&self, host_workspace: &Path) -> PathBuf {
        match self {
            Backend::Host => host_workspace.to_path_buf(),
            Backend::Container(_) => PathBuf::from(CONTAINER_WORKSPACE),
        }
    }

    /// True when a nonstandard exit code plausibly means the sandbox limit
    /// fired (docker reports SIGKILL terminations — OOM kill, `docker kill`
    /// — as exit 137).
    pub fn explains_kill(&self, code: i32) -> Option<&'static str> {
        match self {
            Backend::Container(_) if code == 137 => {
                Some("container killed (exit 137) — memory limit or forced stop")
            }
            _ => None,
        }
    }

    /// Runs one `coven-code --headless` session to completion or timeout.
    /// The brief must already be at `<host_workspace>/session-brief.json`;
    /// the result is expected at `<host_workspace>/result.json` (both paths
    /// as seen from the host).
    pub async fn run(
        &self,
        worker: &WorkerConfig,
        host_workspace: &Path,
        task_id: &str,
        git_token: &str,
    ) -> LaunchOutcome {
        let timeout = Duration::from_secs(worker.timeout_secs);
        match self {
            Backend::Host => {
                let mut command = Command::new(&worker.coven_code_bin);
                command
                    .arg("--headless")
                    .arg("--context")
                    .arg(host_workspace.join(BRIEF_FILE))
                    .arg("--output")
                    .arg(host_workspace.join(RESULT_FILE))
                    // Git auth is injected via the environment, never written
                    // to the session brief or any durable artifact (issue #4).
                    .env("COVEN_GIT_TOKEN", git_token);
                await_child(command, timeout, None).await
            }
            Backend::Container(container) => {
                let name = container_name(task_id);
                let mut command = Command::new(&container.docker_bin);
                command.args(docker_run_args(container, host_workspace, &name));
                // `-e COVEN_GIT_TOKEN` (name-only) inherits the value from
                // the docker CLI's environment — set here, absent from argv.
                command.env("COVEN_GIT_TOKEN", git_token);
                let kill = KillSpec {
                    docker_bin: container.docker_bin.clone(),
                    name,
                };
                await_child(command, timeout, Some(kill)).await
            }
        }
    }
}

/// How to stop a containerized session whose docker CLI we killed.
struct KillSpec {
    docker_bin: PathBuf,
    name: String,
}

/// Container name for one task attempt. Unique per attempt: docker rejects
/// duplicate names, so a stale name must never collide with a retry.
fn container_name(task_id: &str) -> String {
    format!("coven-task-{task_id}-{}", uuid::Uuid::new_v4().simple())
}

/// The full `docker run` argv (everything after the docker binary itself).
/// Pure so tests can pin the isolation profile.
pub fn docker_run_args(
    container: &ContainerConfig,
    host_workspace: &Path,
    name: &str,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        name.into(),
        // Resource limits.
        "--cpus".into(),
        container.cpus.to_string(),
        "--memory".into(),
        container.memory.clone(),
        "--pids-limit".into(),
        container.pids.to_string(),
        // Egress policy.
        "--network".into(),
        container.network.clone(),
        // Hardened profile: read-only root, no capabilities, no privilege
        // escalation; writable space is the workspace mount plus a bounded
        // tmpfs.
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--tmpfs".into(),
        format!("/tmp:size={}", container.tmpfs_size),
        // Only the task workspace is mounted; no other host state exists
        // inside the sandbox.
        "-v".into(),
        format!("{}:{CONTAINER_WORKSPACE}", host_workspace.display()),
        "-w".into(),
        CONTAINER_WORKSPACE.into(),
        // Name-only env forwarding: the value never enters argv.
        "-e".into(),
        "COVEN_GIT_TOKEN".into(),
        container.image.clone(),
    ];
    args.extend([
        container.coven_code_bin.clone(),
        "--headless".into(),
        "--context".into(),
        format!("{CONTAINER_WORKSPACE}/{BRIEF_FILE}"),
        "--output".into(),
        format!("{CONTAINER_WORKSPACE}/{RESULT_FILE}"),
    ]);
    args
}

async fn await_child(
    mut command: Command,
    timeout: Duration,
    kill: Option<KillSpec>,
) -> LaunchOutcome {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return LaunchOutcome::Failed(format!("failed to spawn session: {e}")),
    };
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => LaunchOutcome::Exited(status.code()),
        Ok(Err(e)) => LaunchOutcome::Failed(format!("failed to await session: {e}")),
        Err(_) => {
            // Kill the CLI process first…
            let _ = child.kill().await;
            let _ = child.wait().await;
            // …then the container itself: killing the docker CLI does not
            // reliably stop the container it launched.
            if let Some(kill) = kill {
                match Command::new(&kill.docker_bin)
                    .args(["kill", &kill.name])
                    .output()
                    .await
                {
                    Ok(_) => {}
                    Err(e) => warn!(container = %kill.name, "docker kill failed: {e}"),
                }
            }
            LaunchOutcome::TimedOut
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container() -> ContainerConfig {
        ContainerConfig::default()
    }

    #[test]
    fn docker_argv_pins_the_isolation_profile() {
        let args = docker_run_args(&container(), Path::new("/srv/tasks/t1"), "coven-task-x");
        let joined = args.join(" ");
        for expected in [
            "run --rm",
            "--cpus 1",
            "--memory 2g",
            "--pids-limit 512",
            "--network bridge",
            "--read-only",
            "--cap-drop ALL",
            "--security-opt no-new-privileges",
            "--tmpfs /tmp:size=256m",
            "-v /srv/tasks/t1:/workspace",
            "-w /workspace",
            "-e COVEN_GIT_TOKEN",
            "--headless --context /workspace/session-brief.json --output /workspace/result.json",
        ] {
            assert!(joined.contains(expected), "missing `{expected}` in: {joined}");
        }
    }

    #[test]
    fn token_values_never_enter_argv() {
        // The env var is forwarded by NAME; no argv element may ever carry a
        // value for it.
        let args = docker_run_args(&container(), Path::new("/srv/tasks/t1"), "n");
        let position = args.iter().position(|a| a == "-e").expect("-e present");
        assert_eq!(args[position + 1], "COVEN_GIT_TOKEN");
        assert!(
            !args.iter().any(|a| a.contains('=') && a.contains("COVEN_GIT_TOKEN")),
            "no NAME=value form allowed: {args:?}"
        );
    }

    #[test]
    fn container_names_are_unique_per_attempt() {
        assert_ne!(container_name("t1"), container_name("t1"));
    }

    #[test]
    fn exit_137_is_explained_only_for_containers() {
        let containerised = Backend::Container(container());
        assert!(containerised.explains_kill(137).is_some());
        assert!(containerised.explains_kill(1).is_none());
        assert!(Backend::Host.explains_kill(137).is_none());
    }

    #[test]
    fn workspace_view_maps_into_the_container() {
        let host = Path::new("/srv/tasks/t1");
        assert_eq!(Backend::Host.workspace_view(host), host);
        assert_eq!(
            Backend::Container(container()).workspace_view(host),
            Path::new("/workspace")
        );
    }
}
