//! Durable adapter state: webhook deliveries, tasks, and attempt records
//! (issue #2). Design: `docs/durable-task-store.md`.
//!
//! Embedded SQLite via rusqlite. One writer connection behind a mutex, WAL
//! journal, forward-only migrations tracked by `PRAGMA user_version`. All
//! async entry points hop to `spawn_blocking`; SQLite work never blocks the
//! runtime.

use anyhow::{Context, Result};
use coven_github_api::tasks::{surface_of, TaskListItem, TaskListStatus};
use coven_github_api::{Task, TaskKind};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Current schema version, stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i32 = 5;

/// Handle to the durable store. Cheap to clone; all clones share one writer
/// connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Coordinates of one GitHub webhook delivery, keyed by `X-GitHub-Delivery`.
#[derive(Debug, Clone)]
pub struct Delivery {
    pub delivery_id: String,
    pub event: String,
    pub action: Option<String>,
    pub installation_id: Option<u64>,
    /// `owner/name` when the payload names a repository.
    pub repo: Option<String>,
    /// Hex SHA-256 of the raw request body.
    pub payload_hash: String,
}

/// How the adapter routed a delivery.
pub enum Routing<'a> {
    /// The delivery produced a task to execute.
    Task(&'a Task),
    /// The delivery was acknowledged without work (ping, unroutable event,
    /// casual mention, …).
    Ignored(&'a str),
}

/// Whether a delivery was seen for the first time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recorded {
    New,
    /// The delivery id was already recorded — a GitHub redelivery. Callers
    /// MUST NOT dispatch work for duplicates.
    Duplicate,
}

