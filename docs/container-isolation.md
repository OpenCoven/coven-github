# Container Isolation

Self-hosted development can run `coven-code` directly on the host. Hosted OpenCoven should not. A paid GitHub App service needs a stronger worker boundary because tasks clone private repositories and execute project commands.

## Production Target

Each task should run in a fresh container or equivalent sandbox:

- One repository task per container.
- Temporary filesystem mounted at the task workspace.
- Installation token scoped to the target repository only.
- CPU, memory, disk, and wall-clock limits.
- Network egress policy appropriate to the customer's trust tier.
- Workspace deleted after task completion, failure, or timeout.

## Minimum Worker Contract

The host worker should:

1. Receive a persisted task from the queue.
2. Mint or fetch a short-lived installation token.
3. Start an isolated workspace.
4. Inject the session brief and git auth.
5. Run `coven-code --headless --context <brief> --output <result>`.
6. Stream progress to task state and Check Runs.
7. Stop the process when `timeout_secs` expires.
8. Copy out only the result envelope and required logs.
9. Destroy the workspace.

## What Not To Persist

Do not persist:

- Raw installation tokens.
- GitHub App private keys.
- Full repository checkouts.
- Unredacted model provider keys.
- Arbitrary command output without secret filtering.

Persist only task metadata, summaries, exit reasons, changed file lists, branch names, PR links, Check Run links, and explicitly retained logs.

## Self-Hosted Guidance

For self-hosters, local process execution is acceptable for early evaluation if the host already has permission to clone and build the target repositories. Operators should move to containers before letting untrusted repositories or broad organizations trigger tasks.

Recommended baseline:

- Dedicated worker user.
- Dedicated workspace root.
- Short task timeout.
- Low concurrency until resource needs are known.
- Separate GitHub App per environment.
- Private key and webhook secret stored outside the repo.

## Hosted Roadmap

Recommended order:

1. Enforce process timeouts for all local workers. ✅
2. Persist task state before and after each worker phase. ✅ (#2)
3. Add Docker-based worker backend behind the current worker interface. ✅ (#5)
4. Add resource limits and workspace cleanup tests. ✅ (#5)
5. Add a dedicated worker pool tier for paid hosted customers.

## Operator Configuration (issue #5)

The worker backend is configured in `[worker]`:

```toml
[worker]
backend = "container"            # "host" (dev) or "container" (hosted)
# allow_host_backend = true      # explicit opt-in to host execution when
                                 # [[installations]] are configured

[worker.container]
image = "ghcr.io/opencoven/coven-code:latest"
docker_bin = "docker"            # any docker-compatible CLI: podman, nerdctl
cpus = 1.0
memory = "2g"
pids = 512
tmpfs_size = "256m"
network = "bridge"               # "none" for full egress denial
```

Each task attempt runs `docker run --rm` with a fresh, uniquely named
container: read-only rootfs, `--cap-drop ALL`, `no-new-privileges`, a
bounded `/tmp` tmpfs, and only the task workspace bind-mounted. Git
authority is forwarded by environment variable *name* (`-e COVEN_GIT_TOKEN`)
so the token never appears in argv or `docker inspect`. On timeout the
adapter kills both the CLI process and the container by name; `--rm` tears
the container down on every path, and the host workspace is deleted after
success and failure alike.

**Minimum supported backends:** any docker-CLI-compatible runtime (Docker
Engine, Podman, nerdctl/containerd). The `host` backend remains supported
for development and trusted self-hosting; once `[[installations]]` blocks
exist (the hosted posture), `doctor` refuses `host` unless
`worker.allow_host_backend = true` is set deliberately.
