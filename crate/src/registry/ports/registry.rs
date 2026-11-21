use std::collections::HashSet;

use dcl_contents::errors::ApiError;
use dcl_contents::registry::{async_trait, WorldPolicy};
use serde::Serialize;
use sqlx::{PgPool, Row};

#[derive(Debug, Clone, Serialize)]
pub struct DenylistEntry {
    pub entity_id: String,
    pub reason: Option<String>,
    pub created_by: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone)]
pub struct BuildJob {
    pub entity_id: String,
    pub platform: String,
    pub claimed_at: i64,
}

#[derive(Clone, Default)]
pub struct RegistryStore {
    pub(super) pool: Option<PgPool>,
}

impl RegistryStore {
    pub fn new(pool: Option<PgPool>) -> Self {
        Self { pool }
    }

    pub fn enabled(&self) -> bool {
        self.pool.is_some()
    }

    pub async fn denylist_rows(&self) -> Result<Vec<DenylistEntry>, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Ok(Vec::new());
        };
        let rows = sqlx::query(
            "SELECT entity_id, reason, created_by, created_at, updated_at \
             FROM denylist ORDER BY created_at DESC",
        )
        .fetch_all(pool)
        .await?;
        Ok(rows.into_iter().map(row_to_entry).collect())
    }

    pub async fn add_to_denylist(
        &self,
        entity_id: &str,
        created_by: &str,
        reason: Option<&str>,
    ) -> Result<DenylistEntry, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let now = now_ms();
        let row = sqlx::query(
            "INSERT INTO denylist (entity_id, reason, created_by, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $4) \
             ON CONFLICT (entity_id) DO UPDATE \
               SET reason = EXCLUDED.reason, updated_at = EXCLUDED.updated_at \
             RETURNING entity_id, reason, created_by, created_at, updated_at",
        )
        .bind(entity_id.to_lowercase())
        .bind(reason)
        .bind(created_by.to_lowercase())
        .bind(now)
        .fetch_one(pool)
        .await?;
        Ok(row_to_entry(row))
    }

    pub async fn remove_from_denylist(&self, entity_id: &str) -> Result<bool, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let res = sqlx::query("DELETE FROM denylist WHERE lower(entity_id) = $1")
            .bind(entity_id.to_lowercase())
            .execute(pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn queue_paused(&self) -> Result<bool, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Ok(false);
        };
        let row = sqlx::query("SELECT paused FROM queue_control WHERE id = 1")
            .fetch_optional(pool)
            .await?;
        Ok(row.map(|r| r.get::<bool, _>("paused")).unwrap_or(false))
    }

    pub async fn set_queue_paused(
        &self,
        paused: bool,
        updated_by: &str,
    ) -> Result<bool, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let now = now_ms();
        sqlx::query(
            "INSERT INTO queue_control (id, paused, updated_by, updated_at) \
             VALUES (1, $1, $2, $3) \
             ON CONFLICT (id) DO UPDATE \
               SET paused = EXCLUDED.paused, \
                   updated_by = EXCLUDED.updated_by, \
                   updated_at = EXCLUDED.updated_at",
        )
        .bind(paused)
        .bind(updated_by)
        .bind(now)
        .execute(pool)
        .await?;
        Ok(paused)
    }

    pub async fn record_retry(
        &self,
        entity_id: &str,
        requested_by: &str,
    ) -> Result<i32, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let now = now_ms();
        let row = sqlx::query(
            "INSERT INTO build_retries (entity_id, requested_by, requested_at, attempts) \
             VALUES ($1, $2, $3, 1) \
             ON CONFLICT (entity_id) DO UPDATE \
               SET requested_by = EXCLUDED.requested_by, \
                   requested_at = EXCLUDED.requested_at, \
                   attempts = build_retries.attempts + 1 \
             RETURNING attempts",
        )
        .bind(entity_id.to_lowercase())
        .bind(requested_by)
        .bind(now)
        .fetch_one(pool)
        .await?;
        Ok(row.get::<i32, _>("attempts"))
    }

    pub const BUILD_PLATFORMS: [&'static str; 4] = ["windows", "mac", "webgl", "linux"];

    pub async fn enqueue_build(
        &self,
        entity_id: &str,
        requested_by: &str,
    ) -> Result<Vec<String>, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let now = now_ms();
        let id = entity_id.to_lowercase();
        let mut enqueued = Vec::with_capacity(Self::BUILD_PLATFORMS.len());
        for platform in Self::BUILD_PLATFORMS {
            sqlx::query(
                "INSERT INTO build_jobs (entity_id, platform, status, requested_by, enqueued_at, updated_at) \
                 VALUES ($1, $2, 'pending', $3, $4, $4) \
                 ON CONFLICT (entity_id, platform) DO UPDATE \
                   SET status = 'pending', \
                       requested_by = EXCLUDED.requested_by, \
                       enqueued_at = EXCLUDED.enqueued_at, \
                       updated_at = GREATEST(build_jobs.updated_at + 1, EXCLUDED.updated_at)",
            )
            .bind(&id)
            .bind(platform)
            .bind(requested_by)
            .bind(now)
            .execute(pool)
            .await?;
            enqueued.push(platform.to_string());
        }
        Ok(enqueued)
    }

    pub async fn claim_build_job(&self) -> Result<Option<BuildJob>, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let now = now_ms();
        let row = sqlx::query(
            "WITH next_job AS (SELECT entity_id, platform FROM build_jobs \
             WHERE (status = 'pending' OR (status = 'running' AND updated_at < $1)) \
             AND NOT EXISTS (SELECT 1 FROM queue_control WHERE id = 1 AND paused) \
             ORDER BY enqueued_at, entity_id, platform FOR UPDATE SKIP LOCKED LIMIT 1) \
             UPDATE build_jobs j SET status = 'running', updated_at = GREATEST(j.updated_at + 1, $2) \
             FROM next_job n WHERE j.entity_id = n.entity_id AND j.platform = n.platform \
             RETURNING j.entity_id, j.platform, j.updated_at",
        ).bind(now - 300_000).bind(now).fetch_optional(pool).await?;
        Ok(row.map(|row| BuildJob {
            entity_id: row.get("entity_id"),
            platform: row.get("platform"),
            claimed_at: row.get("updated_at"),
        }))
    }

    pub async fn renew_build_job(&self, job: &mut BuildJob) -> Result<bool, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let next = sqlx::query_scalar::<_, i64>(
            "UPDATE build_jobs SET updated_at = GREATEST(updated_at + 1, $4) \
             WHERE entity_id = $1 AND platform = $2 AND status = 'running' AND updated_at = $3 \
             RETURNING updated_at",
        )
        .bind(&job.entity_id)
        .bind(&job.platform)
        .bind(job.claimed_at)
        .bind(now_ms())
        .fetch_optional(pool)
        .await?;
        if let Some(next) = next {
            job.claimed_at = next;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn finish_build_job(
        &self,
        job: &BuildJob,
        succeeded: bool,
    ) -> Result<bool, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Err(disabled());
        };
        let result = sqlx::query(
            "UPDATE build_jobs SET status = $4, updated_at = GREATEST(updated_at + 1, $5) \
             WHERE entity_id = $1 AND platform = $2 AND status = 'running' AND updated_at = $3",
        )
        .bind(&job.entity_id)
        .bind(&job.platform)
        .bind(job.claimed_at)
        .bind(if succeeded { "complete" } else { "failed" })
        .bind(now_ms())
        .execute(pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub async fn pending_jobs_for(&self, platform: &str) -> Result<HashSet<String>, sqlx::Error> {
        let Some(pool) = &self.pool else {
            return Ok(HashSet::new());
        };
        let rows = sqlx::query(
            "SELECT entity_id FROM build_jobs WHERE status IN ('pending', 'running') AND platform = $1",
        )
        .bind(platform)
        .fetch_all(pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| r.get::<String, _>("entity_id"))
            .collect())
    }
}