impl Store {
    /// Opens (creating if needed) the store at `path` and runs migrations.
    /// Parent directories are created.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create store directory {}", parent.display())
                })?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open store at {}", path.display()))?;
        Self::init(conn)
    }

    /// In-memory store for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Records a delivery and its routing outcome atomically, enqueueing the
    /// task row when the routing produced one. Returns [`Recorded::Duplicate`]
    /// — persisting nothing further — when the delivery id was already seen.
    ///
    /// For PR-review tasks, older still-`queued` reviews of the same PR are
    /// tombstoned `superseded` in the same transaction (issue #10 semantics,
    /// durable form).
    pub async fn record_delivery(
        &self,
        delivery: Delivery,
        routing: Routing<'_>,
    ) -> Result<Recorded> {
        let routing_label = match &routing {
            Routing::Task(task) => format!("task:{}", task.id),
            Routing::Ignored(reason) => format!("ignored:{reason}"),
        };
        let task = match routing {
            Routing::Task(task) => Some(task.clone()),
            Routing::Ignored(_) => None,
        };
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().expect("store mutex poisoned");
            record_delivery_sync(&mut conn, &delivery, &routing_label, task.as_ref())
        })
        .await
        .expect("store task panicked")
    }

    /// Routing label recorded for a delivery id, if the delivery was seen.
    pub async fn delivery_routing(&self, delivery_id: &str) -> Result<Option<String>> {
        let conn = self.conn.clone();
        let id = delivery_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let mut stmt =
                conn.prepare("SELECT routing FROM webhook_deliveries WHERE delivery_id = ?1")?;
            let mut rows = stmt.query(params![id])?;
            match rows.next()? {
                Some(row) => Ok(Some(row.get(0)?)),
                None => Ok(None),
            }
        })
        .await
        .expect("store task panicked")
    }

    /// `(task_id, state)` pairs, oldest first. Read surface for tests and ops.
    pub async fn task_states(&self) -> Result<Vec<(String, String)>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let mut stmt =
                conn.prepare("SELECT id, state FROM tasks ORDER BY created_at, id")?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .expect("store task panicked")
    }

    /// Atomically claims the oldest queued task, marking it `running` and
    /// opening an attempt record. Returns `None` when the queue is empty.
    /// Tombstoned (`superseded`) rows are never claimable.
    ///
    /// `concurrency_caps` maps installation ids to their `max_concurrent`
    /// limit (issue #15): a queued task whose installation is already at its
    /// cap is skipped — capacity frees when its running tasks finish, and the
    /// claim loop's poll timer retries.
    pub async fn claim_next(
        &self,
        concurrency_caps: &std::collections::HashMap<u64, u32>,
    ) -> Result<Option<Task>> {
        let conn = self.conn.clone();
        let caps = concurrency_caps.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().expect("store mutex poisoned");
            let now = now_rfc3339();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Installations at their concurrency cap right now.
            let mut capped: Vec<u64> = Vec::new();
            if !caps.is_empty() {
                let mut stmt = tx.prepare(
                    "SELECT installation_id, COUNT(*) FROM tasks
                     WHERE state = 'running' GROUP BY installation_id",
                )?;
                let running: Vec<(u64, u32)> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                for (installation, count) in running {
                    if caps.get(&installation).is_some_and(|cap| count >= *cap) {
                        capped.push(installation);
                    }
                }
            }
            let capped_json = serde_json::to_string(&capped)?;
            let claimed = tx
                .query_row(
                    "UPDATE tasks
                       SET state = 'running', attempts = attempts + 1, updated_at = ?1
                     WHERE id = (SELECT id FROM tasks
                                 WHERE state = 'queued'
                                   AND installation_id NOT IN
                                       (SELECT value FROM json_each(?2))
                                 ORDER BY created_at, id LIMIT 1)
                     RETURNING id, installation_id, repo, familiar_id, kind,
                               commander, attempts",
                    params![now, capped_json],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, u32>(6)?,
                        ))
                    },
                )
                .optional()?;
            let Some((id, installation_id, repo, familiar_id, kind_json, commander, attempt)) =
                claimed
            else {
                tx.commit()?;
                return Ok(None);
            };
            tx.execute(
                "INSERT INTO task_attempts (task_id, attempt, started_at)
                 VALUES (?1, ?2, ?3)",
                params![id, attempt, now],
            )?;
            tx.commit()?;

            let (repo_owner, repo_name) = repo
                .split_once('/')
                .map(|(o, n)| (o.to_string(), n.to_string()))
                .unwrap_or_else(|| (repo.clone(), String::new()));
            let kind: TaskKind =
                serde_json::from_str(&kind_json).context("stored task kind is unreadable")?;
            Ok(Some(Task {
                id,
                installation_id,
                repo_owner,
                repo_name,
                familiar_id,
                commander,
                kind,
            }))
        })
        .await
        .expect("store task panicked")
    }

    /// Records the Check Run URL once pre-flight created it.
    pub async fn set_check_run_url(&self, task_id: &str, url: &str) -> Result<()> {
        let conn = self.conn.clone();
        let (task_id, url) = (task_id.to_string(), url.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            conn.execute(
                "UPDATE tasks SET check_run_url = ?1, updated_at = ?2 WHERE id = ?3",
                params![url, now_rfc3339(), task_id],
            )?;
            Ok(())
        })
        .await
        .expect("store task panicked")
    }

    /// Moves a task to its terminal state and closes the open attempt.
    /// Idempotent and safe on unknown ids (0 rows updated).
    pub async fn finish(&self, task_id: &str, terminal: Terminal) -> Result<()> {
        let conn = self.conn.clone();
        let task_id = task_id.to_string();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().expect("store mutex poisoned");
            let now = now_rfc3339();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE tasks
                   SET state = ?1, result_status = ?2, branch = ?3, pr_number = ?4,
                       summary = ?5, updated_at = ?6
                 WHERE id = ?7",
                params![
                    terminal.state.as_str(),
                    terminal.result_status,
                    terminal.branch,
                    terminal.pr_number,
                    terminal.summary,
                    now,
                    task_id,
                ],
            )?;
            tx.execute(
                "UPDATE task_attempts SET ended_at = ?1, outcome = ?2, detail = ?3
                 WHERE task_id = ?4 AND ended_at IS NULL",
                params![now, terminal.state.as_str(), terminal.detail, task_id],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("store task panicked")
    }

    /// Startup recovery: any `running` row belongs to a dead process (one
    /// process owns the store). Requeue it — or fail it once its claim
    /// attempts reach `max_attempts`, so a crash-looping task cannot poison
    /// the queue. Returns `(requeued, failed)`.
    pub async fn recover_interrupted(&self, max_attempts: u32) -> Result<(usize, usize)> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().expect("store mutex poisoned");
            let now = now_rfc3339();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE task_attempts SET ended_at = ?1, outcome = 'interrupted'
                 WHERE ended_at IS NULL",
                params![now],
            )?;
            let failed = tx.execute(
                "UPDATE tasks SET state = 'failed', updated_at = ?1,
                        summary = COALESCE(summary, 'interrupted repeatedly; giving up')
                 WHERE state = 'running' AND attempts >= ?2",
                params![now, max_attempts],
            )?;
            let requeued = tx.execute(
                "UPDATE tasks SET state = 'queued', updated_at = ?1
                 WHERE state = 'running'",
                params![now],
            )?;
            tx.commit()?;
            Ok((requeued, failed))
        })
        .await
        .expect("store task panicked")
    }

    /// Tombstones every still-queued task for a supersession key (the
    /// maintainer `cancel` command). Returns how many were cancelled.
    pub async fn cancel_queued(&self, supersede_key: &str) -> Result<usize> {
        self.supersede_queued(supersede_key, None).await
    }

    /// Post-gate supersession for a command-initiated review (issue #13):
    /// once the worker has verified the commander's write access, older
    /// queued reviews of the same PR yield to `current_task_id`.
    pub async fn supersede_queued_except(
        &self,
        supersede_key: &str,
        current_task_id: &str,
    ) -> Result<usize> {
        self.supersede_queued(supersede_key, Some(current_task_id.to_string()))
            .await
    }

    async fn supersede_queued(
        &self,
        supersede_key: &str,
        except_task_id: Option<String>,
    ) -> Result<usize> {
        let conn = self.conn.clone();
        let key = supersede_key.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let n = conn.execute(
                "UPDATE tasks SET state = 'superseded', updated_at = ?1
                 WHERE supersede_key = ?2 AND state = 'queued'
                   AND (?3 IS NULL OR id <> ?3)",
                params![now_rfc3339(), key, except_task_id],
            )?;
            Ok(n)
        })
        .await
        .expect("store task panicked")
    }

    /// The Cave oversight projection: every non-reply task, newest first.
    /// `familiar_names` maps familiar ids to display names (config-owned).
    /// `scope` limits visibility to one tenant (issue #3); `None` is the
    /// adapter's own unrestricted view.
    pub async fn cave_list(
        &self,
        familiar_names: HashMap<String, String>,
        scope: Option<ApiScope>,
    ) -> Result<Vec<TaskListItem>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT id, repo, familiar_id, kind, state, result_status,
                        branch, pr_number, check_run_url, updated_at,
                        installation_id
                 FROM tasks
                 WHERE json_extract(kind, '$.kind') <> 'command_reply'
                   AND (?1 IS NULL OR installation_id = ?1)
                 ORDER BY updated_at DESC, id",
            )?;
            let installation_filter = scope.as_ref().map(|s| s.installation_id);
            let rows = stmt.query_map(params![installation_filter], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<u64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, u64>(10)?,
                ))
            })?;
            let mut items = Vec::new();
            for row in rows {
                let (
                    id,
                    repo,
                    familiar_id,
                    kind_json,
                    state,
                    result_status,
                    branch,
                    pr_number,
                    check_run_url,
                    updated_at,
                    _installation_id,
                ) = row?;
                // Repository narrowing within the installation, when scoped.
                if let Some(scope) = &scope {
                    if let Some(repos) = &scope.repos {
                        if !repos.contains(&repo) {
                            continue;
                        }
                    }
                }
                let kind: TaskKind = serde_json::from_str(&kind_json)
                    .context("stored task kind is unreadable")?;
                let (issue_number, issue_title) = surface_of(&kind);
                let status = project_status(&state, result_status.as_deref(), pr_number);
                items.push(TaskListItem {
                    id: id.clone(),
                    repo: repo.clone(),
                    issue_number,
                    issue_title,
                    branch,
                    pr_number,
                    pr_url: pr_number.map(|n| format!("https://github.com/{repo}/pull/{n}")),
                    status,
                    familiar_id: familiar_id.clone(),
                    familiar_name: familiar_names
                        .get(&familiar_id)
                        .cloned()
                        .unwrap_or(familiar_id),
                    session_id: Some(id),
                    updated_at,
                    check_run_url,
                });
            }
            Ok(items)
        })
        .await
        .expect("store task panicked")
    }
}

/// Tenant visibility limits for task API reads (issue #3).
#[derive(Debug, Clone)]
pub struct ApiScope {
    pub installation_id: u64,
    /// `None` = every repository in the installation; `Some` narrows further.
    pub repos: Option<Vec<String>>,
}

