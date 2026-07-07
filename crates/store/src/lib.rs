//! Durable task store for coven-github (issue #2).
//!
//! Embedded SQLite (WAL) behind one writer connection. Phase 1 (see
//! `docs/durable-task-store.md`) records webhook deliveries for idempotency and
//! persists accepted tasks in `queued` state before the adapter acknowledges
//! GitHub. The durable claim loop, restart recovery, and supersession move into
//! the store in Phase 2; this crate deliberately exposes only what Phase 1 uses
//! plus the schema those later phases build on.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use coven_github_api::Task;
use rusqlite::Connection;

/// Current schema version. Bumped by each forward-only migration.
const SCHEMA_VERSION: i64 = 1;

/// Durable store handle. Cheap to clone (shares one guarded connection).
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Whether a delivery was newly recorded or had already been seen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// First time this `X-GitHub-Delivery` was recorded — proceed with routing.
    New,
    /// Already recorded — a redelivery; the caller must not re-run work.
    Duplicate,
}

/// Routing coordinates of a webhook delivery, minus the payload body (only a
/// hash is retained — see `docs/security.md`).
#[derive(Debug, Clone)]
pub struct DeliveryRecord {
    pub delivery_id: String,
    pub event: String,
    pub action: Option<String>,
    pub installation_id: Option<i64>,
    pub repo: Option<String>,
    pub payload_hash: String,
}

