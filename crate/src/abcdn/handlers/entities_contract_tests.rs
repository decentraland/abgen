use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use dcl_contents::manifest_store::AbManifestStore;
use dcl_contents::registry::{async_trait, EntitySource, RegistryStateInner};
use dcl_contents::types::{ContentFile, RegistryStatus};
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::registry::ports::registry::RegistryStore;
use crate::registry::source::RegisteredSource;

struct CatalystFixture {
    history: Vec<ActiveEntity>,
    current: Mutex<HashSet<String>>,
}

#[async_trait]
impl EntitySource for CatalystFixture {
    async fn resolve_pointers(&self, pointers: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        let current = self.current.lock().unwrap();
        Ok(self
            .history
            .iter()
            .filter(|entity| {
                current.contains(&entity.entity_id)
                    && entity.world_name().is_none()
                    && entity.pointers.iter().any(|p| pointers.contains(p))
            })
            .cloned()
            .collect())
    }
    async fn resolve_profiles(&self, _: &[String]) -> Result<Vec<ActiveEntity>, ApiError> {
        Ok(vec![])
    }
    async fn resolve_world(&self, world: &str) -> Result<Vec<ActiveEntity>, ApiError> {
        let current = self.current.lock().unwrap();
        Ok(self
            .history
            .iter()
            .filter(|entity| {
                current.contains(&entity.entity_id)
                    && entity
                        .world_name()
                        .is_some_and(|name| name.eq_ignore_ascii_case(world))
            })
            .cloned()
            .collect())
    }
    async fn resolve_candidates(
        &self,
        pointers: &[String],
        world: Option<&str>,
    ) -> Result<Vec<ActiveEntity>, ApiError> {
        let entities = match world {
            Some(world) => self.resolve_world(world).await?,
            None => self
                .history
                .iter()
                .filter(|entity| entity.world_name().is_none())
                .cloned()
                .collect(),
        };
        Ok(entities
            .into_iter()
            .filter(|entity| entity.pointers.iter().any(|p| pointers.contains(p)))
            .collect())
    }
    async fn resolve_one(&self, id: &str) -> Result<Option<ActiveEntity>, ApiError> {
        Ok(self
            .history
            .iter()
            .find(|entity| entity.entity_id == id)
            .cloned())
    }
}

fn entity(id: &str, timestamp: i64, world: Option<&str>) -> ActiveEntity {
    ActiveEntity {
        deployment_id: 0,
        entity_id: id.into(),
        entity_type: "scene".into(),
        timestamp,
        pointers: vec!["0,0".into()],
        metadata: world
            .map(|name| json!({"worldConfiguration":{"name":name},"marker":id}))
            .unwrap_or_else(|| json!({"marker":id})),
        deployer_address: Some("0x1234".into()),
        content: vec![ContentFile {
            file: "scene.json".into(),
            hash: format!("hash-{id}"),
        }],
    }
}

fn manifest(root: &std::path::Path, id: &str, exit_code: i32) {
    let directory = root.join(id);
    std::fs::create_dir_all(&directory).unwrap();
    for platform in ["mac", "windows"] {
        std::fs::write(directory.join(format!("{platform}.manifest.json")),
            json!({"version":format!("v-{id}"),"exitCode":exit_code,"date":"2026-01-01T00:00:00.000Z"}).to_string()).unwrap();
    }
}

fn query(world: Option<&str>) -> Query<HashMap<String, String>> {
    Query(
        world
            .map(|world| HashMap::from([("world_name".into(), world.into())]))
            .unwrap_or_default(),
    )
}

async fn active(state: &AppState, world: Option<&str>) -> Vec<Value> {
    post_entities_active(
        State(state.clone()),
        query(world),
        Json(json!({"pointers":["0,0"]})),
    )
    .await
    .unwrap()
    .0
}

async fn versions(state: &AppState, world: Option<&str>) -> Vec<Value> {
    post_entities_versions(
        State(state.clone()),
        query(world),
        Json(json!({"pointers":["0,0"]})),
    )
    .await
    .unwrap()
    .0
}

