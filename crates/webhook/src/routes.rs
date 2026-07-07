//! Axum route handlers for the webhook endpoint.

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

use coven_github_api::tasks::TaskListStatus;
use coven_github_api::{GitHubEvent, Task, TaskKind};
use coven_github_config::{ApiConfig, ApiMode, Config};
use coven_github_store::{ApiScope, Delivery, Recorded, Routing, Store};

use crate::{
    commands::{parse_mention, Command, MentionKind, COMMAND_LIST},
    events::{parse_event, WebhookPayload},
    verify_signature,
};

/// Shared application state passed to route handlers.
#[derive(Clone)]
pub struct AppState {
    pub config: std::sync::Arc<Config>,
    /// Durable deliveries + task queue (issue #2). Deliveries are recorded —
    /// and deduplicated — here before GitHub is told the webhook succeeded;
    /// the worker claims queued tasks from the same store.
    pub store: Store,
    /// Wake-up signal to the worker pool after a task row is enqueued.
    pub notify: std::sync::Arc<tokio::sync::Notify>,
}

/// Display names by familiar id — the store holds ids; names are config's.
fn familiar_names(config: &Config) -> std::collections::HashMap<String, String> {
    config
        .familiars
        .iter()
        .map(|f| (f.id.clone(), f.display_name.clone()))
        .collect()
}

/// GET /api/github/tasks — task state for CovenCave polling, behind the
/// tenant boundary (issue #3). `token` mode fails closed; a tenant token sees
/// only its own installation (optionally narrowed to repositories); the
/// service token — and `open` mode, for local development — see everything.
/// Every read lands in the audit trail.
pub async fn list_tasks(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let action = "list_tasks";
    let (caller, scope) = match authorize_api(&state.config.api, &headers) {
        ApiCaller::Denied => {
            if let Err(e) = state
                .store
                .record_api_read("anonymous", "none", action, "denied")
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            // Fail closed with a body that reveals nothing about what exists.
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "unauthorized"})),
            )
                .into_response();
        }
        ApiCaller::Open => ("open".to_string(), None),
        ApiCaller::Service => ("service".to_string(), None),
        ApiCaller::Tenant(tenant) => (
            format!("tenant:{}", tenant.installation_id),
            Some(ApiScope {
                installation_id: tenant.installation_id,
                repos: if tenant.repos.is_empty() {
                    None
                } else {
                    Some(tenant.repos.clone())
                },
            }),
        ),
    };
    let scope_label = scope
        .as_ref()
        .map(|s| format!("installation:{}", s.installation_id))
        .unwrap_or_else(|| "all".to_string());

    match state
        .store
        .cave_list(familiar_names(&state.config), scope)
        .await
    {
        Ok(tasks) => {
            if let Err(e) = state
                .store
                .record_api_read(&caller, &scope_label, action, &format!("ok:{}", tasks.len()))
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            Json(json!({ "ok": true, "tasks": tasks })).into_response()
        }
        Err(e) => {
            error!("task list unavailable: {e:#}");
            let _ = state
                .store
                .record_api_read(&caller, &scope_label, action, "error")
                .await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": "task list unavailable"})),
            )
                .into_response()
        }
    }
}

/// GET /api/github/memory — memory activity for the tenant boundary (issue #6),
/// so a customer can inspect what memory a familiar read from and attempted to
/// write to their repositories. Same auth as `list_tasks`: `token` mode fails
/// closed, a tenant sees only its own installation, and every read is audited.
pub async fn list_memory(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let action = "list_memory";
    let (caller, scope) = match authorize_api(&state.config.api, &headers) {
        ApiCaller::Denied => {
            if let Err(e) = state
                .store
                .record_api_read("anonymous", "none", action, "denied")
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "unauthorized"})),
            )
                .into_response();
        }
        ApiCaller::Open => ("open".to_string(), None),
        ApiCaller::Service => ("service".to_string(), None),
        ApiCaller::Tenant(tenant) => (
            format!("tenant:{}", tenant.installation_id),
            Some(ApiScope {
                installation_id: tenant.installation_id,
                repos: if tenant.repos.is_empty() {
                    None
                } else {
                    Some(tenant.repos.clone())
                },
            }),
        ),
    };
    let scope_label = scope
        .as_ref()
        .map(|s| format!("installation:{}", s.installation_id))
        .unwrap_or_else(|| "all".to_string());

    match state.store.list_memory(scope).await {
        Ok(entries) => {
            if let Err(e) = state
                .store
                .record_api_read(&caller, &scope_label, action, &format!("ok:{}", entries.len()))
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            Json(json!({ "ok": true, "memory": entries })).into_response()
        }
        Err(e) => {
            error!("memory list unavailable: {e:#}");
            let _ = state
                .store
                .record_api_read(&caller, &scope_label, action, "error")
                .await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": "memory list unavailable"})),
            )
                .into_response()
        }
    }
}

/// Body of a memory revocation request (issue #6).
#[derive(serde::Deserialize)]
pub struct RevokeRequest {
    /// `owner/name`. Must be within the caller's scope.
    pub repo: String,
    /// The memory key or prefix to revoke.
    pub key: String,
    /// Required for service/open callers; ignored for tenants (taken from the
    /// token so a tenant can only ever revoke its own installation).
    pub installation_id: Option<u64>,
}

/// POST /api/github/memory/revoke — revoke a memory key/prefix for a repo
/// (issue #6). A tenant may only revoke within its own installation (and
/// repositories, when scoped); the operator service token may revoke any
/// installation named in the body. Every revocation is audited.
pub async fn revoke_memory(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RevokeRequest>,
) -> impl IntoResponse {
    let action = "revoke_memory";
    let (caller, installation_id) = match authorize_api(&state.config.api, &headers) {
        ApiCaller::Denied => {
            if let Err(e) = state
                .store
                .record_api_read("anonymous", "none", action, "denied")
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "unauthorized"})),
            )
                .into_response();
        }
        ApiCaller::Tenant(tenant) => {
            // A scoped tenant may only revoke within its own repositories.
            if !tenant.repos.is_empty() && !tenant.repos.contains(&req.repo) {
                let _ = state
                    .store
                    .record_api_read(
                        &format!("tenant:{}", tenant.installation_id),
                        &req.repo,
                        action,
                        "forbidden",
                    )
                    .await;
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"ok": false, "error": "repository not in scope"})),
                )
                    .into_response();
            }
            (
                format!("tenant:{}", tenant.installation_id),
                tenant.installation_id,
            )
        }
        // Operator-wide callers must name the installation explicitly.
        ApiCaller::Service | ApiCaller::Open => match req.installation_id {
            Some(id) => ("service".to_string(), id),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": "installation_id required"})),
                )
                    .into_response()
            }
        },
    };

    match state
        .store
        .record_revocation(installation_id, &req.repo, &req.key)
        .await
    {
        Ok(()) => {
            if let Err(e) = state
                .store
                .record_api_read(&caller, &req.repo, action, "ok")
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            Json(json!({"ok": true, "revoked": req.key})).into_response()
        }
        Err(e) => {
            error!("revocation failed: {e:#}");
            let _ = state
                .store
                .record_api_read(&caller, &req.repo, action, "error")
                .await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": "revocation failed"})),
            )
                .into_response()
        }
    }
}

/// GET /api/github/usage — metering rollup by installation, repository, and
/// familiar (issue #15). Same tenant boundary and audit trail as the task
/// list.
pub async fn usage(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let action = "usage";
    let (caller, scope) = match authorize_api(&state.config.api, &headers) {
        ApiCaller::Denied => {
            if let Err(e) = state
                .store
                .record_api_read("anonymous", "none", action, "denied")
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "unauthorized"})),
            )
                .into_response();
        }
        ApiCaller::Open => ("open".to_string(), None),
        ApiCaller::Service => ("service".to_string(), None),
        ApiCaller::Tenant(tenant) => (
            format!("tenant:{}", tenant.installation_id),
            Some(ApiScope {
                installation_id: tenant.installation_id,
                repos: if tenant.repos.is_empty() {
                    None
                } else {
                    Some(tenant.repos.clone())
                },
            }),
        ),
    };
    let scope_label = scope
        .as_ref()
        .map(|s| format!("installation:{}", s.installation_id))
        .unwrap_or_else(|| "all".to_string());

    match state.store.usage(scope).await {
        Ok(rows) => {
            if let Err(e) = state
                .store
                .record_api_read(&caller, &scope_label, action, &format!("ok:{}", rows.len()))
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            Json(json!({ "ok": true, "usage": rows })).into_response()
        }
        Err(e) => {
            error!("usage rollup unavailable: {e:#}");
            let _ = state
                .store
                .record_api_read(&caller, &scope_label, action, "error")
                .await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": "usage unavailable"})),
            )
                .into_response()
        }
    }
}

/// Number of audit events one dashboard request returns.
const AUDIT_LIMIT: u32 = 200;

/// GET /api/github/audit — task-lifecycle audit stream for the Cave dashboard
/// (issue #18), tenant-scoped and audited, same contract as `list_tasks`.
pub async fn audit(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let action = "audit";
    let (caller, scope) = match authorize_api(&state.config.api, &headers) {
        ApiCaller::Denied => {
            if let Err(e) = state
                .store
                .record_api_read("anonymous", "none", action, "denied")
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "unauthorized"})),
            )
                .into_response();
        }
        ApiCaller::Open => ("open".to_string(), None),
        ApiCaller::Service => ("service".to_string(), None),
        ApiCaller::Tenant(tenant) => (
            format!("tenant:{}", tenant.installation_id),
            Some(ApiScope {
                installation_id: tenant.installation_id,
                repos: if tenant.repos.is_empty() {
                    None
                } else {
                    Some(tenant.repos.clone())
                },
            }),
        ),
    };
    let scope_label = scope
        .as_ref()
        .map(|s| format!("installation:{}", s.installation_id))
        .unwrap_or_else(|| "all".to_string());

    match state.store.audit_events(scope, AUDIT_LIMIT).await {
        Ok(events) => {
            if let Err(e) = state
                .store
                .record_api_read(&caller, &scope_label, action, &format!("ok:{}", events.len()))
                .await
            {
                error!("api audit write failed: {e:#}");
            }
            Json(json!({ "ok": true, "events": events })).into_response()
        }
        Err(e) => {
            error!("audit stream unavailable: {e:#}");
            let _ = state
                .store
                .record_api_read(&caller, &scope_label, action, "error")
                .await;
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": "audit unavailable"})),
            )
                .into_response()
        }
    }
}

