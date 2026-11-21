use dcl_contents::types::{
    ActiveEntity, BuildStatus, BundlePlatformStatuses, Bundles, PlatformVersion, RegistryStatus,
    Versions,
};
use serde::Serialize;
use sqlx::{postgres::PgRow, types::Json, Row};

use super::registry::RegistryStore;

#[derive(Debug, Clone)]
pub struct ConversionAttempt {
    pub entity_id: String,
    pub platform: String,
    pub generation: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegistryRecord {
    pub entity_id: String,
    pub world_name: String,
    pub timestamp: i64,
    pub pointers: Vec<String>,
    pub status: RegistryStatus,
    pub bundles: Bundles,
    pub versions: Versions,
}

fn disabled() -> sqlx::Error {
    sqlx::Error::Configuration(
        "registry persistence requires AB_REGISTRY_PG_CONNECTION_STRING".into(),
    )
}

fn row_to_record(row: PgRow) -> Result<RegistryRecord, sqlx::Error> {
    let status: String = row.try_get("status")?;
    Ok(RegistryRecord {
        entity_id: row.try_get("entity_id")?,
        world_name: row.try_get("world_name")?,
        timestamp: row.try_get("entity_timestamp")?,
        pointers: row.try_get("pointers")?,
        status: serde_json::from_value(serde_json::Value::String(status))
            .map_err(|err| sqlx::Error::Decode(Box::new(err)))?,
        bundles: row.try_get::<Json<Bundles>, _>("bundles")?.0,
        versions: row.try_get::<Json<Versions>, _>("versions")?.0,
    })
}

fn status_name(status: RegistryStatus) -> &'static str {
    match status {
        RegistryStatus::Complete => "complete",
        RegistryStatus::Fallback => "fallback",
        RegistryStatus::Pending => "pending",
        RegistryStatus::Failed => "failed",
        RegistryStatus::Obsolete => "obsolete",
    }
}

fn preserve_known_conversion(
    bundles: &mut Bundles,
    versions: &mut Versions,
    previous: &RegistryRecord,
    authoritative_platform: Option<&str>,
) {
    fn authoritative(platform: &str, selected: Option<&str>) -> bool {
        selected == Some("*") || selected == Some(platform)
    }
    fn statuses(
        next: &mut BundlePlatformStatuses,
        old: &BundlePlatformStatuses,
        selected: Option<&str>,
    ) {
        for (platform, next, old) in [
            ("windows", &mut next.windows, old.windows),
            ("mac", &mut next.mac, old.mac),
            ("webgl", &mut next.webgl, old.webgl),
            ("linux", &mut next.linux, old.linux),
        ] {
            if *next == BuildStatus::Pending
                || (old != BuildStatus::Pending && !authoritative(platform, selected))
            {
                *next = old;
            }
        }
    }
    fn version(next: &mut PlatformVersion, old: &PlatformVersion, preserve: bool) {
        if preserve || next.version.is_empty() {
            next.version.clone_from(&old.version);
        }
        if preserve || next.build_date.is_empty() {
            next.build_date.clone_from(&old.build_date);
        }
    }
    statuses(
        &mut bundles.assets,
        &previous.bundles.assets,
        authoritative_platform,
    );
    match (&mut bundles.lods, &previous.bundles.lods) {
        (Some(next), Some(old)) => {
            statuses(next, old, authoritative_platform.filter(|p| *p == "*"))
        }
        (next @ None, Some(old)) => *next = Some(old.clone()),
        _ => {}
    }
    for (platform, next, old, old_status) in [
        (
            "windows",
            &mut versions.assets.windows,
            &previous.versions.assets.windows,
            previous.bundles.assets.windows,
        ),
        (
            "mac",
            &mut versions.assets.mac,
            &previous.versions.assets.mac,
            previous.bundles.assets.mac,
        ),
        (
            "webgl",
            &mut versions.assets.webgl,
            &previous.versions.assets.webgl,
            previous.bundles.assets.webgl,
        ),
        (
            "linux",
            &mut versions.assets.linux,
            &previous.versions.assets.linux,
            previous.bundles.assets.linux,
        ),
    ] {
        version(
            next,
            old,
            old_status != BuildStatus::Pending && !authoritative(platform, authoritative_platform),
        );
    }
}