/// One usage metering row (issue #15).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRow {
    pub installation_id: u64,
    pub repo: String,
    pub familiar_id: String,
    /// Tasks accepted (every state).
    pub tasks: u32,
    pub completed: u32,
    pub failed: u32,
    /// Total attempt wall-clock runtime, seconds.
    pub runtime_secs: u64,
}

impl Store {
    /// Appends a task-API read to the audit trail (issue #3).
    pub async fn record_api_read(
        &self,
        caller: &str,
        scope: &str,
        action: &str,
        result: &str,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let (caller, scope, action, result) = (
            caller.to_string(),
            scope.to_string(),
            action.to_string(),
            result.to_string(),
        );
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            conn.execute(
                "INSERT INTO api_audit (at, caller, scope, action, result)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![now_rfc3339(), caller, scope, action, result],
            )?;
            Ok(())
        })
        .await
        .expect("store task panicked")
    }

    /// `(caller, scope, action, result)` audit rows, oldest first.
    pub async fn api_audit_entries(&self) -> Result<Vec<(String, String, String, String)>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let mut stmt =
                conn.prepare("SELECT caller, scope, action, result FROM api_audit ORDER BY id")?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .expect("store task panicked")
    }

    /// Number of tasks accepted for an installation since `cutoff`
    /// (RFC 3339) — the intake-side daily-cap check (issue #15).
    pub async fn tasks_created_since(&self, installation_id: u64, cutoff: &str) -> Result<u32> {
        let conn = self.conn.clone();
        let cutoff = cutoff.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let count: u32 = conn.query_row(
                "SELECT COUNT(*) FROM tasks
                 WHERE installation_id = ?1 AND created_at >= ?2",
                params![installation_id, cutoff],
                |row| row.get(0),
            )?;
            Ok(count)
        })
        .await
        .expect("store task panicked")
    }

    /// Usage metering rollup (issue #15): task counts and attempt runtime,
    /// grouped by installation, repository, and familiar. `scope` applies the
    /// same tenant boundary as the task list.
    pub async fn usage(&self, scope: Option<ApiScope>) -> Result<Vec<UsageRow>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT t.installation_id, t.repo, t.familiar_id,
                        COUNT(DISTINCT t.id),
                        SUM(CASE WHEN t.state = 'completed' THEN 1 ELSE 0 END),
                        SUM(CASE WHEN t.state = 'failed' THEN 1 ELSE 0 END),
                        COALESCE(SUM(
                          CASE WHEN a.ended_at IS NOT NULL
                            THEN MAX(0, CAST(strftime('%s', a.ended_at) AS INTEGER)
                                      - CAST(strftime('%s', a.started_at) AS INTEGER))
                            ELSE 0 END), 0)
                 FROM tasks t
                 LEFT JOIN task_attempts a ON a.task_id = t.id
                 WHERE (?1 IS NULL OR t.installation_id = ?1)
                 GROUP BY t.installation_id, t.repo, t.familiar_id
                 ORDER BY t.installation_id, t.repo, t.familiar_id",
            )?;
            let installation_filter = scope.as_ref().map(|s| s.installation_id);
            let rows = stmt
                .query_map(params![installation_filter], |row| {
                    Ok(UsageRow {
                        installation_id: row.get(0)?,
                        repo: row.get(1)?,
                        familiar_id: row.get(2)?,
                        tasks: row.get(3)?,
                        completed: row.get(4)?,
                        failed: row.get(5)?,
                        runtime_secs: row.get(6)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut rows = rows;
            if let Some(scope) = &scope {
                if let Some(repos) = &scope.repos {
                    rows.retain(|r| repos.contains(&r.repo));
                }
            }
            Ok(rows)
        })
        .await
        .expect("store task panicked")
    }

    /// Records memory activity for the inspect/audit trail (issue #6). Every
    /// read and write the runtime reported is recorded with the adapter's
    /// verdict, so a customer can later see what a familiar read from and
    /// attempted to write to their repository's memory.
    pub async fn record_memory_activity(&self, rows: Vec<MemoryActivity>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().expect("store mutex poisoned");
            let tx = conn.transaction()?;
            for row in &rows {
                // Honor an explicit timestamp when provided (tests/backfills);
                // otherwise stamp the current time.
                let at = if row.at.is_empty() {
                    now_rfc3339()
                } else {
                    row.at.clone()
                };
                tx.execute(
                    "INSERT INTO memory_activity
                       (at, installation_id, repo, task_id, op, target, scope, outcome)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        at,
                        row.installation_id,
                        row.repo,
                        row.task_id,
                        row.op,
                        row.target,
                        row.scope,
                        row.outcome,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("store task panicked")
    }

    /// Lists recorded memory activity, newest first, narrowed to the caller's
    /// [`ApiScope`] (issue #6). `None` scope (service/open callers) sees all.
    pub async fn list_memory(&self, scope: Option<ApiScope>) -> Result<Vec<MemoryActivity>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let installation_filter = scope.as_ref().map(|s| s.installation_id);
            let mut stmt = conn.prepare(
                "SELECT at, installation_id, repo, task_id, op, target, scope, outcome
                 FROM memory_activity
                 WHERE (?1 IS NULL OR installation_id = ?1)
                 ORDER BY id DESC",
            )?;
            let rows = stmt
                .query_map(params![installation_filter], |row| {
                    Ok(MemoryActivity {
                        at: row.get(0)?,
                        installation_id: row.get(1)?,
                        repo: row.get(2)?,
                        task_id: row.get(3)?,
                        op: row.get(4)?,
                        target: row.get(5)?,
                        scope: row.get(6)?,
                        outcome: row.get(7)?,
                    })
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            // Repository narrowing within the installation, when scoped.
            let filtered = rows
                .into_iter()
                .filter(|r| match scope.as_ref().and_then(|s| s.repos.as_ref()) {
                    Some(repos) => repos.contains(&r.repo),
                    None => true,
                })
                .collect();
            Ok(filtered)
        })
        .await
        .expect("store task panicked")
    }
}

impl Store {
    /// Records a memory revocation for an installation/repo (issue #6). The
    /// target is a key or prefix; the worker refuses matching reads/writes and
    /// forwards it to the runtime as a denial list.
    pub async fn record_revocation(
        &self,
        installation_id: u64,
        repo: &str,
        target: &str,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let (repo, target) = (repo.to_string(), target.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            conn.execute(
                "INSERT INTO memory_revocations (at, installation_id, repo, target)
                 VALUES (?1, ?2, ?3, ?4)",
                params![now_rfc3339(), installation_id, repo, target],
            )?;
            Ok(())
        })
        .await
        .expect("store task panicked")
    }

    /// Revoked targets for an installation/repo, for the policy denial list.
    pub async fn revocations_for(&self, installation_id: u64, repo: &str) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        let repo = repo.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let mut stmt = conn.prepare(
                "SELECT target FROM memory_revocations
                 WHERE installation_id = ?1 AND repo = ?2
                 ORDER BY id",
            )?;
            let rows = stmt
                .query_map(params![installation_id, repo], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .expect("store task panicked")
    }

    /// Purges all memory records for an installation (issue #6 delete-on-
    /// uninstall). Returns the number of activity + revocation rows removed.
    pub async fn delete_memory_for_installation(&self, installation_id: u64) -> Result<usize> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().expect("store mutex poisoned");
            let tx = conn.transaction()?;
            let a = tx.execute(
                "DELETE FROM memory_activity WHERE installation_id = ?1",
                params![installation_id],
            )?;
            let r = tx.execute(
                "DELETE FROM memory_revocations WHERE installation_id = ?1",
                params![installation_id],
            )?;
            tx.commit()?;
            Ok(a + r)
        })
        .await
        .expect("store task panicked")
    }

    /// Deletes memory-activity audit rows older than `cutoff` (RFC 3339) —
    /// retention expiry (issue #6). Revocations are active policy and are never
    /// expired: an expired revocation would silently un-revoke memory.
    pub async fn expire_memory_activity_before(&self, cutoff: &str) -> Result<usize> {
        let conn = self.conn.clone();
        let cutoff = cutoff.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().expect("store mutex poisoned");
            let n = conn.execute(
                "DELETE FROM memory_activity WHERE at < ?1",
                params![cutoff],
            )?;
            Ok(n)
        })
        .await
        .expect("store task panicked")
    }

    /// Deletes memory-activity audit rows older than `retention_days` from now.
    pub async fn expire_memory_activity(&self, retention_days: u32) -> Result<usize> {
        let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days as i64)).to_rfc3339();
        self.expire_memory_activity_before(&cutoff).await
    }
}