/// GET /api/github/routing[?installation_id=N] — the effective routing policy
/// for the Cave dashboard (issue #18). A tenant sees its own installation; the
/// service/open caller names the installation via the query string.
pub async fn routing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let action = "routing";
    let (caller, installation_id) = match authorize_api(&state.config.api, &headers) {
        ApiCaller::Denied => {
            let _ = state
                .store
                .record_api_read("anonymous", "none", action, "denied")
                .await;
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "unauthorized"})),
            )
                .into_response();
        }
        ApiCaller::Tenant(tenant) => (
            format!("tenant:{}", tenant.installation_id),
            tenant.installation_id,
        ),
        ApiCaller::Service | ApiCaller::Open => {
            match params.get("installation_id").and_then(|s| s.parse::<u64>().ok()) {
                Some(id) => ("service".to_string(), id),
                None => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"ok": false, "error": "installation_id required"})),
                    )
                        .into_response()
                }
            }
        }
    };

    let view = state.config.routing_view(installation_id);
    let _ = state
        .store
        .record_api_read(&caller, &installation_id.to_string(), action, "ok")
        .await;
    Json(json!({ "ok": true, "routing": view })).into_response()
}

/// Resolved identity of a task-API caller.
enum ApiCaller<'a> {
    /// `open` mode: unauthenticated local development.
    Open,
    /// Operator-wide service token: unrestricted visibility.
    Service,
    /// Tenant token: one installation's scope.
    Tenant(&'a coven_github_config::TenantToken),
    /// No valid credential in `token` mode. Fail closed.
    Denied,
}

fn authorize_api<'a>(api: &'a ApiConfig, headers: &HeaderMap) -> ApiCaller<'a> {
    if api.mode == ApiMode::Open {
        return ApiCaller::Open;
    }
    let candidate = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let Some(candidate) = candidate else {
        return ApiCaller::Denied;
    };
    if let Some(service) = &api.service_token {
        if token_matches(candidate, service) {
            return ApiCaller::Service;
        }
    }
    for tenant in &api.tenants {
        if token_matches(candidate, &tenant.token) {
            return ApiCaller::Tenant(tenant);
        }
    }
    ApiCaller::Denied
}

/// Constant-time token comparison: comparing fixed-length digests leaks
/// nothing about how many characters of the token matched.
fn token_matches(candidate: &str, expected: &str) -> bool {
    Sha256::digest(candidate.as_bytes()) == Sha256::digest(expected.as_bytes())
}

/// POST /webhook — GitHub webhook receiver.
pub async fn handle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Validate HMAC signature.
    let sig = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            warn!("webhook missing x-hub-signature-256");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing signature"})),
            )
                .into_response();
        }
    };

    if let Err(e) = verify_signature(&state.config.github.webhook_secret, &body, &sig) {
        warn!("webhook signature invalid: {e}");
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid signature"})),
        )
            .into_response();
    }

    // 2. Parse event type and delivery id headers. GitHub sends
    //    X-GitHub-Delivery on every delivery; it is the idempotency key
    //    (issue #2), so a request without one is not a GitHub delivery.
    let event_type = match headers.get("x-github-event").and_then(|v| v.to_str().ok()) {
        Some(e) => e.to_string(),
        None => {
            warn!("webhook missing x-github-event");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing event type"})),
            )
                .into_response();
        }
    };
    let delivery_id = match headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(id) => id.to_string(),
        None => {
            warn!("webhook missing x-github-delivery");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing delivery id"})),
            )
                .into_response();
        }
    };

    // 3. Parse payload.
    let payload: WebhookPayload = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            warn!("webhook payload parse error: {e}");
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "parse error"})),
            )
                .into_response();
        }
    };

    let event = parse_event(&event_type, &payload);
    info!(?event_type, "received webhook event");

    // Delivery coordinates for the durable record (issue #2): enough to
    // answer "was this accepted, and what did it become?" without retaining
    // the payload body itself.
    let delivery = Delivery {
        delivery_id: delivery_id.clone(),
        event: event_type.clone(),
        action: payload.action.clone(),
        installation_id: payload.installation.as_ref().map(|i| i.id),
        repo: payload
            .repository
            .as_ref()
            .map(|r| format!("{}/{}", r.owner.login, r.name)),
        payload_hash: hex::encode(Sha256::digest(&body)),
    };

    // `ping` is GitHub's webhook-configuration handshake — acknowledge it
    // explicitly so operators get a clear signal the endpoint is wired up.
    if matches!(event, GitHubEvent::Ping) {
        info!("webhook ping received — endpoint configured");
        return match state
            .store
            .record_delivery(delivery, Routing::Ignored("ping"))
            .await
        {
            Ok(_) => (StatusCode::OK, Json(json!({"ok": true, "pong": true}))).into_response(),
            Err(e) => storage_unavailable(e),
        };
    }

    // Billing: `marketplace_purchase` deliveries carry plan changes for the
    // purchasing *account*. Record the entitlement before acknowledging, and
    // dedupe by delivery id like every other delivery.
    if event_type == "marketplace_purchase" {
        return handle_marketplace_purchase(&state, delivery, &payload).await;
    }

    // `installation` lifecycle events name the account behind the
    // installation — the join key that resolves a Marketplace purchase to
    // this tenant. Keep the mapping current on every such event.
    if event_type == "installation" && payload.action.as_deref() != Some("deleted") {
        if let Some(inst) = &payload.installation {
            if let Some(account) = &inst.account {
                if let Err(e) = state
                    .store
                    .record_installation_account(inst.id, account.id, &account.login)
                    .await
                {
                    error!("failed to record installation account: {e:#}");
                }
            }
        }
    }

    // Installation removed → purge that tenant's durable memory records
    // (issue #6, delete-on-uninstall). Idempotent via the delivery id.
    if event_type == "installation" && payload.action.as_deref() == Some("deleted") {
        let installation_id = payload.installation.as_ref().map(|i| i.id);
        return match state
            .store
            .record_delivery(delivery, Routing::Ignored("installation_deleted"))
            .await
        {
            Ok(Recorded::New) => {
                if let Some(id) = installation_id {
                    // Purge both memory (issue #6) and task artifacts (issue
                    // #12) for the departing tenant. Idempotent: a redelivery
                    // finds nothing left to remove.
                    let memory = state
                        .store
                        .delete_memory_for_installation(id)
                        .await
                        .unwrap_or_else(|e| {
                            error!("failed to purge memory on uninstall: {e:#}");
                            0
                        });
                    let tasks = state
                        .store
                        .delete_tasks_for_installation(id)
                        .await
                        .unwrap_or_else(|e| {
                            error!("failed to purge task artifacts on uninstall: {e:#}");
                            0
                        });
                    info!(installation_id = id, memory, tasks, "purged tenant data on uninstall");
                    // Forget the account mapping too; the account's plan
                    // survives for its other installations.
                    if let Err(e) = state.store.delete_installation_account(id).await {
                        error!("failed to forget installation account on uninstall: {e:#}");
                    }
                    // Audit what was deleted (issue #12).
                    let _ = state
                        .store
                        .record_api_read(
                            &format!("installation:{id}"),
                            &id.to_string(),
                            "delete_on_uninstall",
                            &format!("memory:{memory},tasks:{tasks}"),
                        )
                        .await;
                }
                (StatusCode::OK, Json(json!({"ok": true}))).into_response()
            }
            Ok(Recorded::Duplicate) => {
                (StatusCode::OK, Json(json!({"ok": true, "duplicate": true}))).into_response()
            }
            Err(e) => storage_unavailable(e),
        };
    }

    // 4. Route event → task, then persist BEFORE acknowledging: GitHub must
    //    only hear success once durable state exists, and a redelivered
    //    delivery id must never dispatch twice (issue #2).
    match event_to_task(&state, event).await {
        Some(task) => {
            // Entitlement resolution: explicit TOML limits win per field;
            // otherwise a purchased plan supplies its tier defaults
            // (docs/pricing.md). Neither = unlimited (self-hosted default).
            let plan = match state.store.plan_for_installation(task.installation_id).await {
                Ok(plan) => plan,
                Err(e) => return storage_unavailable(e),
            };
            let entitled = plan.as_ref().filter(|p| p.entitled());

            // The hosted monetization gate: with `billing.require_plan`, an
            // installation must hold an entitled plan or an explicit
            // [[installations]] entry (operator-vouched, e.g. Dedicated).
            if state.config.billing.require_plan
                && entitled.is_none()
                && !state.config.installation_listed(task.installation_id)
            {
                warn!(
                    installation_id = task.installation_id,
                    "no entitled plan — delivery ignored"
                );
                if let Err(e) = state
                    .store
                    .record_delivery(delivery, Routing::Ignored("no_plan"))
                    .await
                {
                    return storage_unavailable(e);
                }
                return (
                    StatusCode::OK,
                    Json(json!({"ok": true, "ignored": "no_plan"})),
                )
                    .into_response();
            }

            let tier = entitled.map(|p| {
                let tier: coven_github_config::PlanTier =
                    p.tier.parse().unwrap_or(coven_github_config::PlanTier::Unknown);
                if tier == coven_github_config::PlanTier::Unknown {
                    warn!(
                        installation_id = task.installation_id,
                        plan = %p.plan_name,
                        "unclassified plan — applying Starter limits"
                    );
                }
                tier
            });
            let limits = coven_github_config::effective_limits(
                state.config.limits_for(task.installation_id),
                tier,
            );

            // Daily task cap (issue #15): over-quota installations get their
            // delivery recorded as ignored — visible in the audit trail — and
            // GitHub still hears 200 (the delivery itself succeeded).
            if let Some(cap) = limits.max_tasks_per_day
            {
                let cutoff = (chrono::Utc::now() - chrono::Duration::hours(24)).to_rfc3339();
                match state
                    .store
                    .tasks_created_since(task.installation_id, &cutoff)
                    .await
                {
                    Ok(used) if used >= cap => {
                        warn!(
                            installation_id = task.installation_id,
                            used, cap, "daily task cap reached — delivery ignored"
                        );
                        if let Err(e) = state
                            .store
                            .record_delivery(delivery, Routing::Ignored("quota_exceeded"))
                            .await
                        {
                            return storage_unavailable(e);
                        }
                        return (
                            StatusCode::OK,
                            Json(json!({"ok": true, "ignored": "quota_exceeded"})),
                        )
                            .into_response();
                    }
                    Ok(_) => {}
                    Err(e) => return storage_unavailable(e),
                }
            }
            match state
                .store
                .record_delivery(delivery, Routing::Task(&task))
                .await
            {
                Ok(Recorded::New) => {}
                Ok(Recorded::Duplicate) => {
                    info!(delivery_id, "duplicate delivery — already routed, nothing dispatched");
                    return (
                        StatusCode::OK,
                        Json(json!({"ok": true, "duplicate": true})),
                    )
                        .into_response();
                }
                Err(e) => return storage_unavailable(e),
            }
            // The tasks table IS the queue: enqueueing is the durable insert
            // above (with supersession tombstones applied in-transaction);
            // this only wakes the claim loop. No channel, no drop path.
            info!(task_id = %task.id, "task enqueued");
            state.notify.notify_one();
        }
        None => {
            if let Err(e) = state
                .store
                .record_delivery(delivery, Routing::Ignored("unroutable"))
                .await
            {
                return storage_unavailable(e);
            }
        }
    }

    (StatusCode::OK, Json(json!({"ok": true}))).into_response()
}