impl Store {
    /// Opens (creating if needed) the SQLite database at `path`, applies
    /// pragmas, and runs migrations. The parent directory is created if absent.
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("failed to create store directory {}", parent.display())
                })?;
            }
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open store at {}", path.display()))?;
        Self::from_connection(conn)
    }

    /// Opens a private in-memory database — used by tests.
    pub fn open_in_memory() -> Result<Store> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Store> {
        // WAL: concurrent readers with a single writer, matching the adapter's
        // shape (webhook writes, worker claims, Cave reads). NORMAL sync is
        // durable across app crashes under WAL; busy_timeout absorbs the brief
        // writer contention between the webhook and worker.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn)?;
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    async fn with_conn<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("store connection mutex poisoned");
            f(&guard)
        })
        .await
        .context("store task panicked")?
    }

    /// Records a webhook delivery, keyed by its id. Returns [`DeliveryOutcome`]:
    /// `New` on first sight, `Duplicate` if the id was already recorded. The
    /// insert-or-ignore is the idempotency gate — a redelivered webhook yields
    /// `Duplicate` and the caller skips re-running work.
    pub async fn record_delivery(&self, record: DeliveryRecord) -> Result<DeliveryOutcome> {
        self.with_conn(move |conn| {
            let changed = conn.execute(
                "INSERT INTO webhook_deliveries \
                   (delivery_id, event, action, installation_id, repo, payload_hash, routing, received_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'received', ?7) \
                 ON CONFLICT(delivery_id) DO NOTHING",
                rusqlite::params![
                    record.delivery_id,
                    record.event,
                    record.action,
                    record.installation_id,
                    record.repo,
                    record.payload_hash,
                    now_rfc3339(),
                ],
            )?;
            Ok(if changed == 1 {
                DeliveryOutcome::New
            } else {
                DeliveryOutcome::Duplicate
            })
        })
        .await
    }

    /// Updates a delivery's routing outcome (`task:<id>` or `ignored:<reason>`).
    pub async fn set_delivery_routing(&self, delivery_id: &str, routing: &str) -> Result<()> {
        let delivery_id = delivery_id.to_string();
        let routing = routing.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "UPDATE webhook_deliveries SET routing = ?2 WHERE delivery_id = ?1",
                rusqlite::params![delivery_id, routing],
            )?;
            Ok(())
        })
        .await
    }

    /// Persists an accepted task in `queued` state, linked to the delivery that
    /// produced it. `supersede_key` (`owner/repo#pr` for PR reviews) is recorded
    /// for the Phase 2 claim loop; Phase 1 does not yet tombstone on insert.
    pub async fn insert_task(
        &self,
        task: &Task,
        delivery_id: &str,
        supersede_key: Option<String>,
    ) -> Result<()> {
        let id = task.id.clone();
        let delivery_id = delivery_id.to_string();
        let installation_id = task.installation_id as i64;
        let repo = format!("{}/{}", task.repo_owner, task.repo_name);
        let familiar_id = task.familiar_id.clone();
        let kind = serde_json::to_string(&task.kind).context("failed to serialize task kind")?;
        let commander = task.commander.clone();
        self.with_conn(move |conn| {
            let ts = now_rfc3339();
            conn.execute(
                "INSERT INTO tasks \
                   (id, delivery_id, installation_id, repo, familiar_id, kind, commander, \
                    state, supersede_key, attempts, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'queued', ?8, 0, ?9, ?9)",
                rusqlite::params![
                    id,
                    delivery_id,
                    installation_id,
                    repo,
                    familiar_id,
                    kind,
                    commander,
                    supersede_key,
                    ts,
                ],
            )?;
            Ok(())
        })
        .await
    }

    /// Removes a delivery record, releasing its idempotency claim. Used to
    /// compensate when task persistence fails after the delivery was recorded,
    /// so GitHub's redelivery re-processes the event instead of being deduped
    /// against a claim with no task behind it.
    pub async fn delete_delivery(&self, delivery_id: &str) -> Result<()> {
        let delivery_id = delivery_id.to_string();
        self.with_conn(move |conn| {
            conn.execute(
                "DELETE FROM webhook_deliveries WHERE delivery_id = ?1",
                [delivery_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Number of persisted tasks. Test/observability helper.
    pub async fn count_tasks(&self) -> Result<i64> {
        self.with_conn(|conn| {
            Ok(conn.query_row("SELECT COUNT(*) FROM tasks", [], |row| row.get(0))?)
        })
        .await
    }

    /// The recorded routing for a delivery, or `None` if unknown. Test helper.
    pub async fn delivery_routing(&self, delivery_id: &str) -> Result<Option<String>> {
        let delivery_id = delivery_id.to_string();
        self.with_conn(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT routing FROM webhook_deliveries WHERE delivery_id = ?1",
                    [delivery_id],
                    |row| row.get(0),
                )
                .ok())
        })
        .await
    }
}

/// Applies forward-only migrations up to [`SCHEMA_VERSION`], tracked by
/// `PRAGMA user_version`.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        conn.execute_batch(
            "CREATE TABLE webhook_deliveries (
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
             );",
        )?;
    }
    conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;
    use coven_github_api::TaskKind;

    fn delivery(id: &str) -> DeliveryRecord {
        DeliveryRecord {
            delivery_id: id.to_string(),
            event: "issues".to_string(),
            action: Some("assigned".to_string()),
            installation_id: Some(123),
            repo: Some("OpenCoven/coven-code".to_string()),
            payload_hash: "abc123".to_string(),
        }
    }

    fn task(id: &str) -> Task {
        Task {
            id: id.to_string(),
            installation_id: 123,
            repo_owner: "OpenCoven".to_string(),
            repo_name: "coven-code".to_string(),
            familiar_id: "cody".to_string(),
            commander: None,
            kind: TaskKind::FixIssue {
                issue_number: 42,
                issue_title: "Fix auth".to_string(),
                issue_body: "Body".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn first_delivery_is_new_and_replay_is_duplicate() {
        let store = Store::open_in_memory().unwrap();

        assert_eq!(
            store.record_delivery(delivery("d-1")).await.unwrap(),
            DeliveryOutcome::New
        );
        // Same id again — the idempotency gate.
        assert_eq!(
            store.record_delivery(delivery("d-1")).await.unwrap(),
            DeliveryOutcome::Duplicate
        );
        // A different id is independent.
        assert_eq!(
            store.record_delivery(delivery("d-2")).await.unwrap(),
            DeliveryOutcome::New
        );
    }

    #[tokio::test]
    async fn routing_is_recorded_and_updatable() {
        let store = Store::open_in_memory().unwrap();
        store.record_delivery(delivery("d-1")).await.unwrap();
        assert_eq!(
            store.delivery_routing("d-1").await.unwrap().as_deref(),
            Some("received")
        );

        store.set_delivery_routing("d-1", "task:t-1").await.unwrap();
        assert_eq!(
            store.delivery_routing("d-1").await.unwrap().as_deref(),
            Some("task:t-1")
        );
    }

    #[tokio::test]
    async fn insert_task_persists_a_queued_row() {
        let store = Store::open_in_memory().unwrap();
        store.record_delivery(delivery("d-1")).await.unwrap();

        assert_eq!(store.count_tasks().await.unwrap(), 0);
        store.insert_task(&task("t-1"), "d-1", None).await.unwrap();
        assert_eq!(store.count_tasks().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn schema_survives_reopening_the_same_file() {
        let path = std::env::temp_dir().join(format!("coven-store-{}.db", uuid::Uuid::new_v4()));

        {
            let store = Store::open(&path).unwrap();
            store.record_delivery(delivery("d-1")).await.unwrap();
            store.insert_task(&task("t-1"), "d-1", None).await.unwrap();
        }
        // Reopen the same file: migrations are idempotent, data persists.
        {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.count_tasks().await.unwrap(), 1);
            assert_eq!(
                store.record_delivery(delivery("d-1")).await.unwrap(),
                DeliveryOutcome::Duplicate,
                "a delivery recorded before restart must still dedupe after"
            );
        }

        let _ = std::fs::remove_file(&path);
    }
}
