# Architecture Diagrams

This page gives a GitHub-rendered map of `coven-github`: what receives GitHub events, what runs familiar work, where humans watch progress, and where hosted reliability boundaries sit.

## At A Glance

```mermaid
flowchart LR
    maintainer[Maintainer]
    github[GitHub issue, label, mention, or review comment]
    app[coven-github GitHub App]
    webhook[Webhook receiver]
    queue[Task queue and task store]
    worker[Worker]
    runtime[coven-code headless session]
    checks[GitHub Check Run]
    pr[Draft pull request]
    cave[CovenCave oversight]

    maintainer --> github
    github --> app
    app --> webhook
    webhook --> queue
    queue --> worker
    worker --> runtime
    worker --> checks
    runtime --> pr
    checks --> maintainer
    pr --> maintainer
    worker --> cave
    cave --> maintainer
```

`coven-github` is intentionally thin. It owns GitHub ingress, routing, task state, worker orchestration, and status surfaces. The familiar's execution quality lives in `coven-code`, while live oversight and intervention live in CovenCave.

## Component Map

```mermaid
flowchart TB
    subgraph github_side[GitHub]
        events[Webhook events]
        checkruns[Check Runs API]
        comments[Issue and PR comments]
        pulls[Pull requests]
        install[Installation access tokens]
    end

    subgraph adapter[coven-github]
        webhook[crates/webhook<br/>HMAC validation<br/>event parsing<br/>routing]
        config[crates/config<br/>familiar mapping<br/>trigger labels<br/>model route]
        gh[crates/github<br/>installations<br/>checks<br/>PRs<br/>comments]
        worker[crates/worker<br/>session brief<br/>process control<br/>timeouts]
        tasks[task API and store<br/>status<br/>links<br/>audit events]
    end

    subgraph runtime[OpenCoven runtime]
        code[coven-code --headless]
        brief[session-brief.json]
        result[result envelope]
    end

    subgraph oversight[CovenCave]
        board[GitHub task board]
        session[Live session view]
        human[Human steering]
    end

    events --> webhook
    webhook --> config
    webhook --> tasks
    config --> worker
    worker --> brief
    brief --> code
    code --> result
    result --> worker
    worker --> gh
    gh --> install
    gh --> checkruns
    gh --> comments
    gh --> pulls
    worker --> tasks
    tasks --> board
    board --> session
    human --> session
```

## Webhook To Pull Request Sequence

```mermaid
sequenceDiagram
    participant Human as Maintainer
    participant GitHub
    participant Webhook as coven-github webhook
    participant Store as Task queue/store
    participant Worker as coven-github worker
    participant Runtime as coven-code headless
    participant Cave as CovenCave

    Human->>GitHub: Assign issue, add trigger label, or mention familiar
    GitHub->>Webhook: Deliver signed webhook
    Webhook->>Webhook: Validate X-Hub-Signature-256
    Webhook->>Store: Record task and dedupe delivery
    Webhook->>GitHub: Create visible Check Run
    Store->>Worker: Dequeue task
    Worker->>GitHub: Mint installation token
    Worker->>Runtime: Start session with sanitized brief
    Worker->>Cave: Publish task/session status
    Runtime-->>Worker: Progress events and result envelope
    Worker->>GitHub: Update Check Run with status/evidence
    Runtime->>GitHub: Push branch with installation token
    Worker->>GitHub: Open draft PR and link original issue
    Worker->>Cave: Mark task review-ready or failed with evidence
    GitHub-->>Human: PR, checks, and status are visible in GitHub
```

## Task State Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Received
    Received --> Rejected: invalid signature or unsupported event
    Received --> Routed: familiar and trigger matched
    Routed --> Queued: task persisted
    Queued --> Running: worker acquired task
    Running --> NeedsInput: ambiguous request
    Running --> TimedOut: task deadline exceeded
    Running --> Failed: runtime or infra failure
    Running --> ReviewReady: branch pushed and draft PR opened
    NeedsInput --> Queued: maintainer responds or re-triggers
    TimedOut --> Queued: retry allowed
    Failed --> Queued: retry allowed
    ReviewReady --> Completed: maintainer accepts downstream PR
    Rejected --> [*]
    Completed --> [*]
```

This lifecycle keeps GitHub quiet but visible: one task status, one Check Run, and draft PRs by default. No mutation should happen without re-checking live GitHub state first.

## Trust And Data Boundaries

```mermaid
flowchart LR
    subgraph tenant[GitHub installation boundary]
        repo[Installed repositories]
        routing[Familiar routing config]
        memory[Optional familiar memory]
        history[Task history]
    end

    subgraph secrets[Secret boundary]
        appkey[GitHub App private key]
        webhooksecret[Webhook secret]
        token[Short-lived installation token]
        modelkeys[Model credentials]
    end

    subgraph worker[Worker boundary]
        workspace[Ephemeral workspace]
        process[coven-code process]
        logs[Redacted logs and evidence]
    end

    repo --> routing
    routing --> workspace
    appkey --> token
    webhooksecret --> routing
    token --> workspace
    modelkeys --> process
    process --> logs
    logs --> history
    memory --> process
```

Security rules:

- Validate webhook HMAC before parsing JSON.
- Scope all task state by GitHub installation before hosted launch.
- Use installation tokens, not user GitHub credentials.
- Keep worker workspaces per task and clean them up after completion or failure.
- Redact secrets from logs, task APIs, Check Runs, issue comments, and PR bodies.
- Make hosted familiar memory opt-in, inspectable, scoped, and revocable.

## Hosted Vs Self-Hosted Deployment

```mermaid
flowchart TB
    subgraph self[Self-hosted adapter]
        self_app[Operator GitHub App]
        self_server[Operator webhook server]
        self_worker[Operator worker host]
        self_cave[Local CovenCave]
        self_runtime[Local coven-code]
    end

    subgraph hosted[Hosted OpenCoven]
        hosted_app[Managed GitHub App]
        hosted_ingress[Managed webhook ingress]
        hosted_queue[Durable queue and task store]
        hosted_workers[Managed isolated workers]
        hosted_cave[Hosted-ready Cave oversight]
        hosted_audit[Usage, audit, and retention controls]
    end

    self_app --> self_server --> self_worker --> self_runtime --> self_cave
    hosted_app --> hosted_ingress --> hosted_queue --> hosted_workers --> hosted_cave
    hosted_workers --> hosted_audit
```

Self-hosting is the inspectable escape hatch. Hosted OpenCoven monetizes the operational burden: durable queues, task history, worker isolation, familiar memory, usage controls, and support.

## Where To Read Next

- [README](../README.md) for the product overview and quick start.
- [Design](../DESIGN.md) for product constraints and operating pattern.
- [Hosted OpenCoven](../HOSTED.md) for managed service packaging.
- [Security Model](security.md) for credential, token, worker, and tenant boundaries.
- [Self-hosting](self-hosting.md) for operator setup.
