//! Scheduled Branch Gardener task enqueueing.

use anyhow::Result;
use coven_github_api::{Task, TaskKind};
use coven_github_config::{Config, InstallationConfig};
use coven_github_gardener::parse_schedule;
use coven_github_store::{Delivery, Recorded, Routing, Store};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub async fn run(config: Arc<Config>, store: Store, notify: Arc<tokio::sync::Notify>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        ticker.tick().await;
        if let Err(error) =
            enqueue_due_gardener_runs(&config, &store, &notify, current_unix_minutes()).await
        {
            tracing::error!("scheduled branch gardener enqueue failed: {error:#}");
        }
    }
}

fn current_unix_minutes() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_div(60) as i64
}

async fn enqueue_due_gardener_runs(
    config: &Config,
    store: &Store,
    notify: &tokio::sync::Notify,
    unix_minutes: i64,
) -> Result<usize> {
    let mut enqueued = 0;
    for installation in &config.installations {
        let Some(familiar_id) = scheduled_familiar_id(config, installation) else {
            continue;
        };
        for repo in scheduled_repo_candidates(config, installation) {
            if !repo_matches_installation_account(&repo, installation) {
                continue;
            }
            let policy = config.gardener_policy(&repo);
            if !policy.enabled {
                continue;
            }
            let schedule = match parse_schedule(&policy.schedule) {
                Ok(schedule) => schedule,
                Err(error) => {
                    tracing::warn!(
                        installation_id = installation.id,
                        repo,
                        "skipping branch gardener schedule with invalid cron: {error}"
                    );
                    continue;
                }
            };
            let Some(slot_id) = schedule.slot_id(unix_minutes) else {
                continue;
            };
            let Some((repo_owner, repo_name)) = repo.split_once('/') else {
                tracing::warn!(
                    installation_id = installation.id,
                    repo,
                    "skipping branch gardener schedule with invalid repo key"
                );
                continue;
            };
            let task = Task {
                id: format!(
                    "garden-{}-{}-{}",
                    installation.id,
                    sanitize_id(&repo),
                    sanitize_id(&slot_id)
                ),
                installation_id: installation.id,
                repo_owner: repo_owner.to_string(),
                repo_name: repo_name.to_string(),
                kind: TaskKind::GardenRun { report_issue: None },
                familiar_id: familiar_id.clone(),
                commander: None,
            };
            let recorded = store
                .record_delivery(
                    Delivery {
                        delivery_id: format!(
                            "schedule:garden:{}:{repo}:{slot_id}",
                            installation.id
                        ),
                        event: "schedule".to_string(),
                        action: Some("gardener".to_string()),
                        installation_id: Some(installation.id),
                        repo: Some(repo),
                        payload_hash: format!("garden:{unix_minutes}"),
                    },
                    Routing::Task(&task),
                )
                .await?;
            if recorded == Recorded::New {
                enqueued += 1;
                notify.notify_one();
            }
        }
    }
    Ok(enqueued)
}

fn scheduled_repo_candidates(
    config: &Config,
    installation: &InstallationConfig,
) -> BTreeSet<String> {
    config
        .gardener
        .repos
        .keys()
        .chain(installation.repos.keys())
        .cloned()
        .collect()
}

fn scheduled_familiar_id(config: &Config, installation: &InstallationConfig) -> Option<String> {
    if installation.familiars.is_empty() {
        return config.familiars.first().map(|familiar| familiar.id.clone());
    }
    installation
        .familiars
        .iter()
        .find(|id| config.familiars.iter().any(|familiar| familiar.id == **id))
        .cloned()
}

fn repo_matches_installation_account(repo: &str, installation: &InstallationConfig) -> bool {
    let Some(account) = &installation.account else {
        return true;
    };
    repo.split_once('/')
        .map(|(owner, _)| owner == account)
        .unwrap_or(false)
}