#[async_trait]
impl WorldPolicy for RegistryStore {
    async fn denylist(&self) -> Result<HashSet<String>, ApiError> {
        Ok(self
            .denylist_rows()
            .await?
            .into_iter()
            .map(|row| row.entity_id.to_lowercase())
            .collect())
    }

    async fn spawn_override(&self, world_name: &str) -> Result<Option<(i64, i64)>, ApiError> {
        let Some(pool) = &self.pool else {
            return Ok(None);
        };
        Ok(sqlx::query_as::<_, (i64, i64)>(
            "SELECT x, y FROM world_spawn_coordinates WHERE lower(world_name) = $1",
        )
        .bind(world_name.to_lowercase())
        .fetch_optional(pool)
        .await?)
    }
}

fn disabled() -> sqlx::Error {
    sqlx::Error::Configuration(
        "ab_registry DB not configured (set AB_REGISTRY_PG_CONNECTION_STRING)".into(),
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn row_to_entry(r: sqlx::postgres::PgRow) -> DenylistEntry {
    DenylistEntry {
        entity_id: r.get("entity_id"),
        reason: r.try_get::<Option<String>, _>("reason").unwrap_or(None),
        created_by: r.try_get::<Option<String>, _>("created_by").unwrap_or(None),
        created_at: r.try_get("created_at").unwrap_or(0),
        updated_at: r.try_get("updated_at").unwrap_or(0),
    }
}

#[cfg(test)]
mod native_contract_tests {
    use super::*;
    use axum::{extract::State, http::HeaderMap, Json};
    use dcl_contents::{
        content::ContentComponent, manifest_store::AbManifestStore, types::BuildStatus,
    };
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
    use std::sync::Arc;

    #[tokio::test]
    #[ignore = "requires CATALYST_TEST_DATABASE_URL; run explicitly with --ignored"]
    async fn policy_and_queue_reads_use_durable_state_and_flush_preserves_it() {
        let url = std::env::var("CATALYST_TEST_DATABASE_URL")
            .expect("CATALYST_TEST_DATABASE_URL is required for this integration test");
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("registry_native_{}_{suffix}", std::process::id());
        let control = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&control)
            .await
            .unwrap();
        let options = url
            .parse::<PgConnectOptions>()
            .unwrap()
            .options([("search_path", schema.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        for ddl in [
            include_str!("../../../migrations/0001_ab_registry.sql"),
            include_str!("../../../migrations/0002_queue_control.sql"),
            include_str!("../../../migrations/0003_build_jobs.sql"),
        ] {
            sqlx::raw_sql(ddl).execute(&pool).await.unwrap();
        }
        let store = RegistryStore::new(Some(pool.clone()));
        store
            .add_to_denylist("BafkBlocked", "moderator", None)
            .await
            .unwrap();
        assert_eq!(
            store.denylist().await.unwrap(),
            HashSet::from(["bafkblocked".to_string()])
        );
        sqlx::query("INSERT INTO world_spawn_coordinates (world_name, x, y) VALUES ('MiXeD.dcl.eth', -4, 7)")
            .execute(&pool).await.unwrap();
        assert_eq!(
            store.spawn_override("MIXED.dcl.eth").await.unwrap(),
            Some((-4, 7))
        );
        assert_eq!(store.spawn_override("unknown.dcl.eth").await.unwrap(), None);

        // There is deliberately no Catalyst deployments table: pending queues
        // must come from jobs, including more than the old 2,000-scene scan cap.
        sqlx::query("INSERT INTO build_jobs (entity_id, platform, status) SELECT 'entity-' || n, 'windows', 'pending' FROM generate_series(1, 2001) AS n")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO build_jobs (entity_id, platform, status) VALUES ('mac-only', 'mac', 'pending'), ('finished', 'windows', 'complete'), ('failed', 'windows', 'failed'), ('linux-only', 'linux', 'pending')")
            .execute(&pool).await.unwrap();
        let root = std::env::temp_dir().join(&schema);
        let entity_id = "bafkflushcachefixture";
        let manifest_dir = root.join(entity_id);
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(
            manifest_dir.join("windows.manifest.json"),
            r#"{"exitCode":0}"#,
        )
        .unwrap();
        let manifests = AbManifestStore::new(&root);
        assert_eq!(
            manifests.get(entity_id).await.windows_status(),
            BuildStatus::Complete
        );
        std::fs::remove_file(manifest_dir.join("windows.manifest.json")).unwrap();
        let state = Arc::new(crate::registry::AppStateInner {
            content: Arc::new(ContentComponent::new(pool.clone())),
            manifests: manifests.clone(),
            registry: store,
            admin_token: Some("test-native-admin".into()),
            denylist_moderators: vec![],
        });
        let Json(before) =
            crate::registry::handlers::queues::get_queues_status(State(state.clone()))
                .await
                .unwrap();
        assert_eq!(before.windows_pending_jobs.len(), 2001);
        assert_eq!(before.mac_pending_jobs, vec!["mac-only"]);
        let wire = serde_json::to_value(&before).unwrap();
        assert_eq!(wire.as_object().unwrap().len(), 2);
        assert!(!before
            .windows_pending_jobs
            .iter()
            .any(|id| id == "finished" || id == "failed"));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-native-admin".parse().unwrap());
        let Json(flushed) =
            crate::registry::handlers::admin::flush_cache(State(state.clone()), headers)
                .await
                .unwrap();
        assert_eq!(
            flushed,
            serde_json::json!({"ok": true, "message": "Cache flushed"})
        );
        let Json(after) =
            crate::registry::handlers::queues::get_queues_status(State(state.clone()))
                .await
                .unwrap();
        assert_eq!(serde_json::to_value(after).unwrap(), wire);
        assert_eq!(
            manifests.get(entity_id).await.windows_status(),
            BuildStatus::Complete
        );
        let count: (i64,) = sqlx::query_as("SELECT count(*) FROM build_jobs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count.0, 2005);
        drop(state);
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&control)
            .await
            .unwrap();
        control.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
