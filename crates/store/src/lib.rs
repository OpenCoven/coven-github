//! Durable adapter state: webhook deliveries, tasks, and attempt records
//! (issue #2). Design: `docs/durable-task-store.md`.
//!
//! Embedded SQLite via rusqlite. One writer connection behind a mutex, WAL
//! journal, forward-only migrations tracked by `PRAGMA user_version`. All
//! async entry points hop to `spawn_blocking`; SQLite work never blocks the
//! runtime.

use anyhow::{Context, Result};
use coven_github_api::{Task, TaskKind};
use rusqlite::{params, Connection, TransactionBehavior};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Current schema version, stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i32 = 1;

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
        if let Some(key) = &supersede_key {
            // A newer review of the same PR supersedes anything still queued.
            tx.execute(
                "UPDATE tasks SET state = 'superseded', updated_at = ?1
                 WHERE supersede_key = ?2 AND state = 'queued'",
                params![now, key],
            )?;
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