#[tokio::test]
#[ignore = "requires CATALYST_TEST_DATABASE_URL; run explicitly with --ignored"]
async fn registry_reads_share_catalyst_and_preserve_conversion_fallback_denylist_and_world_scope() {
    let url = std::env::var("CATALYST_TEST_DATABASE_URL")
        .expect("CATALYST_TEST_DATABASE_URL is required");
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let schema = format!("registry_reads_{}_{suffix}", std::process::id());
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
        .max_connections(3)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    let store = RegistryStore::new(Some(pool.clone()));
    let root = std::env::temp_dir().join(&schema);
    let city_old = entity("bafkcityold", 1000, None);
    let city_new = entity("bafkcitynew", 2000, None);
    let mut world_old = entity("bafkworldold", 1500, Some("world.dcl.eth"));
    world_old.pointers.push("1,0".into());
    let mut world_other = entity("bafkworldother", 2500, Some("world.dcl.eth"));
    world_other.pointers = vec!["1,0".into()];
    let world_new = entity("bafkworldnew", 2500, Some("world.dcl.eth"));
    let source = Arc::new(CatalystFixture {
        history: vec![
            city_old.clone(),
            city_new.clone(),
            world_old.clone(),
            world_new.clone(),
            world_other.clone(),
        ],
        current: Mutex::new(HashSet::from([
            city_new.entity_id.clone(),
            world_old.entity_id.clone(),
        ])),
    });
    manifest(&root, &city_old.entity_id, 0);
    manifest(&root, &world_old.entity_id, 0);
    let manifests = AbManifestStore::new(&root);
    let registered = Arc::new(RegisteredSource {
        content: source.clone(),
        registry: store.clone(),
        manifests: manifests.clone(),
    });
    let contents = Arc::new(RegistryStateInner {
        content: registered,
        manifests,
        profile_images_url: "https://images.example.org".into(),
        world_policy: Arc::new(store.clone()),
    });
    let mut app = super::super::tests::mk_lane_state(&root, None);
    let inner = Arc::get_mut(&mut app).unwrap();
    inner.contents_registry = Some(contents.clone());
    inner.registry_store = Some(store.clone());

    let response = active(&app, None).await;
    assert_eq!(response.len(), 1);
    assert_eq!(response[0]["id"], city_old.entity_id);
    assert_eq!(response[0]["timestamp"], "1000");
    assert_eq!(response[0]["status"], "fallback");
    assert_eq!(response[0]["deployer"], "0x1234");
    assert_eq!(response[0]["metadata"]["marker"], city_old.entity_id);
    assert_eq!(
        response[0]["content"][0]["hash"],
        format!("hash-{}", city_old.entity_id)
    );
    assert_eq!(
        response[0]["bundles"]["assets"].as_object().unwrap().len(),
        2
    );
    let version = versions(&app, None).await;
    assert_eq!(version[0]["status"], "fallback");
    assert_eq!(
        version[0]["versions"]["assets"]["mac"]["version"],
        format!("v-{}", city_old.entity_id)
    );
    assert_eq!(version[0].as_object().unwrap().len(), 4);
    store
        .add_to_denylist(&city_old.entity_id, "moderator", None)
        .await
        .unwrap();
    assert!(active(&app, None).await.is_empty());
    assert_eq!(
        versions(&app, None).await.len(),
        1,
        "versions includes denylisted ready records"
    );
    store
        .remove_from_denylist(&city_old.entity_id)
        .await
        .unwrap();
    manifest(&root, &city_new.entity_id, 0);
    assert_eq!(active(&app, None).await[0]["id"], city_new.entity_id);
    assert_eq!(
        store
            .get_record(&city_old.entity_id)
            .await
            .unwrap()
            .unwrap()
            .status,
        RegistryStatus::Obsolete
    );

    assert_eq!(
        active(&app, Some("WORLD.DCL.ETH")).await[0]["id"],
        world_old.entity_id
    );
    {
        let mut current = source.current.lock().unwrap();
        current.remove(&world_old.entity_id);
        current.insert(world_new.entity_id.clone());
        current.insert(world_other.entity_id.clone());
    }
    manifest(&root, &world_new.entity_id, 1);
    let response = active(&app, Some("world.dcl.eth")).await;
    assert_eq!(response[0]["id"], world_old.entity_id);
    assert_eq!(response[0]["status"], "fallback");
    assert_eq!(response[0]["timestamp"], "1500");
    assert_eq!(
        active(&app, None).await[0]["id"],
        city_new.entity_id,
        "world rotation must not change city state"
    );
    let Json(world) = dcl_contents::handlers::worlds::get_world_manifest(
        State(contents.clone()),
        axum::extract::Path("world.dcl.eth".into()),
    )
    .await
    .unwrap();
    assert_eq!(world.occupied, vec!["0,0", "1,0"]);
    assert_eq!(
        response[0]["pointers"],
        json!(["0,0", "1,0"]),
        "querying one parcel must retain a ready fallback spanning two live pending replacements"
    );
    store
        .add_to_denylist(&world_old.entity_id, "moderator", None)
        .await
        .unwrap();
    assert!(active(&app, Some("world.dcl.eth")).await.is_empty());
    assert_eq!(versions(&app, Some("world.dcl.eth")).await.len(), 1);
    let Json(world) = dcl_contents::handlers::worlds::get_world_manifest(
        State(contents.clone()),
        axum::extract::Path("world.dcl.eth".into()),
    )
    .await
    .unwrap();
    assert!(world.occupied.is_empty());
    store
        .remove_from_denylist(&world_old.entity_id)
        .await
        .unwrap();
    source.current.lock().unwrap().remove(&world_new.entity_id);
    assert!(
        active(&app, Some("world.dcl.eth")).await.is_empty(),
        "a genuinely undeployed parcel must still invalidate the full entity document"
    );
    source
        .current
        .lock()
        .unwrap()
        .remove(&world_other.entity_id);
    let Json(world) = dcl_contents::handlers::worlds::get_world_manifest(
        State(contents.clone()),
        axum::extract::Path("world.dcl.eth".into()),
    )
    .await
    .unwrap();
    assert!(
        world.occupied.is_empty(),
        "undeployed worlds must not resurrect historical conversions"
    );
    assert!(
        active(&app, Some("world.dcl.eth")).await.is_empty(),
        "undeployed world active reads must not resurrect historical conversions"
    );

    assert!(versions(&app, Some("world.dcl.eth")).await.is_empty());

    drop(app);
    drop(contents);
    drop(store);
    pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&control)
        .await
        .unwrap();
    control.close().await;
    std::fs::remove_dir_all(root).unwrap();
}