/// Handles a `marketplace_purchase` delivery: record the purchasing
/// account's plan (billing entitlement), audit the transition, and
/// acknowledge. Purchases key on the account; installations resolve to it
/// through the mapping kept by `installation` events.
async fn handle_marketplace_purchase(
    state: &AppState,
    delivery: Delivery,
    payload: &WebhookPayload,
) -> axum::response::Response {
    let action = payload.action.as_deref().unwrap_or("");
    let reason = format!("marketplace_purchase:{action}");

    // Dedupe first: only a first-seen delivery may change plan state.
    match state
        .store
        .record_delivery(delivery, Routing::Ignored(&reason))
        .await
    {
        Ok(Recorded::New) => {}
        Ok(Recorded::Duplicate) => {
            return (StatusCode::OK, Json(json!({"ok": true, "duplicate": true})))
                .into_response();
        }
        Err(e) => return storage_unavailable(e),
    }

    let Some(purchase) = &payload.marketplace_purchase else {
        warn!("marketplace_purchase delivery without a purchase object");
        return (StatusCode::OK, Json(json!({"ok": true, "ignored": reason})))
            .into_response();
    };

    // Pending changes apply at the end of the billing cycle; GitHub sends a
    // concrete `changed` event when they take effect. Nothing to do yet.
    let plan_state = match action {
        "purchased" | "changed" if purchase.on_free_trial => "trial",
        "purchased" | "changed" => "active",
        "cancelled" => "cancelled",
        _ => {
            info!(action, "marketplace_purchase acknowledged without state change");
            return (StatusCode::OK, Json(json!({"ok": true, "ignored": reason})))
                .into_response();
        }
    };

    let plan_name = purchase
        .plan
        .as_ref()
        .map(|p| p.name.clone())
        .unwrap_or_default();
    let tier = coven_github_config::PlanTier::parse(&plan_name);
    let record = coven_github_store::AccountPlan {
        account_id: purchase.account.id,
        account_login: purchase.account.login.clone(),
        plan_name: plan_name.clone(),
        tier: tier.as_str().to_string(),
        state: plan_state.to_string(),
        source: "marketplace".to_string(),
        updated_at: String::new(),
    };
    if let Err(e) = state.store.set_account_plan(record).await {
        return storage_unavailable(e);
    }
    info!(
        account = %purchase.account.login,
        plan = %plan_name,
        tier = tier.as_str(),
        state = plan_state,
        "marketplace plan recorded"
    );
    // Every plan transition lands in the audit trail (issue #12).
    if let Err(e) = state
        .store
        .record_api_read(
            "billing:marketplace",
            &format!("account:{}", purchase.account.id),
            action,
            &format!("tier:{},state:{plan_state}", tier.as_str()),
        )
        .await
    {
        error!("billing audit write failed: {e:#}");
    }

    (StatusCode::OK, Json(json!({"ok": true, "plan": tier.as_str(), "state": plan_state})))
        .into_response()
}

/// The durable store could not record the delivery: answer 5xx so GitHub
/// retries later instead of treating unheld work as delivered (issue #2).
fn storage_unavailable(e: anyhow::Error) -> axum::response::Response {
    error!("durable store unavailable: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "storage unavailable"})),
    )
        .into_response()
}

/// Maps a parsed event to a worker task, or returns None if not actionable.
/// Routing is installation-scoped (issue #7): the delivery's installation id
/// and repository resolve a [`coven_github_config::RoutingScope`] first, and
/// only that scope's familiars and open trigger lanes can match.
async fn event_to_task(state: &AppState, event: GitHubEvent) -> Option<Task> {
    let scope_of = |installation_id: u64, owner: &str, name: &str| {
        state
            .config
            .scope_for(installation_id, &format!("{owner}/{name}"))
    };
    match event {
        GitHubEvent::IssueAssigned(e) => {
            let scope = scope_of(e.installation_id, &e.repo_owner, &e.repo_name);
            if !scope.assignment_enabled() {
                return None;
            }
            // Find a familiar whose bot_username matches the assignee.
            let familiar = scope.familiar_by_bot(&e.assignee_login)?;

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                commander: None,
                kind: TaskKind::FixIssue {
                    issue_number: e.issue_number,
                    issue_title: e.issue_title,
                    issue_body: e.issue_body,
                },
            })
        }

        GitHubEvent::IssueLabeled(e) => {
            let scope = scope_of(e.installation_id, &e.repo_owner, &e.repo_name);
            if !scope.labels_enabled() {
                return None;
            }
            let familiar = scope.familiar_by_label(&e.label_name)?;

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                commander: None,
                kind: TaskKind::FixIssue {
                    issue_number: e.issue_number,
                    issue_title: e.issue_title,
                    issue_body: e.issue_body,
                },
            })
        }

        // Comment surfaces speak the maintainer command protocol (issue #13):
        // only command-position mentions act; casual mentions are ignored.
        GitHubEvent::IssueComment(e) => {
            let scope = scope_of(e.installation_id, &e.repo_owner, &e.repo_name);
            if !scope.commands_enabled() {
                return None;
            }
            let (familiar, command) = parse_command(&scope, &e.comment_body, &e.commenter_login)?;
            let surface = CommandSurface {
                installation_id: e.installation_id,
                repo_owner: &e.repo_owner,
                repo_name: &e.repo_name,
                number: e.issue_number,
                title: &e.issue_title,
                issue_body: &e.issue_body,
                comment_body: &e.comment_body,
                on_pull_request: e.on_pull_request,
                commander: &e.commenter_login,
            };
            command_task(state, familiar, command, surface).await
        }

        GitHubEvent::PullRequestReview(e) => {
            let scope = scope_of(e.installation_id, &e.repo_owner, &e.repo_name);
            if !scope.commands_enabled() {
                return None;
            }
            let (familiar, command) = parse_command(&scope, &e.review_body, &e.reviewer_login)?;
            let surface = CommandSurface {
                installation_id: e.installation_id,
                repo_owner: &e.repo_owner,
                repo_name: &e.repo_name,
                number: e.pr_number,
                title: &e.pr_title,
                issue_body: "",
                comment_body: &e.review_body,
                on_pull_request: true,
                commander: &e.reviewer_login,
            };
            command_task(state, familiar, command, surface).await
        }

        GitHubEvent::PullRequestReviewComment(e) => {
            let scope = scope_of(e.installation_id, &e.repo_owner, &e.repo_name);
            if !scope.commands_enabled() {
                return None;
            }
            let (familiar, command) = parse_command(&scope, &e.comment_body, &e.commenter_login)?;
            let surface = CommandSurface {
                installation_id: e.installation_id,
                repo_owner: &e.repo_owner,
                repo_name: &e.repo_name,
                number: e.pr_number,
                title: &e.pr_title,
                issue_body: "",
                comment_body: &e.comment_body,
                on_pull_request: true,
                commander: &e.commenter_login,
            };
            command_task(state, familiar, command, surface).await
        }

        GitHubEvent::PullRequestChanged(e) => {
            // Loop prevention: never auto-review PRs authored by our own
            // familiars — the adapter's draft PRs would otherwise re-trigger.
            // Checked against the global list on purpose: scope filtering
            // must never re-enable a self-trigger loop.
            if state
                .config
                .familiars
                .iter()
                .any(|f| f.bot_username == e.author_login)
            {
                return None;
            }
            let scope = scope_of(e.installation_id, &e.repo_owner, &e.repo_name);
            let repo = format!("{}/{}", e.repo_owner, e.repo_name);
            let policy = &state.config.review;
            let familiar = if e.action == "labeled" {
                // A review label is an explicit per-PR opt-in: it works even
                // when the automatic lane is off, and on drafts — the same
                // contract as issue trigger labels (the `labels` lane).
                if !scope.labels_enabled() {
                    return None;
                }
                let label = e.label_name.as_deref()?;
                scope.familiar_by_label(label)?
            } else {
                if !scope.reviews_enabled() {
                    return None;
                }
                if !policy.pull_request_enabled(&repo) {
                    return None;
                }
                if e.draft && !policy.drafts_included(&repo) {
                    return None;
                }
                let reviewer = policy.reviewer(&repo)?;
                scope.familiar_by_id(reviewer)?
            };

            Some(Task {
                id: uuid::Uuid::new_v4().to_string(),
                installation_id: e.installation_id,
                repo_owner: e.repo_owner,
                repo_name: e.repo_name,
                familiar_id: familiar.id.clone(),
                commander: None,
                kind: TaskKind::ReviewPullRequest {
                    pr_number: e.pr_number,
                    pr_title: e.pr_title,
                    reason: e.action,
                },
            })
        }

        // Push review needs a PR-less task kind — a contract v3 lane. The
        // event is parsed and typed so fixtures and policy land now; routing
        // activates with the v3 brief (issue #10).
        GitHubEvent::Push(_) => None,

        // `ping` is acknowledged at the handler; it never produces a task.
        GitHubEvent::Ping | GitHubEvent::Unsupported { .. } => None,
    }
}

/// Finds the first scoped familiar addressed in command position, skipping
/// the bots' own comments (self-trigger loop guard).
fn parse_command<'a>(
    scope: &coven_github_config::RoutingScope<'a>,
    body: &str,
    author: &str,
) -> Option<(&'a coven_github_config::FamiliarConfig, Command)> {
    if slash_garden_command(body) {
        return scope
            .familiars()
            .find(|f| author != f.bot_username)
            .map(|f| (f, Command::Garden));
    }

    scope.familiars().find_map(|f| {
        if author == f.bot_username {
            return None;
        }
        match parse_mention(body, &f.bot_username) {
            MentionKind::Command(command) => Some((f, command)),
            MentionKind::Casual | MentionKind::None => None,
        }
    })
}

fn slash_garden_command(body: &str) -> bool {
    let mut words = body.split_whitespace();
    matches!(words.next(), Some("/coven")) && matches!(words.next(), Some("garden"))
}

/// The issue/PR conversation a command arrived on.
struct CommandSurface<'a> {
    installation_id: u64,
    repo_owner: &'a str,
    repo_name: &'a str,
    number: u64,
    title: &'a str,
    issue_body: &'a str,
    comment_body: &'a str,
    on_pull_request: bool,
    commander: &'a str,
}