/// One recorded memory operation for the inspect/audit trail (issue #6).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MemoryActivity {
    /// RFC 3339 timestamp; set by the store on insert (ignored on the way in).
    pub at: String,
    pub installation_id: u64,
    pub repo: String,
    pub task_id: String,
    /// `read` or `write`.
    pub op: String,
    /// The memory entry id/key.
    pub target: String,
    /// Namespace: `repo` | `tenant_shared` | `familiar_global`.
    pub scope: String,
    /// Adapter verdict: `accepted` or `rejected:<reason>`.
    pub outcome: String,
}

/// Terminal transition applied by [`Store::finish`].
#[derive(Debug, Clone, Default)]
pub struct Terminal {
    pub state: TerminalState,
    /// Session result classification: `success` / `partial` / `failure` /
    /// `needs_input`. `None` for adapter-only outcomes (replies, declines).
    pub result_status: Option<String>,
    pub branch: Option<String>,
    pub pr_number: Option<u64>,
    pub summary: Option<String>,
    /// Attempt detail (already redacted by the caller).
    pub detail: Option<String>,
}

/// Durable terminal states a claimed task can reach.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TerminalState {
    #[default]
    Completed,
    Failed,
    /// The target moved or a newer event replaced this task (issues #8/#10).
    Superseded,
}

impl TerminalState {
    fn as_str(self) -> &'static str {
        match self {
            TerminalState::Completed => "completed",
            TerminalState::Failed => "failed",
            TerminalState::Superseded => "superseded",
        }
    }
}