fn sanitize_id(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::enqueue_due_gardener_runs;
    use coven_github_api::TaskKind;
    use coven_github_config::{
        ApiConfig, Config, ContainerConfig, FamiliarConfig, GardenerConfig, GitHubAppConfig,
        InstallationConfig, InstallationLimits, MemoryConfig, RepoGardenerOverride, ReviewConfig,
        ServerConfig, StorageConfig, TriggerPolicy, WorkerBackendKind, WorkerConfig,
    };
    use coven_github_store::Store;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn config() -> Arc<Config> {
        let mut repos = HashMap::new();
        repos.insert(
            "OpenCoven/demo".to_string(),
            RepoGardenerOverride {
                enabled: Some(true),
                autonomy: Some("propose".to_string()),
                schedule: Some("15 5 * * *".to_string()),
                exclude: None,
                draft_pr_label: None,
            },
        );
        repos.insert(
            "OtherOrg/skip".to_string(),
            RepoGardenerOverride {
                enabled: Some(true),
                autonomy: Some("propose".to_string()),
                schedule: Some("15 5 * * *".to_string()),
                exclude: None,
                draft_pr_label: None,
            },
        );

        Arc::new(Config {
            server: ServerConfig {
                bind: "127.0.0.1:0".to_string(),
                cave_base_url: None,
            },
            github: GitHubAppConfig {
                app_id: 1,
                private_key_path: PathBuf::from("unused.pem"),
                webhook_secret: "a-long-random-webhook-secret".to_string(),
                api_base_url: None,
            },
            worker: WorkerConfig {
                concurrency: 1,
                coven_code_bin: PathBuf::from("coven-code"),
                workspace_root: PathBuf::from("."),
                timeout_secs: 600,
                max_retries: 2,
                backend: WorkerBackendKind::Host,
                container: ContainerConfig::default(),
                allow_host_backend: true,
            },
            familiars: vec![
                FamiliarConfig {
                    id: "cody".to_string(),
                    display_name: "Cody".to_string(),
                    bot_username: "coven-cody[bot]".to_string(),
                    model: None,
                    skills: Vec::new(),
                    trigger_labels: Vec::new(),
                },
                FamiliarConfig {
                    id: "nova".to_string(),
                    display_name: "Nova".to_string(),
                    bot_username: "coven-nova[bot]".to_string(),
                    model: None,
                    skills: Vec::new(),
                    trigger_labels: Vec::new(),
                },
            ],
            review: ReviewConfig::default(),
            storage: StorageConfig::default(),
            memory: MemoryConfig::default(),
            gardener: GardenerConfig {
                enabled: false,
                autonomy: "propose".to_string(),
                schedule: "0 4 * * *".to_string(),
                exclude: Vec::new(),
                draft_pr_label: None,
                repos,
            },
            api: ApiConfig::default(),
            installations: vec![InstallationConfig {
                id: 42,
                account: Some("OpenCoven".to_string()),
                familiars: vec!["nova".to_string()],
                triggers: TriggerPolicy::default(),
                limits: InstallationLimits::default(),
                repos: HashMap::new(),
            }],
        })
    }

    #[tokio::test]
    async fn scheduled_tick_enqueues_one_garden_run_per_due_repo_and_slot() {
        let store = Store::open_in_memory().expect("store");
        let notify = Arc::new(tokio::sync::Notify::new());
        let minute = 5 * 60 + 15;

        let enqueued = enqueue_due_gardener_runs(&config(), &store, &notify, minute)
            .await
            .expect("enqueue scheduled runs");

        assert_eq!(enqueued, 1);
        let task = store
            .claim_next(&HashMap::new())
            .await
            .expect("claim")
            .expect("scheduled task");
        assert_eq!(task.installation_id, 42);
        assert_eq!(task.repo_owner, "OpenCoven");
        assert_eq!(task.repo_name, "demo");
        assert_eq!(task.familiar_id, "nova");
        assert_eq!(task.commander, None);
        assert!(matches!(
            task.kind,
            TaskKind::GardenRun { report_issue: None }
        ));
    }

    #[tokio::test]
    async fn scheduled_tick_is_idempotent_for_the_same_slot() {
        let store = Store::open_in_memory().expect("store");
        let notify = Arc::new(tokio::sync::Notify::new());
        let minute = 2 * 1440 + 5 * 60 + 15;

        let first = enqueue_due_gardener_runs(&config(), &store, &notify, minute)
            .await
            .expect("first enqueue");
        let second = enqueue_due_gardener_runs(&config(), &store, &notify, minute)
            .await
            .expect("second enqueue");

        assert_eq!(first, 1);
        assert_eq!(second, 0);
        assert_eq!(store.task_states().await.expect("states").len(), 1);
    }

    #[tokio::test]
    async fn scheduled_tick_skips_repos_before_their_schedule() {
        let store = Store::open_in_memory().expect("store");
        let notify = Arc::new(tokio::sync::Notify::new());

        let enqueued = enqueue_due_gardener_runs(&config(), &store, &notify, 5 * 60 + 14)
            .await
            .expect("not due");

        assert_eq!(enqueued, 0);
        assert!(store.task_states().await.expect("states").is_empty());
    }
}