/// Maps a typed maintainer command to a task. Work commands and gated
/// acknowledgements (cancel, remember/forget) carry the commander for the
/// worker's permission gate; clarifications and status replies carry none.
async fn command_task(
    state: &AppState,
    familiar: &coven_github_config::FamiliarConfig,
    command: Command,
    s: CommandSurface<'_>,
) -> Option<Task> {
    let repo = format!("{}/{}", s.repo_owner, s.repo_name);
    let make = |kind: TaskKind, commander: Option<String>| Task {
        id: uuid::Uuid::new_v4().to_string(),
        installation_id: s.installation_id,
        repo_owner: s.repo_owner.to_string(),
        repo_name: s.repo_name.to_string(),
        familiar_id: familiar.id.clone(),
        commander,
        kind,
    };
    let commander = Some(s.commander.to_string());
    let reply = |body: String| {
        make(
            TaskKind::CommandReply {
                issue_number: s.number,
                body,
            },
            None,
        )
    };

    Some(match command {
        Command::Review | Command::Deepen | Command::Retry if s.on_pull_request => make(
            TaskKind::ReviewPullRequest {
                pr_number: s.number,
                pr_title: s.title.to_string(),
                reason: format!("command:{}", verb(&command)),
            },
            commander,
        ),
        // `retry` on an issue re-runs the fix lane.
        Command::Retry => make(
            TaskKind::FixIssue {
                issue_number: s.number,
                issue_title: s.title.to_string(),
                issue_body: s.issue_body.to_string(),
            },
            commander,
        ),
        Command::Review | Command::Deepen => reply(format!(
            "`{}` needs a pull request; this is an issue. Commands here: {COMMAND_LIST}.",
            verb(&command)
        )),
        Command::Fix { .. } if s.on_pull_request => make(
            TaskKind::AddressReviewComment {
                pr_number: s.number,
                comment_body: s.comment_body.to_string(),
                diff_hunk: None,
            },
            commander,
        ),
        Command::Fix { .. } => make(
            TaskKind::FixIssue {
                issue_number: s.number,
                issue_title: s.title.to_string(),
                issue_body: s.issue_body.to_string(),
            },
            commander,
        ),
        // Cancellation mutates queued work, so it rides a gated adapter task:
        // the worker verifies the commander's write access before tombstoning
        // anything (issue #13). In-flight work is not interrupted (documented
        // limitation); the next review command or PR event re-arms the lane.
        Command::Cancel if s.on_pull_request => make(
            TaskKind::CancelReviews {
                pr_number: s.number,
            },
            commander,
        ),
        Command::Cancel => reply("`cancel` currently applies to queued pull-request reviews only.".to_string()),
        Command::Garden => make(
            TaskKind::GardenRun {
                report_issue: Some(s.number),
            },
            commander,
        ),
        // Memory acknowledgements are gated too: only maintainers should hear
        // how the familiar handles memory intents.
        Command::Remember { .. } | Command::Forget { .. } => make(
            TaskKind::CommandReply {
                issue_number: s.number,
                body: "Noted, but memory persistence is not wired up yet — it lands with the \
                       hosted memory governance contract (#6). Nothing was stored or deleted."
                    .to_string(),
            },
            commander,
        ),
        Command::Status => {
            let items = state
                .store
                .cave_list(familiar_names(&state.config), None)
                .await
                .unwrap_or_else(|e| {
                    warn!("status could not reach the store: {e:#}");
                    Vec::new()
                });
            let mut lines: Vec<String> = items
                .iter()
                .filter(|t| t.repo == repo && t.issue_number == Some(s.number))
                .map(|t| format!("- {} — {}", t.issue_title, status_label(&t.status)))
                .collect();
            let body = if lines.is_empty() {
                format!("No tracked tasks for {repo}#{}.", s.number)
            } else {
                lines.sort();
                format!("Tasks for {repo}#{}:\n{}", s.number, lines.join("\n"))
            };
            reply(body)
        }
        Command::Unknown { verb } => reply(format!(
            "I don't recognize `{verb}`. Supported commands: {COMMAND_LIST}."
        )),
    })
}

fn verb(command: &Command) -> &'static str {
    match command {
        Command::Review => "review",
        Command::Fix { .. } => "fix",
        Command::Deepen => "deepen",
        Command::Retry => "retry",
        Command::Cancel => "cancel",
        Command::Garden => "garden",
        Command::Remember { .. } => "remember",
        Command::Forget { .. } => "forget",
        Command::Status => "status",
        Command::Unknown { .. } => "unknown",
    }
}

fn status_label(status: &TaskListStatus) -> &'static str {
    match status {
        TaskListStatus::Queued => "queued",
        TaskListStatus::Running => "running",
        TaskListStatus::Review => "awaiting review",
        TaskListStatus::Done => "done",
        TaskListStatus::Failed => "failed",
        TaskListStatus::Superseded => "superseded",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::{IssueCommentEvent, IssueLabeledEvent, TaskKind};
    use coven_github_config::{FamiliarConfig, GitHubAppConfig, ServerConfig, WorkerConfig};
    use std::{path::PathBuf, sync::Arc};

    pub(crate) fn app_state() -> AppState {
        app_state_with_review(coven_github_config::ReviewConfig::default())
    }

    pub(crate) fn app_state_with_review(review: coven_github_config::ReviewConfig) -> AppState {
        AppState {
            config: Arc::new(Config {
                server: ServerConfig {
                    bind: "127.0.0.1:0".to_string(),
                    cave_base_url: None,
                },
                github: GitHubAppConfig {
                    app_id: 1,
                    private_key_path: PathBuf::from("private.pem"),
                    webhook_secret: "secret".to_string(),
                    api_base_url: None,
                },
                worker: WorkerConfig {
                    concurrency: 1,
                    coven_code_bin: PathBuf::from("coven-code"),
                    workspace_root: PathBuf::from("/tmp/coven-github-test"),
                    timeout_secs: 60,
                    max_retries: 0,
                backend: coven_github_config::WorkerBackendKind::Host,
                container: coven_github_config::ContainerConfig::default(),
                allow_host_backend: false,
                },
                familiars: vec![FamiliarConfig {
                    id: "cody".to_string(),
                    display_name: "Cody".to_string(),
                    bot_username: "coven-cody[bot]".to_string(),
                    model: None,
                    skills: vec![],
                    trigger_labels: vec!["coven:fix".to_string(), "coven:review".to_string()],
                }],
                review,
                storage: coven_github_config::StorageConfig::default(),
                memory: coven_github_config::MemoryConfig::default(),
                gardener: coven_github_config::GardenerConfig::default(),
                api: coven_github_config::ApiConfig::default(),
                installations: vec![],
                billing: Default::default(),
            }),
            store: Store::open_in_memory().expect("in-memory store"),
            notify: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Enqueues a durable task row the way the webhook path would.
    pub(crate) async fn seed_task(state: &AppState, delivery_id: &str, task: &Task) {
        state
            .store
            .record_delivery(
                coven_github_store::Delivery {
                    delivery_id: delivery_id.to_string(),
                    event: "test".to_string(),
                    action: None,
                    installation_id: Some(task.installation_id),
                    repo: Some(format!("{}/{}", task.repo_owner, task.repo_name)),
                    payload_hash: "h".to_string(),
                },
                coven_github_store::Routing::Task(task),
            )
            .await
            .expect("seed task");
    }

    #[tokio::test]
    async fn labeled_issue_routes_to_familiar_trigger_label() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueLabeled(IssueLabeledEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Token refresh is broken.".to_string(),
                label_name: "coven:fix".to_string(),
            }),
        )
        .await
        .expect("matching trigger label should create a task");

        assert_eq!(task.installation_id, 123);
        assert_eq!(task.repo_owner, "OpenCoven");
        assert_eq!(task.repo_name, "coven-code");
        assert_eq!(task.familiar_id, "cody");
        match task.kind {
            TaskKind::FixIssue {
                issue_number,
                issue_title,
                issue_body,
            } => {
                assert_eq!(issue_number, 42);
                assert_eq!(issue_title, "Fix auth");
                assert_eq!(issue_body, "Token refresh is broken.");
            }
            other => panic!("expected FixIssue task, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn labeled_issue_ignores_unknown_labels() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueLabeled(IssueLabeledEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Token refresh is broken.".to_string(),
                label_name: "help wanted".to_string(),
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn issue_comment_ignores_bot_self_mentions() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "@coven-cody thanks, I opened a PR.".to_string(),
                commenter_login: "coven-cody[bot]".to_string(),
                on_pull_request: false,
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn pr_review_comment_ignores_bot_self_mentions() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReviewComment(coven_github_api::PrReviewCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 7,
                pr_title: "Harden sigil parser".to_string(),
                comment_body: "@coven-cody fix: please".to_string(),
                commenter_login: "coven-cody[bot]".to_string(),
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn issue_comment_on_pr_routes_to_pr_iteration() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 73,
                issue_title: "Add spell compiler cache".to_string(),
                issue_body: "".to_string(),
                comment_body: "@coven-cody fix: the lint is still failing".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: true,
            }),
        )
        .await
        .expect("a mention on a PR comment should create a task");

        match task.kind {
            TaskKind::AddressReviewComment {
                pr_number,
                comment_body,
                diff_hunk,
            } => {
                assert_eq!(pr_number, 73);
                assert!(comment_body.contains("lint is still failing"));
                assert!(diff_hunk.is_none());
            }
            other => panic!("expected AddressReviewComment for a PR comment, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fix_command_on_issue_routes_to_fix_issue_with_commander() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Token refresh is broken.".to_string(),
                comment_body: "@coven-cody fix".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("a fix command on an issue should create a task");

        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::FixIssue {
                issue_number,
                issue_title,
                ..
            } => {
                assert_eq!(issue_number, 42);
                assert_eq!(issue_title, "Fix auth");
            }
            other => panic!("expected FixIssue for a fix command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn garden_command_on_issue_routes_to_garden_run_with_report_surface() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "@coven-cody garden".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("a garden command on an issue should create a task");

        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::GardenRun { report_issue } => assert_eq!(report_issue, Some(42)),
            other => panic!("expected GardenRun for a garden command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn slash_coven_garden_routes_to_garden_run_with_first_available_familiar() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "/coven garden".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("slash garden command should create a task");

        assert_eq!(task.familiar_id, "cody");
        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::GardenRun { report_issue } => assert_eq!(report_issue, Some(42)),
            other => panic!("expected GardenRun for slash garden command, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn garden_command_on_pr_conversation_comment_reports_on_pr_number() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 73,
                issue_title: "Add spell compiler cache".to_string(),
                issue_body: "".to_string(),
                comment_body: "@coven-cody garden".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: true,
            }),
        )
        .await
        .expect("a garden command on a PR conversation should create a task");

        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::GardenRun { report_issue } => assert_eq!(report_issue, Some(73)),
            other => panic!("expected GardenRun for a PR conversation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn casual_mention_creates_no_task() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "thanks @coven-cody, great work".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await;

        assert!(task.is_none(), "casual mentions must not trigger work");
    }

    #[tokio::test]
    async fn unknown_command_earns_a_clarification_reply() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "@coven-cody can you take a look?".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("unknown command should earn a clarification");

        assert!(task.commander.is_none(), "replies need no permission gate");
        match task.kind {
            TaskKind::CommandReply { issue_number, body } => {
                assert_eq!(issue_number, 42);
                assert!(body.contains("`can`"));
                assert!(body.contains("`review`"), "reply should list commands: {body}");
            }
            other => panic!("expected CommandReply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submitted_review_mention_routes_to_pr_iteration() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReview(coven_github_api::PrReviewEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 73,
                pr_title: "Harden sigil parser".to_string(),
                review_body: "@coven-cody fix: add test coverage before merge.".to_string(),
                review_state: "changes_requested".to_string(),
                reviewer_login: "octocat".to_string(),
            }),
        )
        .await
        .expect("a mention in a review body should create a task");

        match task.kind {
            TaskKind::AddressReviewComment { pr_number, .. } => assert_eq!(pr_number, 73),
            other => panic!("expected AddressReviewComment for a review, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn submitted_review_without_mention_is_ignored() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::PullRequestReview(coven_github_api::PrReviewEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                pr_number: 73,
                pr_title: "Harden sigil parser".to_string(),
                review_body: "LGTM, nice work!".to_string(),
                review_state: "approved".to_string(),
                reviewer_login: "octocat".to_string(),
            }),
        )
        .await;

        assert!(task.is_none());
    }

    #[tokio::test]
    async fn ping_event_produces_no_task() {
        let state = app_state();
        assert!(event_to_task(&state, GitHubEvent::Ping).await.is_none());
    }

}

