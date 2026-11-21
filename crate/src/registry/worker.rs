use std::future::Future;
use std::time::Duration;

use super::ports::registry::{BuildJob, RegistryStore};

/// One sequential consumer per runtime. Conversion runs privately; only a still
/// current claim/generation can publish durable facts. No staged bytes are served.
pub fn spawn_worker(
    state: crate::abcdn::state::AppState,
    registry: super::AppState,
) -> Option<tokio::task::JoinHandle<()>> {
    if !registry.registry.enabled() {
        return None;
    }
    Some(tokio::spawn(async move {
        loop {
            let run_state = state.clone();
            let run_registry = registry.clone();
            let publisher = registry.registry.clone();
            let result =
                process_one_with_publisher(
                    &registry.registry,
                    move |job| async move { convert_job(&run_state, &run_registry, &job).await },
                    move |job, outcome| async move {
                        publish_completion(&publisher, job, outcome).await
                    },
                )
                .await;
            match result {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "registry queue processing failed"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }))
}

#[derive(Clone)]
struct Completion {
    entity: dcl_contents::types::ActiveEntity,
    attempt: super::ports::records::ConversionAttempt,
    bundles: dcl_contents::types::Bundles,
    versions: dcl_contents::types::Versions,
}

async fn publish_completion(
    store: &RegistryStore,
    job: BuildJob,
    outcome: anyhow::Result<Completion>,
) -> Result<(), sqlx::Error> {
    match outcome {
        Ok(completion) => {
            // DB failure leaves the running receipt recoverable. A superseded
            // generation retires only this still-owned receipt; a requeued or
            // reclaimed receipt fails the token comparison and remains untouched.
            if store
                .complete_conversion(
                    &completion.entity,
                    &completion.attempt,
                    Some(&job),
                    completion.bundles,
                    completion.versions,
                )
                .await?
                .is_none()
            {
                store.finish_build_job(&job, false).await?;
            }
        }
        Err(error) => {
            tracing::warn!(%error, entity_id = %job.entity_id, "registry conversion setup failed");
            store.finish_build_job(&job, false).await?;
        }
    }
    Ok(())
}

async fn convert_job(
    state: &crate::abcdn::state::AppState,
    registry: &super::AppState,
    job: &BuildJob,
) -> anyhow::Result<Completion> {
    let proxy = state
        .live_proxy
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("local converter unavailable"))?;
    let attempt = registry
        .registry
        .begin_conversion(&job.entity_id, &job.platform)
        .await?;
    let entity = registry
        .content
        .resolve_one(&job.entity_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("converted entity missing from Catalyst"))?;
    let (mut bundles, versions) = match super::conversion::convert(
        proxy.clone(),
        job.entity_id.clone(),
        job.platform.clone(),
        state.manifest_content_server_url.clone(),
    )
    .await
    {
        Ok(facts) => facts,
        Err(error) => {
            tracing::warn!(%error, entity_id = %job.entity_id, "private Registry conversion failed");
            (
                dcl_contents::types::Bundles::default(),
                dcl_contents::types::Versions::default(),
            )
        }
    };
    let status = match job.platform.as_str() {
        "mac" => &mut bundles.assets.mac,
        "windows" => &mut bundles.assets.windows,
        "webgl" => &mut bundles.assets.webgl,
        "linux" => &mut bundles.assets.linux,
        _ => anyhow::bail!("unsupported Registry conversion platform"),
    };
    let succeeded = *status == dcl_contents::types::BuildStatus::Complete;
    if !succeeded {
        *status = dcl_contents::types::BuildStatus::Failed;
    }
    Ok(Completion {
        entity,
        attempt,
        bundles,
        versions,
    })
}

async fn process_one_with_publisher<F, Fut, T, P, PubFut>(
    store: &RegistryStore,
    execute: F,
    publish: P,
) -> Result<bool, sqlx::Error>
where
    F: FnOnce(BuildJob) -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
    P: FnOnce(BuildJob, anyhow::Result<T>) -> PubFut,
    PubFut: Future<Output = Result<(), sqlx::Error>>,
{
    let Some(mut job) = store.claim_build_job().await? else {
        return Ok(false);
    };
    let conversion = execute(job.clone());
    tokio::pin!(conversion);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.tick().await;
    let outcome = loop {
        tokio::select! {
            outcome = &mut conversion => break outcome,
            _ = heartbeat.tick() => {
                if !store.renew_build_job(&mut job).await? {
                    // Dropping this future cannot publish: the blocking converter
                    // owns only private staging, and cleans it when it finishes.
                    return Ok(true);
                }
            }
        }
    };
    publish(job, outcome).await?;
    Ok(true)
}

#[cfg(test)]
async fn process_one<F, Fut>(store: &RegistryStore, execute: F) -> Result<bool, sqlx::Error>
where
    F: FnOnce(BuildJob) -> Fut,
    Fut: Future<Output = anyhow::Result<()>>,
{
    process_one_with_publisher(store, execute, |job, outcome| async move {
        store.finish_build_job(&job, outcome.is_ok()).await?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    #[tokio::test]
    #[ignore = "requires CATALYST_TEST_DATABASE_URL; run explicitly with --ignored"]
    async fn durable_worker_claims_finishes_retries_and_recovers_abandoned_jobs() {
        let url = std::env::var("CATALYST_TEST_DATABASE_URL")
            .expect("CATALYST_TEST_DATABASE_URL is required for this integration test");
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("registry_worker_{}_{suffix}", std::process::id());
        let control = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&control)
            .await
            .unwrap();
        let opts = url
            .parse::<PgConnectOptions>()
            .unwrap()
            .options([("search_path", schema.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(3)
            .connect_with(opts)
            .await
            .unwrap();
        for ddl in [
            include_str!("../../migrations/0001_ab_registry.sql"),
            include_str!("../../migrations/0002_queue_control.sql"),
            include_str!("../../migrations/0003_build_jobs.sql"),
            include_str!("../../migrations/0004_registry_records.sql"),
            include_str!("../../migrations/0005_registry_conversion_attempts.sql"),
        ] {
            sqlx::raw_sql(ddl).execute(&pool).await.unwrap();
        }
        let store = RegistryStore::new(Some(pool.clone()));
        store.enqueue_build("entity", "test").await.unwrap();
        store.set_queue_paused(true, "test").await.unwrap();
        assert!(
            !process_one(&store, |_| async { panic!("paused job must not execute") })
                .await
                .unwrap()
        );
        store.set_queue_paused(false, "test").await.unwrap();
        let mut executed = 0;
        while process_one(&store, |job| async move {
            if job.platform == "linux" {
                anyhow::bail!("fake converter failure");
            }
            Ok(())
        })
        .await
        .unwrap()
        {
            executed += 1;
        }
        assert_eq!(executed, 4);
        let statuses: Vec<(String, String)> =
            sqlx::query_as("SELECT platform, status FROM build_jobs ORDER BY platform")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(
            statuses,
            vec![
                ("linux".into(), "failed".into()),
                ("mac".into(), "complete".into()),
                ("webgl".into(), "complete".into()),
                ("windows".into(), "complete".into()),
            ]
        );
        assert!(store.pending_jobs_for("windows").await.unwrap().is_empty());

        store.enqueue_build("entity", "retry").await.unwrap();
        let mut claimed = store.claim_build_job().await.unwrap().unwrap();
        let old_claim = claimed.clone();
        assert!(store
            .pending_jobs_for(&claimed.platform)
            .await
            .unwrap()
            .contains("entity"));
        assert!(store.renew_build_job(&mut claimed).await.unwrap());
        assert!(claimed.claimed_at > old_claim.claimed_at);
        assert!(!store.finish_build_job(&old_claim, true).await.unwrap());
        // A retry fences the previous running execution even within one millisecond.
        store.enqueue_build("entity", "retry-again").await.unwrap();
        assert!(!store.finish_build_job(&claimed, true).await.unwrap());
        let (first, second) = tokio::join!(store.claim_build_job(), store.claim_build_job());
        let mut first = first.unwrap().unwrap();
        let second = second.unwrap().unwrap();
        assert_ne!(first.platform, second.platform);
        assert!(store.finish_build_job(&second, true).await.unwrap());
        sqlx::query("UPDATE build_jobs SET status = 'complete' WHERE status = 'pending'")
            .execute(&pool)
            .await
            .unwrap();
        // Model a claim made before the lease window in both the worker's
        // receipt and the stored row. Changing only the row to zero would
        // fabricate a future worker token that a same-millisecond claim could
        // coincidentally reuse; that does not model expiry of an actual lease.
        first.claimed_at -= 300_001;
        sqlx::query("UPDATE build_jobs SET updated_at = $3 WHERE entity_id = $1 AND platform = $2")
            .bind(&first.entity_id)
            .bind(&first.platform)
            .bind(first.claimed_at)
            .execute(&pool)
            .await
            .unwrap();
        let recovered = store.claim_build_job().await.unwrap().unwrap();
        assert_eq!(recovered.platform, first.platform);
        assert!(recovered.claimed_at > first.claimed_at);
        assert!(!store.finish_build_job(&first, true).await.unwrap());
        assert!(store.finish_build_job(&recovered, true).await.unwrap());
        assert!(store.claim_build_job().await.unwrap().is_none());
        // A newer HTTP attempt supersedes this worker generation without
        // revoking its queue receipt. Retire that receipt immediately, while
        // preserving a genuinely retried receipt in the following case.
        store.enqueue_build("superseded", "worker").await.unwrap();
        let owned = store.claim_build_job().await.unwrap().unwrap();
        let worker_attempt = store
            .begin_conversion(&owned.entity_id, &owned.platform)
            .await
            .unwrap();
        let completion = Completion {
            entity: dcl_contents::types::ActiveEntity {
                deployment_id: 0,
                entity_id: owned.entity_id.clone(),
                entity_type: "scene".into(),
                timestamp: 1,
                pointers: vec!["0,0".into()],
                metadata: serde_json::json!({}),
                deployer_address: None,
                content: vec![],
            },
            attempt: worker_attempt,
            bundles: dcl_contents::types::Bundles::default(),
            versions: dcl_contents::types::Versions::default(),
        };
        store
            .begin_conversion(&owned.entity_id, &owned.platform)
            .await
            .unwrap();
        publish_completion(&store, owned.clone(), Ok(completion.clone()))
            .await
            .unwrap();
        let status: String = sqlx::query_scalar(
            "SELECT status FROM build_jobs WHERE entity_id = $1 AND platform = $2",
        )
        .bind(&owned.entity_id)
        .bind(&owned.platform)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            status, "failed",
            "superseded still-owned receipt must not linger as running"
        );
        assert!(store.get_record(&owned.entity_id).await.unwrap().is_none());
        store.enqueue_build("superseded", "retry").await.unwrap();
        let revoked = store.claim_build_job().await.unwrap().unwrap();
        let mut retry_completion = completion;
        retry_completion.attempt = store
            .begin_conversion(&revoked.entity_id, &revoked.platform)
            .await
            .unwrap();
        store
            .enqueue_build("superseded", "new-owner")
            .await
            .unwrap();
        publish_completion(&store, revoked.clone(), Ok(retry_completion))
            .await
            .unwrap();
        let status: String = sqlx::query_scalar(
            "SELECT status FROM build_jobs WHERE entity_id = $1 AND platform = $2",
        )
        .bind(&revoked.entity_id)
        .bind(&revoked.platform)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            status, "pending",
            "retiring a stale receipt must not change the new owner's job"
        );

        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&control)
            .await
            .unwrap();
        control.close().await;
    }
}