fn rotation<'a>(
    timestamp: i64,
    bundles: &Bundles,
    related: &'a [RegistryRecord],
) -> (RegistryStatus, Option<(&'a str, RegistryStatus)>) {
    let status = if related
        .iter()
        .any(|record| record.timestamp > timestamp && record.status.is_servable())
    {
        RegistryStatus::Obsolete
    } else if bundles.assets.mac == BuildStatus::Failed
        || bundles.assets.windows == BuildStatus::Failed
    {
        RegistryStatus::Failed
    } else if bundles.assets.mac == BuildStatus::Complete
        && bundles.assets.windows == BuildStatus::Complete
    {
        RegistryStatus::Complete
    } else {
        RegistryStatus::Pending
    };

    let fallback = related
        .iter()
        .filter(|record| record.timestamp < timestamp && record.status.is_servable())
        .max_by_key(|record| record.timestamp)
        .map(|record| {
            let next = if matches!(status, RegistryStatus::Complete | RegistryStatus::Obsolete) {
                RegistryStatus::Obsolete
            } else {
                RegistryStatus::Fallback
            };
            (record.entity_id.as_str(), next)
        });
    (status, fallback)
}

impl RegistryStore {
    pub async fn begin_conversion(
        &self,
        entity_id: &str,
        platform: &str,
    ) -> Result<ConversionAttempt, sqlx::Error> {
        let generation = sqlx::query_scalar(
            "INSERT INTO registry_conversion_attempts(entity_id, platform, generation) VALUES ($1, $2, 1) \
             ON CONFLICT (entity_id, platform) DO UPDATE SET generation = registry_conversion_attempts.generation + 1 \
             RETURNING generation"
        ).bind(entity_id).bind(platform).fetch_one(self.pool.as_ref().ok_or_else(disabled)?).await?;
        Ok(ConversionAttempt {
            entity_id: entity_id.into(),
            platform: platform.into(),
            generation,
        })
    }

    /// Generation ownership, durable platform facts and optional queue completion
    /// commit together. None means a newer attempt or retry owns publication.
    pub async fn complete_conversion(
        &self,
        entity: &ActiveEntity,
        attempt: &ConversionAttempt,
        job: Option<&super::registry::BuildJob>,
        bundles: Bundles,
        versions: Versions,
    ) -> Result<Option<RegistryRecord>, sqlx::Error> {
        if entity.entity_id != attempt.entity_id {
            return Err(sqlx::Error::Protocol("conversion entity mismatch".into()));
        }
        let mut attempted_bundles = Bundles::default();
        let mut attempted_versions = Versions::default();
        let (status, version, target_status, target_version) = match attempt.platform.as_str() {
            "mac" => (
                bundles.assets.mac,
                &versions.assets.mac,
                &mut attempted_bundles.assets.mac,
                &mut attempted_versions.assets.mac,
            ),
            "windows" => (
                bundles.assets.windows,
                &versions.assets.windows,
                &mut attempted_bundles.assets.windows,
                &mut attempted_versions.assets.windows,
            ),
            "webgl" => (
                bundles.assets.webgl,
                &versions.assets.webgl,
                &mut attempted_bundles.assets.webgl,
                &mut attempted_versions.assets.webgl,
            ),
            "linux" => (
                bundles.assets.linux,
                &versions.assets.linux,
                &mut attempted_bundles.assets.linux,
                &mut attempted_versions.assets.linux,
            ),
            _ => {
                return Err(sqlx::Error::Protocol(
                    "unsupported conversion platform".into(),
                ))
            }
        };
        *target_status = status;
        *target_version = version.clone();
        let succeeded = status == BuildStatus::Complete;
        let bundles = attempted_bundles;
        let versions = attempted_versions;
        match self
            .persist_with(
                entity,
                Some(&attempt.platform),
                Some((attempt, job, succeeded)),
                || async { (bundles, versions) },
            )
            .await
        {
            Ok(record) => Ok(Some(record)),
            Err(sqlx::Error::RowNotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Observe derived files only after taking the entity/pointer transaction locks.
    /// A refresh waiting behind a newer conversion cannot commit an older snapshot.
    pub async fn refresh_and_rotate(
        &self,
        entity: &ActiveEntity,
        manifests: &dcl_contents::manifest_store::AbManifestStore,
    ) -> Result<RegistryRecord, sqlx::Error> {
        self.persist_with(entity, None, None, || async {
            let snapshot = manifests.refresh(&entity.entity_id).await;
            let mut bundles = snapshot.bundles(false);
            bundles.lods.get_or_insert_with(Default::default);
            (bundles, snapshot.versions())
        })
        .await
    }

    /// Publish only the platform this converter actually attempted. Other platform
    /// files may be stale on this runtime and cannot replace their durable facts.
    #[cfg(test)]
    pub async fn publish_platform(
        &self,
        entity: &ActiveEntity,
        manifests: &dcl_contents::manifest_store::AbManifestStore,
        platform: &str,
    ) -> Result<RegistryRecord, sqlx::Error> {
        self.persist_with(entity, Some(platform), None, || async {
            let snapshot = manifests.refresh(&entity.entity_id).await;
            let mut bundles = snapshot.bundles(false);
            bundles.lods.get_or_insert_with(Default::default);
            (bundles, snapshot.versions())
        })
        .await
    }

    #[cfg(test)]
    pub async fn persist_and_rotate(
        &self,
        entity: &ActiveEntity,
        bundles: Bundles,
        versions: Versions,
    ) -> Result<RegistryRecord, sqlx::Error> {
        self.persist_with(entity, Some("*"), None, || async { (bundles, versions) })
            .await
    }

    async fn persist_with<F, Fut>(
        &self,
        entity: &ActiveEntity,
        authoritative_platform: Option<&str>,
        completion: Option<(&ConversionAttempt, Option<&super::registry::BuildJob>, bool)>,
        observe: F,
    ) -> Result<RegistryRecord, sqlx::Error>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = (Bundles, Versions)>,
    {
        let pool = self.pool.as_ref().ok_or_else(disabled)?;
        let world = entity.world_name().unwrap_or("").to_lowercase();
        let pointers: Vec<String> = entity.pointers.iter().map(|p| p.to_lowercase()).collect();
        let mut lock_keys: Vec<String> = pointers
            .iter()
            .map(|pointer| serde_json::json!(["registry-pointer", world, pointer]).to_string())
            .collect();
        lock_keys.push(
            serde_json::json!(["registry-entity", entity.entity_id.to_lowercase()]).to_string(),
        );
        lock_keys.sort();
        lock_keys.dedup();

        let mut tx = pool.begin().await?;
        for key in lock_keys {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
                .bind(key)
                .execute(&mut *tx)
                .await?;
        }
        if let Some((attempt, job, _)) = completion {
            let generation: Option<i64> = sqlx::query_scalar(
                "SELECT generation FROM registry_conversion_attempts WHERE entity_id = $1 AND platform = $2 FOR UPDATE"
            ).bind(&attempt.entity_id).bind(&attempt.platform).fetch_optional(&mut *tx).await?;
            if generation != Some(attempt.generation) {
                return Err(sqlx::Error::RowNotFound);
            }
            if let Some(job) = job {
                if job.entity_id != attempt.entity_id || job.platform != attempt.platform {
                    return Err(sqlx::Error::Protocol("conversion claim mismatch".into()));
                }
                let token: Option<i64> = sqlx::query_scalar(
                    "SELECT updated_at FROM build_jobs WHERE entity_id = $1 AND platform = $2 AND status = 'running' FOR UPDATE"
                ).bind(&job.entity_id).bind(&job.platform).fetch_optional(&mut *tx).await?;
                if token != Some(job.claimed_at) {
                    return Err(sqlx::Error::RowNotFound);
                }
            }
        }
        let (mut bundles, mut versions) = observe().await;
        // Converted files are derived artifacts and may have been evicted.
        // Their absence is not a new conversion outcome. Read and merge the
        // durable facts under the same locks as rotation so a stale refresh
        // cannot erase a concurrently completed conversion.
        if let Some(previous) = sqlx::query(
            "SELECT entity_id, world_name, entity_timestamp, pointers, status, bundles, versions \
             FROM registry_records WHERE entity_id = $1 FOR UPDATE",
        )
        .bind(&entity.entity_id)
        .fetch_optional(&mut *tx)
        .await?
        .map(row_to_record)
        .transpose()?
        {
            preserve_known_conversion(
                &mut bundles,
                &mut versions,
                &previous,
                authoritative_platform,
            );
        }
        let related: Vec<RegistryRecord> = sqlx::query(
            "SELECT entity_id, world_name, entity_timestamp, pointers, status, bundles, versions \
             FROM registry_records WHERE world_name = $1 AND pointers && $2 \
             AND lower(entity_id) <> lower($3) AND status <> 'obsolete' \
             ORDER BY entity_timestamp DESC, entity_id FOR UPDATE",
        )
        .bind(&world)
        .bind(&pointers)
        .bind(&entity.entity_id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(row_to_record)
        .collect::<Result<_, _>>()?;
        let (status, fallback) = rotation(entity.timestamp, &bundles, &related);
        let now = chrono::Utc::now().timestamp_millis();
        let row = sqlx::query(
            "INSERT INTO registry_records \
             (entity_id, world_name, entity_timestamp, pointers, status, bundles, versions, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (entity_id) DO UPDATE SET world_name = EXCLUDED.world_name, \
             entity_timestamp = EXCLUDED.entity_timestamp, pointers = EXCLUDED.pointers, \
             status = EXCLUDED.status, bundles = EXCLUDED.bundles, versions = EXCLUDED.versions, \
             updated_at = EXCLUDED.updated_at \
             RETURNING entity_id, world_name, entity_timestamp, pointers, status, bundles, versions",
        )
        .bind(&entity.entity_id)
        .bind(&world)
        .bind(entity.timestamp)
        .bind(&pointers)
        .bind(status_name(status))
        .bind(Json(bundles))
        .bind(Json(versions))
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        if let Some((id, next)) = fallback {
            sqlx::query(
                "UPDATE registry_records SET status = $2, updated_at = $3 WHERE entity_id = $1",
            )
            .bind(id)
            .bind(status_name(next))
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        if let Some((_, Some(job), succeeded)) = completion {
            sqlx::query("UPDATE build_jobs SET status = $3, updated_at = GREATEST(updated_at + 1, $4) WHERE entity_id = $1 AND platform = $2")
                .bind(&job.entity_id).bind(&job.platform).bind(if succeeded { "complete" } else { "failed" }).bind(now)
                .execute(&mut *tx).await?;
        }
        let record = row_to_record(row)?;
        tx.commit().await?;
        Ok(record)
    }

    pub async fn get_record(&self, id: &str) -> Result<Option<RegistryRecord>, sqlx::Error> {
        let pool = self.pool.as_ref().ok_or_else(disabled)?;
        sqlx::query(
            "SELECT entity_id, world_name, entity_timestamp, pointers, status, bundles, versions \
             FROM registry_records WHERE lower(entity_id) = lower($1)",
        )
        .bind(id)
        .fetch_optional(pool)
        .await?
        .map(row_to_record)
        .transpose()
    }

    pub async fn records_by_pointers(
        &self,
        pointers: &[String],
        world: Option<&str>,
        exclude_denylisted: bool,
    ) -> Result<Vec<RegistryRecord>, sqlx::Error> {
        self.select_records(Some(pointers), world, true, exclude_denylisted)
            .await
    }

    pub async fn records_for_pointers_all_statuses(
        &self,
        pointers: &[String],
        world: Option<&str>,
    ) -> Result<Vec<RegistryRecord>, sqlx::Error> {
        self.select_records(Some(pointers), world, false, false)
            .await
    }

    pub async fn records_for_world(&self, world: &str) -> Result<Vec<RegistryRecord>, sqlx::Error> {
        self.select_records(None, Some(world), true, true).await
    }

    async fn select_records(
        &self,
        pointers: Option<&[String]>,
        world: Option<&str>,
        ready_only: bool,
        exclude_denylisted: bool,
    ) -> Result<Vec<RegistryRecord>, sqlx::Error> {
        let pool = self.pool.as_ref().ok_or_else(disabled)?;
        let normalized: Option<Vec<String>> =
            pointers.map(|ps| ps.iter().map(|p| p.to_lowercase()).collect());
        sqlx::query(
            "SELECT entity_id, world_name, entity_timestamp, pointers, status, bundles, versions \
             FROM registry_records r WHERE world_name = $1 \
             AND ($2::text[] IS NULL OR pointers && $2) \
             AND (NOT $3 OR status IN ('complete', 'fallback')) \
             AND (NOT $4 OR NOT EXISTS (SELECT 1 FROM denylist d WHERE d.entity_id = lower(r.entity_id))) \
             ORDER BY entity_timestamp DESC, entity_id",
        )
        .bind(world.unwrap_or("").to_lowercase())
        .bind(normalized)
        .bind(ready_only)
        .bind(exclude_denylisted)
        .fetch_all(pool)
        .await?
        .into_iter()
        .map(row_to_record)
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::{extract::State, http::HeaderMap, Json as HttpJson};
    use dcl_contents::errors::ApiError;
    use dcl_contents::manifest_store::AbManifestStore;
    use dcl_contents::registry::{async_trait, EntitySource};
    use serde_json::json;
    use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

    fn entity(id: &str, timestamp: i64, world: Option<&str>, pointers: &[&str]) -> ActiveEntity {
        ActiveEntity {
            deployment_id: 0,
            entity_id: id.into(),
            entity_type: "scene".into(),
            timestamp,
            pointers: pointers.iter().map(|p| p.to_string()).collect(),
            metadata: world
                .map(|world| json!({"worldConfiguration":{"name":world}}))
                .unwrap_or_else(|| json!({})),
            deployer_address: Some("0x1234".into()),
            content: vec![],
        }
    }

    fn bundles(status: BuildStatus) -> Bundles {
        let mut bundles = Bundles::default();
        bundles.assets.mac = status;
        bundles.assets.windows = status;
        bundles
    }

    struct Source {
        entities: HashMap<String, ActiveEntity>,
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl EntitySource for Source {
        async fn resolve_pointers(&self, _: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
            unreachable!("registration must reuse exact entity lookup")
        }
        async fn resolve_profiles(&self, _: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
            unreachable!()
        }
        async fn resolve_world(&self, _: &str) -> Result<Vec<ActiveEntity>, ApiError> {
            unreachable!()
        }
        async fn resolve_one(&self, id: &str) -> Result<Option<ActiveEntity>, ApiError> {
            self.calls.lock().unwrap().push(id.into());
            Ok(self.entities.get(id).cloned())
        }
    }

    #[tokio::test]
    #[ignore = "requires CATALYST_TEST_DATABASE_URL; run explicitly with --ignored"]
    async fn registration_persists_rotates_filters_and_survives_reconnect() {
        let url = std::env::var("CATALYST_TEST_DATABASE_URL")
            .expect("CATALYST_TEST_DATABASE_URL is required");
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let schema = format!("registry_records_{}_{suffix}", std::process::id());
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
            .application_name(&schema)
            .options([("search_path", schema.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options.clone())
            .await
            .unwrap();
        let migrations = sqlx::migrate!("./migrations");
        let old_migrations = sqlx::migrate::Migrator::with_migrations(
            migrations
                .iter()
                .filter(|migration| migration.version <= 3)
                .cloned()
                .collect(),
        );
        old_migrations.run(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO denylist(entity_id, created_by) VALUES ('preexisting-block', 'moderator')",
        )
        .execute(&pool)
        .await
        .unwrap();
        migrations.run(&pool).await.unwrap();
        let store = RegistryStore::new(Some(pool.clone()));
        assert_eq!(
            store.denylist_rows().await.unwrap()[0].entity_id,
            "preexisting-block"
        );
        let a = entity("bafkregistrya", 1000, None, &["0,0", "1,0"]);
        let b = entity("bafkregistryb", 2000, None, &["0,0", "1,0"]);
        let world = entity("bafkregistryworld", 3000, Some("Mixed.dcl.eth"), &["0,0"]);
        store
            .persist_and_rotate(&a, bundles(BuildStatus::Complete), Versions::default())
            .await
            .unwrap();
        store
            .persist_and_rotate(&b, bundles(BuildStatus::Pending), Versions::default())
            .await
            .unwrap();
        assert_eq!(
            store
                .get_record(&a.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Fallback
        );
        assert_eq!(
            store
                .get_record(&b.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Pending
        );
        store
            .persist_and_rotate(&b, bundles(BuildStatus::Failed), Versions::default())
            .await
            .unwrap();
        assert_eq!(
            store
                .get_record(&a.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Fallback
        );
        let ready = store
            .records_by_pointers(&["0,0".into()], None, true)
            .await
            .unwrap();
        assert_eq!(
            ready
                .iter()
                .map(|r| r.entity_id.as_str())
                .collect::<Vec<_>>(),
            vec![a.entity_id.as_str()]
        );
        store
            .persist_and_rotate(&world, bundles(BuildStatus::Complete), Versions::default())
            .await
            .unwrap();
        assert_eq!(
            store
                .records_for_world("MIXED.DCL.ETH")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .get_record(&a.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Fallback
        );

        // Two updates on the same overlapping pointer set serialize. A newer
        // complete conversion wins even if the old complete import runs later.
        let (old, new) = tokio::join!(
            store.persist_and_rotate(&a, bundles(BuildStatus::Complete), Versions::default()),
            store.persist_and_rotate(&b, bundles(BuildStatus::Complete), Versions::default()),
        );
        old.unwrap();
        new.unwrap();
        assert_eq!(
            store
                .get_record(&a.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Obsolete
        );
        assert_eq!(
            store
                .get_record(&b.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Complete
        );
        assert_eq!(
            store
                .records_for_pointers_all_statuses(&["0,0".into()], None)
                .await
                .unwrap()
                .len(),
            2
        );
        store
            .add_to_denylist(&b.entity_id, "moderator", None)
            .await
            .unwrap();
        assert!(store
            .records_by_pointers(&["0,0".into()], None, true)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .records_by_pointers(&["0,0".into()], None, false)
                .await
                .unwrap()
                .len(),
            1
        );
        store.remove_from_denylist(&b.entity_id).await.unwrap();

        // Exercise POST /registry's shared source, ordered deduplication, real
        // manifest refresh and durable side effect, including partial failure.
        let imported = entity("bafkregistryimported", 4000, None, &["9,9"]);
        let source = Arc::new(Source {
            entities: HashMap::from([(imported.entity_id.clone(), imported.clone())]),
            calls: Mutex::new(vec![]),
        });
        let root = std::env::temp_dir().join(&schema);
        let manifests = AbManifestStore::new(&root);
        assert_eq!(
            manifests.get(&imported.entity_id).await.mac_status(),
            BuildStatus::Pending
        );
        let directory = root.join(&imported.entity_id);
        std::fs::create_dir_all(&directory).unwrap();
        for platform in ["mac", "windows"] {
            std::fs::write(
                directory.join(format!("{platform}.manifest.json")),
                r#"{"version":"v-test","exitCode":0,"date":"2026-01-01T00:00:00.000Z"}"#,
            )
            .unwrap();
        }
        let state = Arc::new(crate::registry::AppStateInner {
            content: source.clone(),
            manifests,
            registry: store.clone(),
            admin_token: Some("test-registration".into()),
            denylist_moderators: vec![],
        });
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-registration".parse().unwrap());
        let HttpJson(response) = crate::registry::handlers::admin::post_registry(
            State(state.clone()), headers,
            Some(HttpJson(json!({"entityIds":["missing-z", imported.entity_id, "missing-a", imported.entity_id]}))),
        ).await.unwrap();
        assert_eq!(
            response,
            json!({"successes":[],"failures":[
                {"entityId":"missing-z","error":"Entity not found in catalyst"},
                {"entityId":"missing-a","error":"Entity not found in catalyst"}
            ]})
        );
        assert_eq!(
            *source.calls.lock().unwrap(),
            vec!["missing-z", imported.entity_id.as_str(), "missing-a"]
        );
        let imported_record = store
            .get_record(&imported.entity_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(imported_record.status, RegistryStatus::Complete);
        assert_eq!(imported_record.versions.assets.mac.version, "v-test");
        assert_eq!(
            imported_record.bundles.lods.as_ref().unwrap().mac,
            BuildStatus::Pending
        );
        let mut known_bundles = imported_record.bundles.clone();
        known_bundles.lods.as_mut().unwrap().windows = BuildStatus::Complete;
        store
            .persist_and_rotate(&imported, known_bundles, imported_record.versions)
            .await
            .unwrap();
        for platform in ["mac", "windows"] {
            std::fs::remove_file(directory.join(format!("{platform}.manifest.json"))).unwrap();
        }
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer test-registration".parse().unwrap());
        let HttpJson(response) = crate::registry::handlers::admin::post_registry(
            State(state.clone()),
            headers,
            Some(HttpJson(json!({"entityIds":[imported.entity_id]}))),
        )
        .await
        .unwrap();
        assert_eq!(response, json!({"successes":[],"failures":[]}));
        let after_eviction = store
            .get_record(&imported.entity_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after_eviction.status, RegistryStatus::Complete);
        assert_eq!(
            after_eviction.bundles.lods.unwrap().windows,
            BuildStatus::Complete
        );
        assert_eq!(after_eviction.versions.assets.mac.version, "v-test");
        assert_eq!(
            after_eviction.versions.assets.mac.build_date,
            "2026-01-01T00:00:00.000Z"
        );
        // An actual failed converter result still supersedes the previous
        // success; only missing/pending observations preserve known outcomes.
        let failed = store
            .persist_and_rotate(&imported, bundles(BuildStatus::Failed), Versions::default())
            .await
            .unwrap();
        assert_eq!(failed.status, RegistryStatus::Failed);
        let still_failed = store
            .persist_and_rotate(&imported, Bundles::default(), Versions::default())
            .await
            .unwrap();
        assert_eq!(still_failed.status, RegistryStatus::Failed);
        store
            .persist_and_rotate(
                &imported,
                bundles(BuildStatus::Complete),
                Versions::default(),
            )
            .await
            .unwrap();

        // Hold the same entity lock as a successful publisher. A refresh starts
        // while files contain failure but must observe them only after this
        // publisher installs success and commits. Poll PostgreSQL's lock wait,
        // rather than depending on scheduling or a guessed sleep duration.
        let mut publication = pool.begin().await.unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(
                serde_json::json!(["registry-entity", imported.entity_id.to_lowercase()])
                    .to_string(),
            )
            .execute(&mut *publication)
            .await
            .unwrap();
        for platform in ["mac", "windows"] {
            std::fs::write(
                directory.join(format!("{platform}.manifest.json")),
                r#"{"version":"stale-failure","exitCode":1}"#,
            )
            .unwrap();
        }
        let refresh_store = store.clone();
        let refresh_entity = imported.clone();
        let refresh_manifests = state.manifests.clone();
        let waiting = tokio::spawn(async move {
            refresh_store
                .refresh_and_rotate(&refresh_entity, &refresh_manifests)
                .await
                .unwrap()
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_locks l JOIN pg_stat_activity a USING(pid) \
                     WHERE l.locktype = 'advisory' AND NOT l.granted AND a.application_name = $1)",
                )
                .bind(&schema)
                .fetch_one(&control)
                .await
                .unwrap();
                if blocked {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("refresh must wait for the publisher's entity lock");
        for platform in ["mac", "windows"] {
            std::fs::write(
                directory.join(format!("{platform}.manifest.json")),
                r#"{"version":"v-test","exitCode":0,"date":"2026-01-01T00:00:00.000Z"}"#,
            )
            .unwrap();
        }
        publication.commit().await.unwrap();
        assert_eq!(waiting.await.unwrap().status, RegistryStatus::Complete);

        // Another runtime can retain failed artifacts after durable success.
        // Reading those artifacts is an observation, not a new converter result.
        for platform in ["mac", "windows"] {
            std::fs::write(
                directory.join(format!("{platform}.manifest.json")),
                r#"{"version":"old-runtime-failure","exitCode":1}"#,
            )
            .unwrap();
        }
        let observed = store
            .refresh_and_rotate(&imported, &state.manifests)
            .await
            .unwrap();
        assert_eq!(observed.status, RegistryStatus::Complete);
        assert_eq!(observed.versions.assets.mac.version, "v-test");
        let attempted = store
            .publish_platform(&imported, &state.manifests, "mac")
            .await
            .unwrap();
        assert_eq!(attempted.bundles.assets.mac, BuildStatus::Failed);
        assert_eq!(
            attempted.bundles.assets.windows,
            BuildStatus::Complete,
            "a mac completion must not publish stale windows observations"
        );
        std::fs::write(
            directory.join("mac.manifest.json"),
            r#"{"version":"v-test","exitCode":0,"date":"2026-01-01T00:00:00.000Z"}"#,
        )
        .unwrap();
        assert_eq!(
            store
                .publish_platform(&imported, &state.manifests, "mac")
                .await
                .unwrap()
                .status,
            RegistryStatus::Complete
        );

        // HTTP and worker completions share generations. A delayed old failure
        // cannot replace a newer successful attempt, even across runtimes.
        let delayed = store
            .begin_conversion(&imported.entity_id, "mac")
            .await
            .unwrap();
        let newest = store
            .begin_conversion(&imported.entity_id, "mac")
            .await
            .unwrap();
        assert!(store
            .complete_conversion(
                &imported,
                &newest,
                None,
                bundles(BuildStatus::Complete),
                Versions::default()
            )
            .await
            .unwrap()
            .is_some());
        assert!(store
            .complete_conversion(
                &imported,
                &delayed,
                None,
                bundles(BuildStatus::Failed),
                Versions::default()
            )
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .get_record(&imported.entity_id)
                .await
                .unwrap()
                .unwrap()
                .bundles
                .assets
                .mac,
            BuildStatus::Complete
        );

        // Retrying a running job revokes its receipt independently of the
        // conversion generation. Neither facts nor terminal queue state change.
        store
            .enqueue_build(&imported.entity_id, "test")
            .await
            .unwrap();
        let revoked = store.claim_build_job().await.unwrap().unwrap();
        let stale_attempt = store
            .begin_conversion(&revoked.entity_id, &revoked.platform)
            .await
            .unwrap();
        store
            .enqueue_build(&imported.entity_id, "retry")
            .await
            .unwrap();
        assert!(store
            .complete_conversion(
                &imported,
                &stale_attempt,
                Some(&revoked),
                bundles(BuildStatus::Failed),
                Versions::default()
            )
            .await
            .unwrap()
            .is_none());
        let pending: String = sqlx::query_scalar(
            "SELECT status FROM build_jobs WHERE entity_id = $1 AND platform = $2",
        )
        .bind(&revoked.entity_id)
        .bind(&revoked.platform)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(pending, "pending");
        let owned = store.claim_build_job().await.unwrap().unwrap();
        let owned_attempt = store
            .begin_conversion(&owned.entity_id, &owned.platform)
            .await
            .unwrap();
        let mut result = Bundles::default();
        match owned.platform.as_str() {
            "linux" => result.assets.linux = BuildStatus::Complete,
            "mac" => result.assets.mac = BuildStatus::Complete,
            "windows" => result.assets.windows = BuildStatus::Complete,
            "webgl" => result.assets.webgl = BuildStatus::Complete,
            _ => unreachable!(),
        }
        assert!(store
            .complete_conversion(
                &imported,
                &owned_attempt,
                Some(&owned),
                result,
                Versions::default()
            )
            .await
            .unwrap()
            .is_some());
        let complete: String = sqlx::query_scalar(
            "SELECT status FROM build_jobs WHERE entity_id = $1 AND platform = $2",
        )
        .bind(&owned.entity_id)
        .bind(&owned.platform)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(complete, "complete");

        // Publication failure rolls back facts and queue acknowledgement as one
        // transaction. The running receipt remains available for recovery.
        let blocked_job = store.claim_build_job().await.unwrap().unwrap();
        let blocked_attempt = store
            .begin_conversion(&blocked_job.entity_id, &blocked_job.platform)
            .await
            .unwrap();
        sqlx::query("ALTER TABLE registry_records ADD CONSTRAINT test_publication_failure CHECK (status <> 'failed') NOT VALID")
            .execute(&pool).await.unwrap();
        let failed_publication = store
            .complete_conversion(
                &imported,
                &blocked_attempt,
                Some(&blocked_job),
                bundles(BuildStatus::Failed),
                Versions::default(),
            )
            .await;
        assert!(failed_publication.is_err());
        let running: String = sqlx::query_scalar(
            "SELECT status FROM build_jobs WHERE entity_id = $1 AND platform = $2",
        )
        .bind(&blocked_job.entity_id)
        .bind(&blocked_job.platform)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(running, "running");
        assert_eq!(
            store
                .get_record(&imported.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Complete
        );
        sqlx::query("ALTER TABLE registry_records DROP CONSTRAINT test_publication_failure")
            .execute(&pool)
            .await
            .unwrap();

        drop(state);
        drop(store);
        pool.close().await;
        let reopened = PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .unwrap();
        // The same migration ledger is valid on restart; no table reset occurs.
        sqlx::migrate!("./migrations").run(&reopened).await.unwrap();
        let reopened_store = RegistryStore::new(Some(reopened.clone()));
        assert_eq!(
            reopened_store
                .get_record(&b.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Complete
        );
        assert_eq!(
            reopened_store
                .get_record(&a.entity_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            RegistryStatus::Obsolete
        );
        assert_eq!(
            reopened_store
                .get_record(&imported.entity_id)
                .await
                .unwrap()
                .unwrap()
                .versions
                .assets
                .mac
                .version,
            "v-test"
        );
        drop(reopened_store);
        reopened.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&control)
            .await
            .unwrap();
        control.close().await;
        std::fs::remove_dir_all(root).unwrap();
    }
}