#[cfg(test)]
mod review_lane_tests {
    use super::tests::{app_state, app_state_with_review};
    use super::*;
    use coven_github_api::PrChangedEvent;
    use coven_github_config::ReviewConfig;

    fn review_on() -> ReviewConfig {
        ReviewConfig {
            familiar: Some("cody".to_string()),
            pull_request: true,
            include_drafts: false,
            audit_instruction: None,
            min_severity: None,
            publish: None,
            repos: std::collections::HashMap::new(),
        }
    }

    fn pr_event(action: &str) -> PrChangedEvent {
        PrChangedEvent {
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            pr_number: 88,
            pr_title: "Add spell compiler cache".to_string(),
            action: action.to_string(),
            label_name: None,
            head_ref: "feat/spell-cache".to_string(),
            head_sha: "abc123".to_string(),
            base_ref: "main".to_string(),
            author_login: "octocat".to_string(),
            draft: false,
        }
    }

    #[tokio::test]
    async fn pr_opened_routes_to_review_when_lane_enabled() {
        let state = app_state_with_review(review_on());
        let task = event_to_task(&state, GitHubEvent::PullRequestChanged(pr_event("opened")))
            .await
            .expect("enabled lane should create a review task");

        assert_eq!(task.familiar_id, "cody");
        match task.kind {
            TaskKind::ReviewPullRequest {
                pr_number, reason, ..
            } => {
                assert_eq!(pr_number, 88);
                assert_eq!(reason, "opened");
            }
            other => panic!("expected ReviewPullRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pr_synchronize_carries_event_time_head() {
        let state = app_state_with_review(review_on());
        let mut event = pr_event("synchronize");
        event.head_sha = "f00dface".to_string();
        let task = event_to_task(&state, GitHubEvent::PullRequestChanged(event))
            .await
            .expect("synchronize should create a review task");

        match task.kind {
            TaskKind::ReviewPullRequest { reason, .. } => {
                assert_eq!(reason, "synchronize");
            }
            other => panic!("expected ReviewPullRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pr_opened_is_ignored_when_lane_disabled() {
        let state = app_state();
        assert!(
            event_to_task(&state, GitHubEvent::PullRequestChanged(pr_event("opened"))).await.is_none()
        );
    }

    #[tokio::test]
    async fn familiar_authored_prs_are_never_auto_reviewed() {
        // The adapter's own draft PRs must not re-trigger reviews (loop guard).
        let state = app_state_with_review(review_on());
        let mut event = pr_event("opened");
        event.author_login = "coven-cody[bot]".to_string();
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event)).await.is_none());
    }

    #[tokio::test]
    async fn draft_prs_are_skipped_unless_policy_includes_them() {
        let state = app_state_with_review(review_on());
        let mut event = pr_event("opened");
        event.draft = true;
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event.clone())).await.is_none());

        let mut inclusive = review_on();
        inclusive.include_drafts = true;
        let state = app_state_with_review(inclusive);
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event)).await.is_some());
    }

    #[tokio::test]
    async fn per_repo_override_disables_the_lane() {
        let mut review = review_on();
        review.repos.insert(
            "OpenCoven/coven-code".to_string(),
            coven_github_config::RepoReviewOverride {
                pull_request: Some(false),
                include_drafts: None,
                familiar: None,
                audit_instruction: None,
                min_severity: None,
                publish: None,
            },
        );
        let state = app_state_with_review(review);
        assert!(
            event_to_task(&state, GitHubEvent::PullRequestChanged(pr_event("opened"))).await.is_none()
        );
    }

    #[tokio::test]
    async fn review_label_is_an_explicit_opt_in_even_with_lane_off_and_draft() {
        // No [review] policy at all — the label alone routes, like issue labels.
        let state = app_state();
        let mut event = pr_event("labeled");
        event.label_name = Some("coven:review".to_string());
        event.draft = true;
        let task = event_to_task(&state, GitHubEvent::PullRequestChanged(event))
            .await
            .expect("review label should opt the PR in");
        assert_eq!(task.familiar_id, "cody");
        assert!(matches!(task.kind, TaskKind::ReviewPullRequest { .. }));
    }

    #[tokio::test]
    async fn unknown_labels_do_not_trigger_review() {
        let state = app_state_with_review(review_on());
        let mut event = pr_event("labeled");
        event.label_name = Some("help wanted".to_string());
        assert!(event_to_task(&state, GitHubEvent::PullRequestChanged(event)).await.is_none());
    }

    #[tokio::test]
    async fn push_events_produce_no_task_until_contract_v3() {
        let state = app_state_with_review(review_on());
        let event = GitHubEvent::Push(coven_github_api::PushEvent {
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            branch: Some("main".to_string()),
            before_sha: "aaa".to_string(),
            after_sha: "bbb".to_string(),
            deleted: false,
            forced: false,
            pusher_login: "octocat".to_string(),
            commit_count: 2,
        });
        assert!(event_to_task(&state, event).await.is_none());
    }
}

#[cfg(test)]
mod command_routing_tests {
    use super::tests::app_state;
    use super::*;
    use coven_github_api::PrReviewCommentEvent;

    fn pr_comment(body: &str) -> GitHubEvent {
        GitHubEvent::PullRequestReviewComment(PrReviewCommentEvent {
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            pr_number: 88,
            pr_title: "Add spell compiler cache".to_string(),
            comment_body: body.to_string(),
            commenter_login: "octocat".to_string(),
        })
    }