/// Maps durable machine state (+ result classification) to the Cave status.
fn project_status(
    state: &str,
    result_status: Option<&str>,
    pr_number: Option<u64>,
) -> TaskListStatus {
    match state {
        "queued" => TaskListStatus::Queued,
        "running" => TaskListStatus::Running,
        "superseded" => TaskListStatus::Superseded,
        "failed" => TaskListStatus::Failed,
        // completed: a PR or an open question needs a human next.
        _ if result_status == Some("needs_input") || pr_number.is_some() => {
            TaskListStatus::Review
        }
        _ => TaskListStatus::Done,
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let version: i32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    if version < 1 {
        conn.execute_batch(
            r#"
            CREATE TABLE webhook_deliveries (
              delivery_id     TEXT PRIMARY KEY,
              event           TEXT NOT NULL,
              action          TEXT,
              installation_id INTEGER,
              repo            TEXT,
              payload_hash    TEXT NOT NULL,
              routing         TEXT NOT NULL,
              received_at     TEXT NOT NULL
            );

            CREATE TABLE tasks (
              id              TEXT PRIMARY KEY,
              delivery_id     TEXT REFERENCES webhook_deliveries(delivery_id),
              installation_id INTEGER NOT NULL,
              repo            TEXT NOT NULL,
              familiar_id     TEXT NOT NULL,
              kind            TEXT NOT NULL,
              commander       TEXT,
              state           TEXT NOT NULL,
              supersede_key   TEXT,
              attempts        INTEGER NOT NULL DEFAULT 0,
              branch          TEXT,
              pr_number       INTEGER,
              check_run_url   TEXT,
              summary         TEXT,
              created_at      TEXT NOT NULL,
              updated_at      TEXT NOT NULL
            );
            CREATE INDEX tasks_state_created ON tasks(state, created_at);
            CREATE INDEX tasks_supersede ON tasks(supersede_key)
              WHERE supersede_key IS NOT NULL;

            CREATE TABLE task_attempts (
              task_id    TEXT NOT NULL REFERENCES tasks(id),
              attempt    INTEGER NOT NULL,
              started_at TEXT NOT NULL,
              ended_at   TEXT,
              outcome    TEXT,
              detail     TEXT,
              PRIMARY KEY (task_id, attempt)
            );
            "#,
        )
        .context("failed to apply schema v1")?;
    }
    if version < 2 {
        // v2: terminal result classification for the Cave projection.
        conn.execute_batch("ALTER TABLE tasks ADD COLUMN result_status TEXT;")
            .context("failed to apply schema v2")?;
    }
    if version < 3 {
        // v3: task API read audit (issue #3).
        conn.execute_batch(
            r#"
            CREATE TABLE api_audit (
              id     INTEGER PRIMARY KEY AUTOINCREMENT,
              at     TEXT NOT NULL,
              caller TEXT NOT NULL,
              scope  TEXT NOT NULL,
              action TEXT NOT NULL,
              result TEXT NOT NULL
            );
            "#,
        )
        .context("failed to apply schema v3")?;
    }
    if version < 4 {
        // v4: memory governance audit for inspect (issue #6). Records every
        // memory read/write the runtime reported, with the adapter's verdict,
        // scoped to installation/repo so customers can inspect it.
        conn.execute_batch(
            r#"
            CREATE TABLE memory_activity (
              id              INTEGER PRIMARY KEY AUTOINCREMENT,
              at              TEXT NOT NULL,
              installation_id INTEGER NOT NULL,
              repo            TEXT NOT NULL,
              task_id         TEXT NOT NULL,
              op              TEXT NOT NULL,
              target          TEXT NOT NULL,
              scope           TEXT NOT NULL,
              outcome         TEXT NOT NULL
            );
            CREATE INDEX memory_activity_scope
              ON memory_activity(installation_id, repo);
            "#,
        )
        .context("failed to apply schema v4")?;
    }
    if version < 5 {
        // v5: memory revocations (issue #6). A revoked key/prefix is refused on
        // future reads/writes and passed to the runtime as a denial list.
        conn.execute_batch(
            r#"
            CREATE TABLE memory_revocations (
              id              INTEGER PRIMARY KEY AUTOINCREMENT,
              at              TEXT NOT NULL,
              installation_id INTEGER NOT NULL,
              repo            TEXT NOT NULL,
              target          TEXT NOT NULL
            );
            CREATE INDEX memory_revocations_scope
              ON memory_revocations(installation_id, repo);
            "#,
        )
        .context("failed to apply schema v5")?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn record_delivery_sync(
    conn: &mut Connection,
    delivery: &Delivery,
    routing_label: &str,
    task: Option<&Task>,
) -> Result<Recorded> {
    let now = now_rfc3339();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let inserted = tx.execute(
        "INSERT INTO webhook_deliveries
           (delivery_id, event, action, installation_id, repo, payload_hash, routing, received_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(delivery_id) DO NOTHING",
        params![
            delivery.delivery_id,
            delivery.event,
            delivery.action,
            delivery.installation_id,
            delivery.repo,
            delivery.payload_hash,
            routing_label,
            now,
        ],
    )?;
    if inserted == 0 {
        // Redelivery: the original record stands; nothing else may happen.
        tx.commit()?;
        return Ok(Recorded::Duplicate);
    }

    if let Some(task) = task {
        let repo = format!("{}/{}", task.repo_owner, task.repo_name);
        let supersede_key = supersede_key(task);
        // Only adapter-initiated (auto) reviews may tombstone at insert.
        // Command-initiated reviews carry a commander whose write access the
        // worker has not yet verified — they supersede older queued reviews
        // post-gate instead (issue #13), so a drive-by `review` comment can
        // never displace legitimate queued work.
        if task.commander.is_none() {
            if let Some(key) = &supersede_key {
                tx.execute(
                    "UPDATE tasks SET state = 'superseded', updated_at = ?1
                     WHERE supersede_key = ?2 AND state = 'queued'",
                    params![now, key],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO tasks
               (id, delivery_id, installation_id, repo, familiar_id, kind, commander,
                state, supersede_key, attempts, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, 0, ?9, ?9)",
            params![
                task.id,
                delivery.delivery_id,
                task.installation_id,
                repo,
                task.familiar_id,
                serde_json::to_string(&task.kind)?,
                task.commander,
                supersede_key,
                now,
            ],
        )?;
    }

    tx.commit()?;
    Ok(Recorded::New)
}

/// PR reviews supersede by target PR; other task kinds never supersede.
fn supersede_key(task: &Task) -> Option<String> {
    match &task.kind {
        TaskKind::ReviewPullRequest { pr_number, .. } => Some(format!(
            "{}/{}#{pr_number}",
            task.repo_owner, task.repo_name
        )),
        _ => None,
    }
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery(id: &str) -> Delivery {
        Delivery {
            delivery_id: id.to_string(),
            event: "issues".to_string(),
            action: Some("assigned".to_string()),
            installation_id: Some(1),
            repo: Some("OpenCoven/demo".to_string()),
            payload_hash: "abc123".to_string(),
        }
    }

    fn fix_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: "t".to_string(),
                issue_body: "b".to_string(),
            },
        }
    }

    fn memory_row(installation_id: u64, repo: &str, target: &str, outcome: &str) -> MemoryActivity {
        MemoryActivity {
            at: String::new(),
            installation_id,
            repo: repo.to_string(),
            task_id: "task-1".to_string(),
            op: "write".to_string(),
            target: target.to_string(),
            scope: "repo".to_string(),
            outcome: outcome.to_string(),
        }
    }

    #[tokio::test]
    async fn memory_activity_records_and_lists_newest_first_with_verdicts() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_memory_activity(vec![
                memory_row(1, "OpenCoven/demo", "repo/OpenCoven/demo/a", "accepted"),
                memory_row(1, "OpenCoven/demo", "repo/other/x", "rejected:out_of_scope"),
            ])
            .await
            .unwrap();

        let all = store.list_memory(None).await.unwrap();
        assert_eq!(all.len(), 2);
        // Newest first: the rejected row was inserted second.
        assert_eq!(all[0].outcome, "rejected:out_of_scope");
        assert!(!all[0].at.is_empty(), "the store stamps the timestamp");
    }

    #[tokio::test]
    async fn revocations_are_recorded_and_scoped() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_revocation(1, "OpenCoven/billing", "repo/OpenCoven/billing/secrets/")
            .await
            .unwrap();
        store
            .record_revocation(1, "OpenCoven/web", "repo/OpenCoven/web/x")
            .await
            .unwrap();

        let billing = store.revocations_for(1, "OpenCoven/billing").await.unwrap();
        assert_eq!(billing, vec!["repo/OpenCoven/billing/secrets/"]);
        // A different repo, and a different installation, are isolated.
        assert!(store.revocations_for(1, "OpenCoven/other").await.unwrap().is_empty());
        assert!(store.revocations_for(2, "OpenCoven/billing").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn retention_expiry_removes_old_activity_but_keeps_revocations() {
        let store = Store::open_in_memory().unwrap();
        // An old audit row (explicit timestamp) and a fresh one (empty → now).
        let mut old = memory_row(1, "OpenCoven/a", "repo/OpenCoven/a/old", "accepted");
        old.at = "2020-01-01T00:00:00+00:00".to_string();
        store.record_memory_activity(vec![old]).await.unwrap();
        store
            .record_memory_activity(vec![memory_row(1, "OpenCoven/a", "repo/OpenCoven/a/new", "accepted")])
            .await
            .unwrap();
        // A revocation must survive the sweep.
        store.record_revocation(1, "OpenCoven/a", "repo/OpenCoven/a/x").await.unwrap();

        let removed = store
            .expire_memory_activity_before("2021-01-01T00:00:00+00:00")
            .await
            .unwrap();
        assert_eq!(removed, 1, "only the 2020 row expires");

        let remaining = store.list_memory(None).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].target, "repo/OpenCoven/a/new");
        // Revocation is untouched — expiring it would un-revoke memory.
        assert_eq!(
            store.revocations_for(1, "OpenCoven/a").await.unwrap(),
            vec!["repo/OpenCoven/a/x"]
        );
    }

    #[tokio::test]
    async fn delete_on_uninstall_purges_activity_and_revocations() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_memory_activity(vec![memory_row(1, "OpenCoven/a", "repo/OpenCoven/a/x", "accepted")])
            .await
            .unwrap();
        store.record_revocation(1, "OpenCoven/a", "repo/OpenCoven/a/x").await.unwrap();
        // Another installation is untouched.
        store
            .record_memory_activity(vec![memory_row(2, "OpenCoven/b", "repo/OpenCoven/b/y", "accepted")])
            .await
            .unwrap();

        let removed = store.delete_memory_for_installation(1).await.unwrap();
        assert_eq!(removed, 2, "one activity + one revocation");
        assert!(store
            .list_memory(Some(ApiScope { installation_id: 1, repos: None }))
            .await
            .unwrap()
            .is_empty());
        assert!(store.revocations_for(1, "OpenCoven/a").await.unwrap().is_empty());
        // Installation 2 survives.
        assert_eq!(
            store
                .list_memory(Some(ApiScope { installation_id: 2, repos: None }))
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn list_memory_is_scoped_to_installation_and_repo() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_memory_activity(vec![
                memory_row(1, "OpenCoven/billing", "repo/OpenCoven/billing/a", "accepted"),
                memory_row(1, "OpenCoven/web", "repo/OpenCoven/web/b", "accepted"),
                memory_row(2, "OpenCoven/other", "repo/OpenCoven/other/c", "accepted"),
            ])
            .await
            .unwrap();

        // Tenant scoped to installation 1 never sees installation 2.
        let inst1 = store
            .list_memory(Some(ApiScope {
                installation_id: 1,
                repos: None,
            }))
            .await
            .unwrap();
        assert_eq!(inst1.len(), 2);
        assert!(inst1.iter().all(|r| r.installation_id == 1));

        // Narrowed further to a single repo.
        let one_repo = store
            .list_memory(Some(ApiScope {
                installation_id: 1,
                repos: Some(vec!["OpenCoven/billing".to_string()]),
            }))
            .await
            .unwrap();
        assert_eq!(one_repo.len(), 1);
        assert_eq!(one_repo[0].repo, "OpenCoven/billing");
    }

    fn review_task(id: &str, pr: u64) -> Task {
        Task {
            kind: TaskKind::ReviewPullRequest {
                pr_number: pr,
                pr_title: "t".to_string(),
                reason: "synchronize".to_string(),
            },
            ..fix_task(id)
        }
    }

    #[tokio::test]
    async fn migrations_are_idempotent_across_reopen() {
        let dir = std::env::temp_dir().join(format!("coven-store-{}", uuid::Uuid::new_v4()));
        let path = dir.join("store.db");
        {
            let store = Store::open(&path).expect("first open");
            store
                .record_delivery(delivery("d1"), Routing::Task(&fix_task("t1")))
                .await
                .expect("record");
        }
        // Reopen: migrations must not re-run or destroy data.
        let store = Store::open(&path).expect("reopen");
        assert_eq!(
            store.delivery_routing("d1").await.unwrap().as_deref(),
            Some("task:t1")
        );
        assert_eq!(
            store.task_states().await.unwrap(),
            vec![("t1".to_string(), "queued".to_string())]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn duplicate_delivery_records_nothing_new() {
        let store = Store::open_in_memory().expect("open");
        let first = store
            .record_delivery(delivery("d1"), Routing::Task(&fix_task("t1")))
            .await
            .expect("first");
        assert_eq!(first, Recorded::New);

        // GitHub redelivery: same delivery id, would-be second task.
        let second = store
            .record_delivery(delivery("d1"), Routing::Task(&fix_task("t2")))
            .await
            .expect("second");
        assert_eq!(second, Recorded::Duplicate);

        // One task row; the original routing stands.
        assert_eq!(store.task_states().await.unwrap().len(), 1);
        assert_eq!(
            store.delivery_routing("d1").await.unwrap().as_deref(),
            Some("task:t1")
        );
    }

    #[tokio::test]
    async fn ignored_routing_is_recorded_without_a_task() {
        let store = Store::open_in_memory().expect("open");
        store
            .record_delivery(delivery("d-ping"), Routing::Ignored("ping"))
            .await
            .expect("record");
        assert_eq!(
            store.delivery_routing("d-ping").await.unwrap().as_deref(),
            Some("ignored:ping")
        );
        assert!(store.task_states().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn newer_review_tombstones_queued_review_of_same_pr() {
        let store = Store::open_in_memory().expect("open");
        store
            .record_delivery(delivery("d1"), Routing::Task(&review_task("r1", 88)))
            .await
            .expect("first review");
        // Different PR: untouched by supersession.
        store
            .record_delivery(delivery("d2"), Routing::Task(&review_task("other", 89)))
            .await
            .expect("other pr");
        store
            .record_delivery(delivery("d3"), Routing::Task(&review_task("r2", 88)))
            .await
            .expect("second review");

        let states = store.task_states().await.unwrap();
        let state_of = |id: &str| {
            states
                .iter()
                .find(|(task, _)| task == id)
                .map(|(_, state)| state.as_str())
                .expect("task present")
        };
        assert_eq!(state_of("r1"), "superseded");
        assert_eq!(state_of("other"), "queued");
        assert_eq!(state_of("r2"), "queued");
    }

    #[tokio::test]
    async fn unknown_future_schema_version_is_left_alone() {
        let store = Store::open_in_memory().expect("open");
        // Simulate a database from a newer adapter.
        {
            let conn = store.conn.lock().unwrap();
            conn.pragma_update(None, "user_version", 999).unwrap();
        }
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
        migrate(&conn).expect("newer schema must not be downgraded");
        let version: i32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 999);
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    fn delivery(id: &str) -> Delivery {
        Delivery {
            delivery_id: id.to_string(),
            event: "issues".to_string(),
            action: Some("assigned".to_string()),
            installation_id: Some(1),
            repo: Some("OpenCoven/demo".to_string()),
            payload_hash: "h".to_string(),
        }
    }

    fn fix_task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: Some("octocat".to_string()),
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "b".to_string(),
            },
        }
    }

    fn review_task(id: &str, pr: u64) -> Task {
        Task {
            kind: TaskKind::ReviewPullRequest {
                pr_number: pr,
                pr_title: "t".to_string(),
                reason: "synchronize".to_string(),
            },
            commander: None,
            ..fix_task(id)
        }
    }

    async fn enqueue(store: &Store, delivery_id: &str, task: &Task) {
        store
            .record_delivery(delivery(delivery_id), Routing::Task(task))
            .await
            .expect("enqueue");
    }

    #[tokio::test]
    async fn claims_are_fifo_and_reconstruct_the_task() {
        let store = Store::open_in_memory().expect("open");
        enqueue(&store, "d1", &fix_task("t1")).await;
        enqueue(&store, "d2", &fix_task("t2")).await;

        let first = store.claim_next(&Default::default()).await.unwrap().expect("first claim");
        assert_eq!(first.id, "t1");
        assert_eq!(first.repo_owner, "OpenCoven");
        assert_eq!(first.repo_name, "demo");
        assert_eq!(first.commander.as_deref(), Some("octocat"));
        assert!(matches!(
            first.kind,
            TaskKind::FixIssue { issue_number: 42, .. }
        ));

        let second = store.claim_next(&Default::default()).await.unwrap().expect("second claim");
        assert_eq!(second.id, "t2");
        assert!(store.claim_next(&Default::default()).await.unwrap().is_none(), "queue drained");
    }

    #[tokio::test]
    async fn superseded_rows_are_never_claimed() {
        let store = Store::open_in_memory().expect("open");
        enqueue(&store, "d1", &review_task("old", 88)).await;
        enqueue(&store, "d2", &review_task("new", 88)).await;

        let claimed = store.claim_next(&Default::default()).await.unwrap().expect("claim");
        assert_eq!(claimed.id, "new", "the tombstoned review must be skipped");
        assert!(store.claim_next(&Default::default()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn finish_reaches_terminal_state_and_closes_the_attempt() {
        let store = Store::open_in_memory().expect("open");
        enqueue(&store, "d1", &fix_task("t1")).await;
        let task = store.claim_next(&Default::default()).await.unwrap().expect("claim");
        store
            .finish(
                &task.id,
                Terminal {
                    state: TerminalState::Completed,
                    result_status: Some("success".to_string()),
                    branch: Some("cody/fix-42".to_string()),
                    pr_number: Some(9),
                    summary: Some("done".to_string()),
                    detail: None,
                },
            )
            .await
            .expect("finish");

        let items = store
            .cave_list(HashMap::from([("cody".to_string(), "Cody".to_string())]), None)
            .await
            .expect("list");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, TaskListStatus::Review);
        assert_eq!(items[0].pr_number, Some(9));
        assert_eq!(
            items[0].pr_url.as_deref(),
            Some("https://github.com/OpenCoven/demo/pull/9")
        );
        assert_eq!(items[0].familiar_name, "Cody");
        assert_eq!(items[0].issue_title, "Fix auth");
    }

    #[tokio::test]
    async fn restart_recovery_requeues_or_fails_by_attempt_budget() {
        let store = Store::open_in_memory().expect("open");

        // "spent" burns three claim attempts across simulated crashes.
        enqueue(&store, "d1", &fix_task("spent")).await;
        for _ in 0..2 {
            let claimed = store.claim_next(&Default::default()).await.unwrap().expect("claim spent");
            assert_eq!(claimed.id, "spent");
            store.recover_interrupted(99).await.expect("interim recovery");
        }
        let claimed = store.claim_next(&Default::default()).await.unwrap().expect("third claim");
        assert_eq!(claimed.id, "spent");

        // "fresh" is claimed once; both are mid-run when the process dies.
        enqueue(&store, "d2", &fix_task("fresh")).await;
        let claimed = store.claim_next(&Default::default()).await.unwrap().expect("claim fresh");
        assert_eq!(claimed.id, "fresh");

        // Boot with a budget of 3 claims: "spent" fails, "fresh" requeues.
        let (requeued, failed) = store.recover_interrupted(3).await.expect("recovery");
        assert_eq!((requeued, failed), (1, 1));
        let states: HashMap<String, String> =
            store.task_states().await.unwrap().into_iter().collect();
        assert_eq!(states["spent"], "failed");
        assert_eq!(states["fresh"], "queued");

        // The requeued task is claimable again; the failed one is not.
        let reclaimed = store.claim_next(&Default::default()).await.unwrap().expect("re-claim");
        assert_eq!(reclaimed.id, "fresh");
        assert!(store.claim_next(&Default::default()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancel_queued_tombstones_only_queued_rows_for_the_key() {
        let store = Store::open_in_memory().expect("open");
        enqueue(&store, "d1", &review_task("running", 88)).await;
        let claimed = store.claim_next(&Default::default()).await.unwrap().expect("claim");
        assert_eq!(claimed.id, "running");
        enqueue(&store, "d2", &review_task("queued", 88)).await;
        enqueue(&store, "d3", &review_task("other", 89)).await;

        let n = store.cancel_queued("OpenCoven/demo#88").await.expect("cancel");
        assert_eq!(n, 1, "only the queued row for PR 88 is cancellable");

        let states: HashMap<String, String> =
            store.task_states().await.unwrap().into_iter().collect();
        assert_eq!(states["running"], "running", "in-flight work is untouched");
        assert_eq!(states["queued"], "superseded");
        assert_eq!(states["other"], "queued");
    }

    #[tokio::test]
    async fn cave_list_hides_command_replies_and_orders_newest_first() {
        let store = Store::open_in_memory().expect("open");
        let reply = Task {
            kind: TaskKind::CommandReply {
                issue_number: 42,
                body: "Status: done".to_string(),
            },
            commander: None,
            ..fix_task("reply")
        };
        enqueue(&store, "d1", &fix_task("work")).await;
        enqueue(&store, "d2", &reply).await;

        let items = store.cave_list(HashMap::new(), None).await.expect("list");
        assert_eq!(items.len(), 1, "adapter replies are not Cave tasks");
        assert_eq!(items[0].id, "work");
        assert_eq!(items[0].status, TaskListStatus::Queued);
    }
}

#[cfg(test)]
mod command_gate_tests {
    //! Insert-time supersession is an auto-review privilege; command reviews
    //! wait for the worker's write-access gate (issue #13).
    use super::*;

    fn delivery(id: &str) -> Delivery {
        Delivery {
            delivery_id: id.to_string(),
            event: "pull_request".to_string(),
            action: Some("synchronize".to_string()),
            installation_id: Some(1),
            repo: Some("OpenCoven/demo".to_string()),
            payload_hash: "h".to_string(),
        }
    }

    fn review(id: &str, commander: Option<&str>) -> Task {
        Task {
            id: id.to_string(),
            installation_id: 1,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "demo".to_string(),
            familiar_id: "cody".to_string(),
            commander: commander.map(str::to_string),
            kind: TaskKind::ReviewPullRequest {
                pr_number: 88,
                pr_title: "t".to_string(),
                reason: "synchronize".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn command_review_does_not_tombstone_at_insert() {
        let store = Store::open_in_memory().expect("open");
        store
            .record_delivery(delivery("d1"), Routing::Task(&review("auto", None)))
            .await
            .expect("auto review");
        // An unverified commander's review must not displace queued work.
        store
            .record_delivery(
                delivery("d2"),
                Routing::Task(&review("commanded", Some("drive-by"))),
            )
            .await
            .expect("commanded review");

        let states: HashMap<String, String> =
            store.task_states().await.unwrap().into_iter().collect();
        assert_eq!(states["auto"], "queued", "insert-time tombstone is auto-only");
        assert_eq!(states["commanded"], "queued");
    }

    #[tokio::test]
    async fn post_gate_supersession_spares_the_current_task() {
        let store = Store::open_in_memory().expect("open");
        store
            .record_delivery(delivery("d1"), Routing::Task(&review("older", None)))
            .await
            .expect("older review");
        store
            .record_delivery(
                delivery("d2"),
                Routing::Task(&review("commanded", Some("octocat"))),
            )
            .await
            .expect("commanded review");

        // The worker calls this once the commander passed the write gate.
        let n = store
            .supersede_queued_except("OpenCoven/demo#88", "commanded")
            .await
            .expect("supersede");
        assert_eq!(n, 1);

        let states: HashMap<String, String> =
            store.task_states().await.unwrap().into_iter().collect();
        assert_eq!(states["older"], "superseded");
        assert_eq!(states["commanded"], "queued", "the commanding task survives");
    }
}

#[cfg(test)]
mod metering_tests {
    //! Usage metering and limit enforcement (issue #15).
    use super::*;

    fn delivery(id: &str) -> Delivery {
        Delivery {
            delivery_id: id.to_string(),
            event: "issues".to_string(),
            action: Some("assigned".to_string()),
            installation_id: Some(1),
            repo: Some("OpenCoven/demo".to_string()),
            payload_hash: "h".to_string(),
        }
    }

    fn task_for(id: &str, installation_id: u64) -> Task {
        Task {
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
        }
    }

    async fn enqueue(store: &Store, delivery_id: &str, task: &Task) {
        store
            .record_delivery(delivery(delivery_id), Routing::Task(task))
            .await
            .expect("enqueue");
    }

    #[tokio::test]
    async fn concurrency_cap_skips_saturated_installations_without_starving_others() {
        let store = Store::open_in_memory().expect("open");
        enqueue(&store, "d1", &task_for("cap-a", 1)).await;
        enqueue(&store, "d2", &task_for("cap-b", 1)).await;
        enqueue(&store, "d3", &task_for("other", 2)).await;

        let caps = std::collections::HashMap::from([(1u64, 1u32)]);
        let first = store.claim_next(&caps).await.unwrap().expect("first claim");
        assert_eq!(first.id, "cap-a");

        // Installation 1 is at its cap: its second task is skipped, but
        // installation 2's task still claims.
        let second = store.claim_next(&caps).await.unwrap().expect("second");
        assert_eq!(second.id, "other", "capped installation must be skipped");
        assert!(store.claim_next(&caps).await.unwrap().is_none());

        // Finishing frees capacity.
        store
            .finish(
                "cap-a",
                Terminal {
                    state: TerminalState::Completed,
                    ..Terminal::default()
                },
            )
            .await
            .expect("finish");
        let third = store.claim_next(&caps).await.unwrap().expect("third");
        assert_eq!(third.id, "cap-b");
    }

    #[tokio::test]
    async fn tasks_created_since_counts_only_the_installation() {
        let store = Store::open_in_memory().expect("open");
        enqueue(&store, "d1", &task_for("a", 1)).await;
        enqueue(&store, "d2", &task_for("b", 1)).await;
        enqueue(&store, "d3", &task_for("c", 2)).await;

        let past = "2000-01-01T00:00:00+00:00";
        assert_eq!(store.tasks_created_since(1, past).await.unwrap(), 2);
        assert_eq!(store.tasks_created_since(2, past).await.unwrap(), 1);
        let future = "2999-01-01T00:00:00+00:00";
        assert_eq!(store.tasks_created_since(1, future).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn usage_rolls_up_by_installation_repo_and_familiar_with_scope() {
        let store = Store::open_in_memory().expect("open");
        enqueue(&store, "d1", &task_for("a", 1)).await;
        enqueue(&store, "d2", &task_for("b", 2)).await;
        // Claim + finish task a so it has runtime and a terminal state.
        let claimed = store
            .claim_next(&Default::default())
            .await
            .unwrap()
            .expect("claim");
        assert_eq!(claimed.id, "a");
        store
            .finish(
                "a",
                Terminal {
                    state: TerminalState::Completed,
                    result_status: Some("success".to_string()),
                    ..Terminal::default()
                },
            )
            .await
            .expect("finish");

        let all = store.usage(None).await.expect("usage");
        assert_eq!(all.len(), 2);
        let one = all.iter().find(|r| r.installation_id == 1).expect("row");
        assert_eq!(one.repo, "OpenCoven/demo");
        assert_eq!(one.familiar_id, "cody");
        assert_eq!((one.tasks, one.completed, one.failed), (1, 1, 0));

        // The tenant boundary applies to metering exactly as to tasks.
        let scoped = store
            .usage(Some(ApiScope {
                installation_id: 2,
                repos: None,
            }))
            .await
            .expect("scoped usage");
        assert_eq!(scoped.len(), 1);
        assert_eq!(scoped[0].installation_id, 2);
        assert_eq!(scoped[0].completed, 0);
    }
}