    #[tokio::test]
    async fn review_command_on_pr_creates_commanded_review() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody review"))
            .await
            .expect("review command should create a task");

        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::ReviewPullRequest {
                pr_number, reason, ..
            } => {
                assert_eq!(pr_number, 88);
                assert_eq!(reason, "command:review");
            }
            other => panic!("expected ReviewPullRequest, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn garden_command_on_pr_review_comment_routes_to_garden_run_with_pr_surface() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody garden"))
            .await
            .expect("garden command should create a task");

        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::GardenRun { report_issue } => assert_eq!(report_issue, Some(88)),
            other => panic!("expected GardenRun, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deepen_command_carries_its_verb_in_the_reason() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody deepen"))
            .await
            .expect("deepen command should create a task");
        assert!(
            matches!(task.kind, TaskKind::ReviewPullRequest { ref reason, .. } if reason == "command:deepen")
        );
    }

    #[tokio::test]
    async fn cancel_command_rides_a_gated_task_not_a_route_side_mutation() {
        let state = app_state();
        let queued = Task {
            id: "task-queued".to_string(),
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "t".to_string(),
                reason: "opened".to_string(),
            },
        };
        super::tests::seed_task(&state, "dl-cancel", &queued).await;

        let task = event_to_task(&state, pr_comment("@coven-cody cancel"))
            .await
            .expect("cancel should route");

        // The route must NOT tombstone anything — the worker does, after the
        // commander's write access is verified (issue #13).
        let states = state.store.task_states().await.expect("states");
        assert_eq!(
            states,
            vec![("task-queued".to_string(), "queued".to_string())],
            "queued review must be untouched until the gate passes"
        );
        match task.kind {
            TaskKind::CancelReviews { pr_number } => assert_eq!(pr_number, 88),
            other => panic!("expected CancelReviews, got {other:?}"),
        }
        assert_eq!(task.commander.as_deref(), Some("octocat"));
    }

    #[tokio::test]
    async fn memory_commands_are_acknowledged_but_deferred() {
        let state = app_state();
        let task = event_to_task(&state, pr_comment("@coven-cody remember we ship Fridays"))
            .await
            .expect("remember should acknowledge");
        // The acknowledgement is gated: it carries the commander so the
        // worker verifies write access before replying (issue #13).
        assert_eq!(task.commander.as_deref(), Some("octocat"));
        match task.kind {
            TaskKind::CommandReply { body, .. } => {
                assert!(body.contains("#6"));
                assert!(body.contains("Nothing was stored"));
            }
            other => panic!("expected CommandReply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn status_command_reports_tracked_tasks_for_the_surface() {
        let state = app_state();
        let tracked = Task {
            id: "task-1".to_string(),
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "Add spell compiler cache".to_string(),
                reason: "opened".to_string(),
            },
        };
        super::tests::seed_task(&state, "dl-status", &tracked).await;

        let task = event_to_task(&state, pr_comment("@coven-cody status"))
            .await
            .expect("status should reply");
        match task.kind {
            TaskKind::CommandReply { body, .. } => {
                assert!(body.contains("Review PR #88"), "status body: {body}");
                assert!(body.contains("queued"), "status body: {body}");
            }
            other => panic!("expected CommandReply, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn review_command_on_an_issue_is_clarified() {
        let state = app_state();
        let task = event_to_task(
            &state,
            GitHubEvent::IssueComment(coven_github_api::IssueCommentEvent {
                installation_id: 123,
                repo_owner: "OpenCoven".to_string(),
                repo_name: "coven-code".to_string(),
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
                comment_body: "@coven-cody review".to_string(),
                commenter_login: "octocat".to_string(),
                on_pull_request: false,
            }),
        )
        .await
        .expect("review on an issue should clarify");
        assert!(matches!(
            task.kind,
            TaskKind::CommandReply { ref body, .. } if body.contains("needs a pull request")
        ));
    }
}

#[cfg(test)]
mod delivery_idempotency_tests {
    //! Route-level proof of the persist-then-ack contract (issue #2): the
    //! delivery record exists before GitHub hears success, and a redelivered
    //! delivery id never dispatches twice.
    use super::tests::app_state;
    use super::*;
    use axum::extract::State;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    const SECRET: &str = "secret"; // matches app_state's webhook_secret

    fn signed_headers(event: &str, delivery_id: Option<&str>, body: &str) -> HeaderMap {
        let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).expect("hmac key");
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", event.parse().expect("header"));
        headers.insert("x-hub-signature-256", sig.parse().expect("header"));
        if let Some(id) = delivery_id {
            headers.insert("x-github-delivery", id.parse().expect("header"));
        }
        headers
    }

    fn assigned_payload() -> String {
        serde_json::json!({
            "action": "assigned",
            "issue": { "number": 42, "title": "Fix auth", "body": "b" },
            "assignee": { "login": "coven-cody[bot]" },
            "repository": { "name": "demo", "owner": { "login": "OpenCoven" } },
            "installation": { "id": 7 }
        })
        .to_string()
    }

    async fn call(
        state: &AppState,
        headers: HeaderMap,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = handle_webhook(
            State(state.clone()),
            headers,
            Bytes::from(body.to_string()),
        )
        .await
        .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn missing_delivery_id_is_rejected_and_nothing_persists() {
        let state = app_state();
        let body = assigned_payload();
        let (status, json) = call(&state, signed_headers("issues", None, &body), &body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json["error"], "missing delivery id");
        assert!(state.store.task_states().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn installation_deleted_purges_only_that_installations_memory() {
        let state = app_state();
        let mem = |installation_id: u64| coven_github_store::MemoryActivity {
            at: String::new(),
            installation_id,
            repo: "OpenCoven/demo".to_string(),
            task_id: "t".to_string(),
            op: "read".to_string(),
            target: "repo/OpenCoven/demo/x".to_string(),
            scope: "repo".to_string(),
            outcome: "accepted".to_string(),
        };
        state.store.record_memory_activity(vec![mem(7)]).await.unwrap();
        state.store.record_memory_activity(vec![mem(99)]).await.unwrap();
        state
            .store
            .record_revocation(7, "OpenCoven/demo", "repo/OpenCoven/demo/x")
            .await
            .unwrap();

        let body = serde_json::json!({ "action": "deleted", "installation": { "id": 7 } })
            .to_string();
        let (status, _) =
            call(&state, signed_headers("installation", Some("del-1"), &body), &body).await;
        assert_eq!(status, StatusCode::OK);

        // Installation 7's memory and revocations are purged; 99 is untouched.
        assert!(state
            .store
            .list_memory(Some(ApiScope { installation_id: 7, repos: None }))
            .await
            .unwrap()
            .is_empty());
        assert!(state.store.revocations_for(7, "OpenCoven/demo").await.unwrap().is_empty());
        assert_eq!(
            state
                .store
                .list_memory(Some(ApiScope { installation_id: 99, repos: None }))
                .await
                .unwrap()
                .len(),
            1
        );

        // The purge (memory + task artifacts) is audited (issue #12).
        let audit = state.store.api_audit_entries().await.unwrap();
        assert!(
            audit.iter().any(|(_, _, action, _)| action == "delete_on_uninstall"),
            "the uninstall purge must leave an audit record: {audit:?}"
        );
    }

    #[tokio::test]
    async fn redelivered_delivery_id_never_dispatches_twice() {
        let state = app_state();
        let body = assigned_payload();

        let (status, json) =
            call(&state, signed_headers("issues", Some("dl-1"), &body), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["duplicate"], serde_json::Value::Null);

        // GitHub redelivers the same delivery id.
        let (status, json) =
            call(&state, signed_headers("issues", Some("dl-1"), &body), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["duplicate"], true);

        // Exactly one durable task row — the queue IS the table, so a second
        // row is what "dispatching twice" would mean.
        let states = state.store.task_states().await.unwrap();
        assert_eq!(states.len(), 1, "one durable task: {states:?}");
        assert_eq!(states[0].1, "queued");

        // The delivery record ties the id to the routed task.
        let routing = state.store.delivery_routing("dl-1").await.unwrap();
        assert_eq!(
            routing.as_deref(),
            Some(format!("task:{}", states[0].0).as_str())
        );
    }

    #[tokio::test]
    async fn ping_and_unroutable_deliveries_are_recorded_as_ignored() {
        let state = app_state();

        let ping = r#"{"zen":"Keep it logically awesome.","hook_id":1}"#;
        let (status, json) =
            call(&state, signed_headers("ping", Some("dl-ping"), ping), ping).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["pong"], true);
        assert_eq!(
            state.store.delivery_routing("dl-ping").await.unwrap().as_deref(),
            Some("ignored:ping")
        );

        // An event no familiar routes: recorded, acknowledged, no task.
        let body = serde_json::json!({
            "action": "assigned",
            "issue": { "number": 1, "title": "t", "body": "b" },
            "assignee": { "login": "someone-else" },
            "repository": { "name": "demo", "owner": { "login": "OpenCoven" } },
            "installation": { "id": 7 }
        })
        .to_string();
        let (status, _) =
            call(&state, signed_headers("issues", Some("dl-2"), &body), &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            state.store.delivery_routing("dl-2").await.unwrap().as_deref(),
            Some("ignored:unroutable")
        );
        assert!(state.store.task_states().await.unwrap().is_empty());
    }
}

#[cfg(test)]
mod tenancy_tests {
    //! The tenant boundary on the task API (issue #3): token mode fails
    //! closed, a tenant sees only its own installation, and every read is
    //! audited.
    use super::tests::{app_state, seed_task};
    use super::*;
    use axum::extract::State;
    use coven_github_config::TenantToken;
    use std::sync::Arc;

    fn token_state(api: ApiConfig) -> AppState {
        let base = app_state();
        let mut config = (*base.config).clone();
        config.api = api;
        AppState {
            config: Arc::new(config),
            ..base
        }
    }

    fn two_tenant_api() -> ApiConfig {
        ApiConfig {
            mode: ApiMode::Token,
            service_token: Some("service-token-0123456789abcdef".to_string()),
            tenants: vec![
                TenantToken {
                    token: "tenant-one-0123456789abcdef".to_string(),
                    installation_id: 1,
                    repos: vec![],
                },
                TenantToken {
                    token: "tenant-two-0123456789abcdef".to_string(),
                    installation_id: 2,
                    repos: vec![],
                },
            ],
        }
    }

    fn task_for(id: &str, installation_id: u64, repo_name: &str) -> Task {
        Task {
            id: id.to_string(),
            installation_id,
            repo_owner: "OpenCoven".to_string(),
            repo_name: repo_name.to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: format!("task {id}"),
                issue_body: "b".to_string(),
            },
        }
    }

    async fn list(state: &AppState, bearer: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if let Some(token) = bearer {
            headers.insert(
                "authorization",
                format!("Bearer {token}").parse().expect("header"),
            );
        }
        let response = list_tasks(State(state.clone()), headers)
            .await
            .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    fn repos_of(json: &serde_json::Value) -> Vec<String> {
        json["tasks"]
            .as_array()
            .expect("tasks array")
            .iter()
            .map(|t| t["repo"].as_str().expect("repo").to_string())
            .collect()
    }

    #[tokio::test]
    async fn open_mode_lists_without_auth_for_local_development() {
        let state = app_state();
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;
        let (status, json) = list(&state, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["tasks"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn token_mode_fails_closed_and_reveals_nothing() {
        let state = token_state(two_tenant_api());
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;

        for bearer in [None, Some("wrong-token-0123456789abcdef")] {
            let (status, json) = list(&state, bearer).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "bearer: {bearer:?}");
            assert_eq!(json["error"], "unauthorized");
            assert!(json.get("tasks").is_none(), "no data may leak: {json}");
        }

        let audit = state.store.api_audit_entries().await.expect("audit");
        assert_eq!(audit.len(), 2);
        for (caller, scope, action, result) in audit {
            assert_eq!(caller, "anonymous");
            assert_eq!(scope, "none");
            assert_eq!(action, "list_tasks");
            assert_eq!(result, "denied");
        }
    }

    #[tokio::test]
    async fn tenant_token_sees_only_its_own_installation() {
        let state = token_state(two_tenant_api());
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;
        seed_task(&state, "d2", &task_for("t2", 2, "beta")).await;

        // Installation 1's token must never see installation 2's task.
        let (status, json) = list(&state, Some("tenant-one-0123456789abcdef")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(repos_of(&json), vec!["OpenCoven/alpha".to_string()]);

        let (_, json) = list(&state, Some("tenant-two-0123456789abcdef")).await;
        assert_eq!(repos_of(&json), vec!["OpenCoven/beta".to_string()]);

        // The operator-wide service token sees both.
        let (_, json) = list(&state, Some("service-token-0123456789abcdef")).await;
        assert_eq!(json["tasks"].as_array().unwrap().len(), 2);

        let audit = state.store.api_audit_entries().await.expect("audit");
        assert_eq!(
            audit
                .iter()
                .map(|(caller, scope, _, result)| (caller.as_str(), scope.as_str(), result.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("tenant:1", "installation:1", "ok:1"),
                ("tenant:2", "installation:2", "ok:1"),
                ("service", "all", "ok:2"),
            ]
        );
    }

    async fn list_mem(state: &AppState, bearer: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if let Some(token) = bearer {
            headers.insert(
                "authorization",
                format!("Bearer {token}").parse().expect("header"),
            );
        }
        let response = list_memory(State(state.clone()), headers)
            .await
            .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    async fn call_audit(state: &AppState, bearer: Option<&str>) -> (StatusCode, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if let Some(token) = bearer {
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        let response = audit(State(state.clone()), headers).await.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn call_routing(
        state: &AppState,
        bearer: Option<&str>,
        installation_id: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if let Some(token) = bearer {
            headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        let mut params = std::collections::HashMap::new();
        if let Some(id) = installation_id {
            params.insert("installation_id".to_string(), id.to_string());
        }
        let response = routing(State(state.clone()), headers, Query(params))
            .await
            .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn seed_ignored(state: &AppState, installation_id: u64) {
        state
            .store
            .record_delivery(
                coven_github_store::Delivery {
                    delivery_id: format!("ig-{installation_id}"),
                    event: "issues".to_string(),
                    action: Some("closed".to_string()),
                    installation_id: Some(installation_id),
                    repo: Some("OpenCoven/demo".to_string()),
                    payload_hash: "h".to_string(),
                },
                coven_github_store::Routing::Ignored("unsupported"),
            )
            .await
            .expect("seed ignored delivery");
    }

    #[tokio::test]
    async fn audit_is_tenant_scoped_and_fails_closed() {
        let state = token_state(two_tenant_api());
        seed_ignored(&state, 1).await;
        seed_ignored(&state, 2).await;

        // No token in token mode → fail closed.
        let (status, json) = call_audit(&state, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(json.get("events").is_none());

        // Installation 1's token sees only its own event.
        let (status, json) = call_audit(&state, Some("tenant-one-0123456789abcdef")).await;
        assert_eq!(status, StatusCode::OK);
        let events = json["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "ignored:unsupported");

        // Service sees both installations.
        let (_, json) = call_audit(&state, Some("service-token-0123456789abcdef")).await;
        assert_eq!(json["events"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn routing_view_is_tenant_scoped_and_fails_closed() {
        let state = token_state(two_tenant_api());

        // Denied without a token.
        let (status, _) = call_routing(&state, None, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A tenant sees its own installation's routing view.
        let (status, json) = call_routing(&state, Some("tenant-one-0123456789abcdef"), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["routing"]["installationId"], 1);

        // Service must name the installation.
        let (status, _) = call_routing(&state, Some("service-token-0123456789abcdef"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, json) =
            call_routing(&state, Some("service-token-0123456789abcdef"), Some("42")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["routing"]["installationId"], 42);
    }

    async fn seed_memory(state: &AppState, installation_id: u64, repo: &str) {
        state
            .store
            .record_memory_activity(vec![coven_github_store::MemoryActivity {
                at: String::new(),
                installation_id,
                repo: repo.to_string(),
                task_id: "t".to_string(),
                op: "read".to_string(),
                target: format!("repo/{repo}/x"),
                scope: "repo".to_string(),
                outcome: "accepted".to_string(),
            }])
            .await
            .expect("record memory");
    }

    #[tokio::test]
    async fn memory_inspect_is_tenant_scoped_and_fails_closed() {
        let state = token_state(two_tenant_api());
        seed_memory(&state, 1, "OpenCoven/alpha").await;
        seed_memory(&state, 2, "OpenCoven/beta").await;

        // No / wrong token in token mode → fail closed, no data leaks.
        for bearer in [None, Some("wrong-token-0123456789abcdef")] {
            let (status, json) = list_mem(&state, bearer).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "bearer: {bearer:?}");
            assert!(json.get("memory").is_none(), "no data may leak: {json}");
        }

        // Installation 1's token sees only its own memory activity.
        let (status, json) = list_mem(&state, Some("tenant-one-0123456789abcdef")).await;
        assert_eq!(status, StatusCode::OK);
        let repos: Vec<&str> = json["memory"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["repo"].as_str().unwrap())
            .collect();
        assert_eq!(repos, vec!["OpenCoven/alpha"]);

        // The service token sees both installations.
        let (_, json) = list_mem(&state, Some("service-token-0123456789abcdef")).await;
        assert_eq!(json["memory"].as_array().unwrap().len(), 2);
    }

    async fn revoke(
        state: &AppState,
        bearer: Option<&str>,
        repo: &str,
        key: &str,
    ) -> (StatusCode, serde_json::Value) {
        let mut headers = HeaderMap::new();
        if let Some(token) = bearer {
            headers.insert(
                "authorization",
                format!("Bearer {token}").parse().expect("header"),
            );
        }
        let response = revoke_memory(
            State(state.clone()),
            headers,
            Json(RevokeRequest {
                repo: repo.to_string(),
                key: key.to_string(),
                installation_id: None,
            }),
        )
        .await
        .into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    #[tokio::test]
    async fn revoke_is_tenant_scoped_and_persists() {
        let mut api = two_tenant_api();
        api.tenants[0].repos = vec!["OpenCoven/alpha".to_string()];
        let state = token_state(api);

        // In-scope revocation succeeds and is stored.
        let (status, json) = revoke(
            &state,
            Some("tenant-one-0123456789abcdef"),
            "OpenCoven/alpha",
            "repo/OpenCoven/alpha/secrets/",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["ok"], true);
        assert_eq!(
            state
                .store
                .revocations_for(1, "OpenCoven/alpha")
                .await
                .unwrap(),
            vec!["repo/OpenCoven/alpha/secrets/"]
        );

        // A repo outside the tenant's scope is forbidden.
        let (status, _) = revoke(
            &state,
            Some("tenant-one-0123456789abcdef"),
            "OpenCoven/beta",
            "k",
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // No credential fails closed.
        let (status, _) = revoke(&state, None, "OpenCoven/alpha", "k").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tenant_repo_scope_narrows_within_the_installation() {
        let mut api = two_tenant_api();
        api.tenants[0].repos = vec!["OpenCoven/alpha".to_string()];
        let state = token_state(api);
        seed_task(&state, "d1", &task_for("t1", 1, "alpha")).await;
        seed_task(&state, "d2", &task_for("t2", 1, "gamma")).await;

        let (_, json) = list(&state, Some("tenant-one-0123456789abcdef")).await;
        assert_eq!(repos_of(&json), vec!["OpenCoven/alpha".to_string()]);
    }
}

#[cfg(test)]
mod installation_routing_tests {
    //! Installation-scoped routing (issue #7): the same label routes to
    //! different familiars per installation, unknown installations fail
    //! closed, and per-repo policy can switch trigger lanes off.
    use super::tests::app_state;
    use super::*;
    use coven_github_api::{IssueAssignedEvent, IssueCommentEvent, IssueLabeledEvent};
    use coven_github_config::{FamiliarConfig, InstallationConfig, RepoRoutingOverride};
    use std::sync::Arc;

    /// cody and nova both claim `coven:fix`; installations 1 and 2 allow-list
    /// one familiar each.
    fn two_installation_state() -> AppState {
        let base = app_state();
        let mut config = (*base.config).clone();
        config.familiars = vec![
            FamiliarConfig {
                id: "cody".to_string(),
                display_name: "Cody".to_string(),
                bot_username: "coven-cody[bot]".to_string(),
                model: None,
                skills: vec![],
                trigger_labels: vec!["coven:fix".to_string()],
            },
            FamiliarConfig {
                id: "nova".to_string(),
                display_name: "Nova".to_string(),
                bot_username: "coven-nova[bot]".to_string(),
                model: None,
                skills: vec![],
                trigger_labels: vec!["coven:fix".to_string()],
            },
        ];
        config.installations = vec![
            InstallationConfig {
                id: 1,
                account: Some("acme".to_string()),
                familiars: vec!["cody".to_string()],
                triggers: Default::default(),
                limits: Default::default(),
                repos: std::collections::HashMap::from([(
                    "acme/frozen".to_string(),
                    RepoRoutingOverride {
                        enabled: Some(false),
                        ..Default::default()
                    },
                ), (
                    "acme/no-labels".to_string(),
                    RepoRoutingOverride {
                        labels: Some(false),
                        ..Default::default()
                    },
                )]),
            },
            InstallationConfig {
                id: 2,
                account: Some("globex".to_string()),
                familiars: vec!["nova".to_string()],
                triggers: Default::default(),
                limits: Default::default(),
                repos: std::collections::HashMap::new(),
            },
        ];
        AppState {
            config: Arc::new(config),
            ..base
        }
    }

    fn labeled(installation_id: u64, owner: &str, name: &str) -> GitHubEvent {
        GitHubEvent::IssueLabeled(IssueLabeledEvent {
            installation_id,
            repo_owner: owner.to_string(),
            repo_name: name.to_string(),
            issue_number: 42,
            issue_title: "t".to_string(),
            issue_body: "b".to_string(),
            label_name: "coven:fix".to_string(),
        })
    }

    #[tokio::test]
    async fn same_label_routes_to_different_familiars_per_installation() {
        let state = two_installation_state();

        let a = event_to_task(&state, labeled(1, "acme", "app"))
            .await
            .expect("installation 1 routes");
        assert_eq!(a.familiar_id, "cody");

        let b = event_to_task(&state, labeled(2, "globex", "app"))
            .await
            .expect("installation 2 routes");
        assert_eq!(b.familiar_id, "nova");
    }

    #[tokio::test]
    async fn unknown_installation_fails_closed_once_installations_exist() {
        let state = two_installation_state();
        assert!(
            event_to_task(&state, labeled(999, "stranger", "app"))
                .await
                .is_none(),
            "an unlisted installation must route nothing"
        );
    }

    #[tokio::test]
    async fn assignment_respects_the_installation_familiar_allow_list() {
        let state = two_installation_state();
        // nova is not allow-listed for installation 1: assigning to nova's
        // bot there must not route, while cody's bot does.
        let assigned = |login: &str| {
            GitHubEvent::IssueAssigned(IssueAssignedEvent {
                installation_id: 1,
                repo_owner: "acme".to_string(),
                repo_name: "app".to_string(),
                issue_number: 42,
                issue_title: "t".to_string(),
                issue_body: "b".to_string(),
                assignee_login: login.to_string(),
            })
        };
        assert!(event_to_task(&state, assigned("coven-nova[bot]"))
            .await
            .is_none());
        let task = event_to_task(&state, assigned("coven-cody[bot]"))
            .await
            .expect("allow-listed familiar routes");
        assert_eq!(task.familiar_id, "cody");
    }

    #[tokio::test]
    async fn repo_policy_disables_triggers_per_repository() {
        let state = two_installation_state();
        // enabled = false: every lane off for that repo only.
        assert!(event_to_task(&state, labeled(1, "acme", "frozen"))
            .await
            .is_none());
        // labels = false: the label lane is off, but commands still work.
        assert!(event_to_task(&state, labeled(1, "acme", "no-labels"))
            .await
            .is_none());
        let command = GitHubEvent::IssueComment(IssueCommentEvent {
            installation_id: 1,
            repo_owner: "acme".to_string(),
            repo_name: "no-labels".to_string(),
            issue_number: 42,
            issue_title: "t".to_string(),
            issue_body: "b".to_string(),
            comment_body: "@coven-cody status".to_string(),
            commenter_login: "octocat".to_string(),
            on_pull_request: false,
        });
        assert!(
            event_to_task(&state, command).await.is_some(),
            "the commands lane must stay open when only labels are disabled"
        );
    }

    #[tokio::test]
    async fn open_routing_is_preserved_when_no_installations_are_configured() {
        // The self-hosted default: app_state has no [[installations]].
        let state = app_state();
        let task = event_to_task(&state, labeled(123, "OpenCoven", "coven-code"))
            .await
            .expect("open routing must keep working");
        assert_eq!(task.familiar_id, "cody");
    }
}

#[cfg(test)]
mod metering_route_tests {
    //! The intake daily cap and the tenant-scoped usage endpoint (issue #15).
    use super::tests::{app_state, seed_task};
    use super::*;
    use axum::extract::State;
    use coven_github_config::{InstallationConfig, InstallationLimits, TenantToken};
    use hmac::{Hmac, Mac};
    use sha2::Sha256 as HmacSha;
    use std::sync::Arc;

    fn capped_state(max_tasks_per_day: u32) -> AppState {
        let base = app_state();
        let mut config = (*base.config).clone();
        config.installations = vec![InstallationConfig {
            id: 7,
            account: None,
            familiars: vec![],
            triggers: Default::default(),
            limits: InstallationLimits {
                max_concurrent: None,
                max_tasks_per_day: Some(max_tasks_per_day),
            },
            repos: std::collections::HashMap::new(),
        }];
        AppState {
            config: Arc::new(config),
            ..base
        }
    }

    fn signed(event: &str, delivery_id: &str, body: &str) -> HeaderMap {
        let mut mac = Hmac::<HmacSha>::new_from_slice(b"secret").expect("hmac");
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", event.parse().expect("header"));
        headers.insert("x-hub-signature-256", sig.parse().expect("header"));
        headers.insert("x-github-delivery", delivery_id.parse().expect("header"));
        headers
    }

    fn assigned(installation: u64, issue: u64) -> String {
        serde_json::json!({
            "action": "assigned",
            "issue": { "number": issue, "title": "t", "body": "b" },
            "assignee": { "login": "coven-cody[bot]" },
            "repository": { "name": "demo", "owner": { "login": "OpenCoven" } },
            "installation": { "id": installation }
        })
        .to_string()
    }

    async fn deliver(state: &AppState, delivery_id: &str, body: &str) -> serde_json::Value {
        let response = handle_webhook(
            State(state.clone()),
            signed("issues", delivery_id, body),
            Bytes::from(body.to_string()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    #[tokio::test]
    async fn daily_cap_ignores_overflow_and_records_the_reason() {
        let state = capped_state(1);

        let first = deliver(&state, "dl-1", &assigned(7, 1)).await;
        assert!(first.get("ignored").is_none());

        let second = deliver(&state, "dl-2", &assigned(7, 2)).await;
        assert_eq!(second["ignored"], "quota_exceeded");

        // One durable task; the overflow delivery is audited as ignored.
        assert_eq!(state.store.task_states().await.unwrap().len(), 1);
        assert_eq!(
            state.store.delivery_routing("dl-2").await.unwrap().as_deref(),
            Some("ignored:quota_exceeded")
        );
    }

    #[tokio::test]
    async fn uncapped_installations_are_unaffected() {
        let state = capped_state(1);
        // installation 8 has no [[installations]] block… but blocks exist, so
        // routing fails closed for it — use the capped installation's sibling
        // config instead: an unlimited installation block.
        let mut config = (*state.config).clone();
        config.installations.push(InstallationConfig {
            id: 8,
            account: None,
            familiars: vec![],
            triggers: Default::default(),
            limits: InstallationLimits::default(),
            repos: std::collections::HashMap::new(),
        });
        let state = AppState {
            config: Arc::new(config),
            ..state
        };
        for (i, delivery_id) in ["dl-a", "dl-b", "dl-c"].iter().enumerate() {
            let json = deliver(&state, delivery_id, &assigned(8, i as u64 + 1)).await;
            assert!(json.get("ignored").is_none(), "delivery {i}: {json}");
        }
        assert_eq!(state.store.task_states().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn usage_endpoint_is_tenant_scoped_and_audited() {
        let base = app_state();
        let mut config = (*base.config).clone();
        config.api = ApiConfig {
            mode: ApiMode::Token,
            service_token: None,
            tenants: vec![TenantToken {
                token: "tenant-one-0123456789abcdef".to_string(),
                installation_id: 1,
                repos: vec![],
            }],
        };
        let state = AppState {
            config: Arc::new(config),
            ..base
        };
        let mk = |id: &str, installation_id: u64| Task {
            id: id.to_string(),
            installation_id,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: "t".to_string(),
                issue_body: "b".to_string(),
            },
        };
        seed_task(&state, "d1", &mk("mine", 1)).await;
        seed_task(&state, "d2", &mk("theirs", 2)).await;

        // No token: fail closed.
        let response = usage(State(state.clone()), HeaderMap::new())
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Tenant token: only installation 1's rollup.
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer tenant-one-0123456789abcdef".parse().expect("header"),
        );
        let response = usage(State(state.clone()), headers).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let rows = json["usage"].as_array().expect("usage rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["installationId"], 1);

        let audit = state.store.api_audit_entries().await.expect("audit");
        assert!(
            audit
                .iter()
                .any(|(c, s, a, r)| c == "tenant:1" && s == "installation:1" && a == "usage" && r == "ok:1"),
            "usage reads must be audited: {audit:?}"
        );
    }
}

#[cfg(test)]
mod billing_route_tests {
    //! Marketplace plan intake: entitlement recording, plan-derived limits,
    //! and the `require_plan` monetization gate.
    use super::tests::app_state;
    use super::*;
    use axum::extract::State;
    use hmac::{Hmac, Mac};
    use sha2::Sha256 as HmacSha;
    use std::sync::Arc;

    fn signed(event: &str, delivery_id: &str, body: &str) -> HeaderMap {
        let mut mac = Hmac::<HmacSha>::new_from_slice(b"secret").expect("hmac");
        mac.update(body.as_bytes());
        let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert("x-github-event", event.parse().expect("header"));
        headers.insert("x-hub-signature-256", sig.parse().expect("header"));
        headers.insert("x-github-delivery", delivery_id.parse().expect("header"));
        headers
    }

    async fn deliver(
        state: &AppState,
        event: &str,
        delivery_id: &str,
        body: &str,
    ) -> serde_json::Value {
        let response = handle_webhook(
            State(state.clone()),
            signed(event, delivery_id, body),
            Bytes::from(body.to_string()),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        serde_json::from_slice(&bytes).expect("json")
    }

    fn purchase(action: &str, account_id: u64, plan: &str, on_free_trial: bool) -> String {
        serde_json::json!({
            "action": action,
            "marketplace_purchase": {
                "account": { "id": account_id, "login": "acme", "type": "Organization" },
                "plan": { "name": plan },
                "on_free_trial": on_free_trial
            }
        })
        .to_string()
    }

    fn installation_created(installation: u64, account_id: u64) -> String {
        serde_json::json!({
            "action": "created",
            "installation": {
                "id": installation,
                "account": { "id": account_id, "login": "acme" }
            }
        })
        .to_string()
    }

    fn assigned(installation: u64, issue: u64) -> String {
        serde_json::json!({
            "action": "assigned",
            "issue": { "number": issue, "title": "t", "body": "b" },
            "assignee": { "login": "coven-cody[bot]" },
            "repository": { "name": "demo", "owner": { "login": "OpenCoven" } },
            "installation": { "id": installation }
        })
        .to_string()
    }

    fn require_plan_state() -> AppState {
        let base = app_state();
        let mut config = (*base.config).clone();
        config.billing.require_plan = true;
        AppState {
            config: Arc::new(config),
            ..base
        }
    }

    #[tokio::test]
    async fn purchase_records_the_plan_and_resolves_through_installations() {
        let state = app_state();
        let body = purchase("purchased", 42, "Hosted Team", false);
        let response = deliver(&state, "marketplace_purchase", "mp-1", &body).await;
        assert_eq!(response["plan"], "team");
        assert_eq!(response["state"], "active");

        deliver(
            &state,
            "installation",
            "in-1",
            &installation_created(7, 42),
        )
        .await;
        let plan = state
            .store
            .plan_for_installation(7)
            .await
            .unwrap()
            .expect("plan resolved");
        assert_eq!(plan.tier, "team");
        assert!(plan.entitled());
    }

    #[tokio::test]
    async fn duplicate_purchase_deliveries_do_not_reapply() {
        let state = app_state();
        let body = purchase("purchased", 42, "Hosted Team", false);
        deliver(&state, "marketplace_purchase", "mp-1", &body).await;
        let dup = deliver(&state, "marketplace_purchase", "mp-1", &body).await;
        assert_eq!(dup["duplicate"], true);
    }

    #[tokio::test]
    async fn require_plan_gates_intake_until_a_purchase_lands() {
        let state = require_plan_state();
        deliver(
            &state,
            "installation",
            "in-1",
            &installation_created(7, 42),
        )
        .await;

        // No plan → the delivery is acknowledged but ignored, and audited.
        let refused = deliver(&state, "issues", "dl-1", &assigned(7, 1)).await;
        assert_eq!(refused["ignored"], "no_plan");
        assert_eq!(
            state.store.delivery_routing("dl-1").await.unwrap().as_deref(),
            Some("ignored:no_plan")
        );

        // Trial purchase → the same trigger now produces a task.
        deliver(
            &state,
            "marketplace_purchase",
            "mp-1",
            &purchase("purchased", 42, "Hosted Starter", true),
        )
        .await;
        let accepted = deliver(&state, "issues", "dl-2", &assigned(7, 2)).await;
        assert!(accepted.get("ignored").is_none());
        assert_eq!(state.store.task_states().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancellation_revokes_entitlement() {
        let state = require_plan_state();
        deliver(
            &state,
            "installation",
            "in-1",
            &installation_created(7, 42),
        )
        .await;
        deliver(
            &state,
            "marketplace_purchase",
            "mp-1",
            &purchase("purchased", 42, "Hosted Team", false),
        )
        .await;
        deliver(
            &state,
            "marketplace_purchase",
            "mp-2",
            &purchase("cancelled", 42, "Hosted Team", false),
        )
        .await;

        let refused = deliver(&state, "issues", "dl-1", &assigned(7, 1)).await;
        assert_eq!(refused["ignored"], "no_plan");
    }

    #[tokio::test]
    async fn pending_changes_do_not_alter_the_plan() {
        let state = app_state();
        deliver(
            &state,
            "marketplace_purchase",
            "mp-1",
            &purchase("purchased", 42, "Hosted Team", false),
        )
        .await;
        deliver(
            &state,
            "marketplace_purchase",
            "mp-2",
            &purchase("pending_change", 42, "Hosted Starter", false),
        )
        .await;
        deliver(
            &state,
            "installation",
            "in-1",
            &installation_created(7, 42),
        )
        .await;
        let plan = state.store.plan_for_installation(7).await.unwrap().unwrap();
        assert_eq!(plan.tier, "team");
    }

    #[tokio::test]
    async fn uninstall_forgets_the_account_mapping() {
        let state = app_state();
        deliver(
            &state,
            "marketplace_purchase",
            "mp-1",
            &purchase("purchased", 42, "Hosted Team", false),
        )
        .await;
        deliver(
            &state,
            "installation",
            "in-1",
            &installation_created(7, 42),
        )
        .await;
        assert!(state.store.plan_for_installation(7).await.unwrap().is_some());

        let body = serde_json::json!({
            "action": "deleted",
            "installation": { "id": 7, "account": { "id": 42, "login": "acme" } }
        })
        .to_string();
        deliver(&state, "installation", "in-2", &body).await;
        assert!(state.store.plan_for_installation(7).await.unwrap().is_none());
    }
}
