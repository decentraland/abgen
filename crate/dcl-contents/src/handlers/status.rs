use axum::extract::{Path, Query, State};
use axum::Json;

use crate::errors::{ApiError, ApiResult};
use crate::manifest_store::AbManifests;
use crate::registry::RegistryAppState;
use crate::types::{BuildStatus, Bundles, EntityStatus, PlatformStatuses, WorldNameQuery};

pub async fn get_entity_status(
    State(state): State<RegistryAppState>,
    Path(id): Path<String>,
    Query(q): Query<WorldNameQuery>,
) -> ApiResult<Json<EntityStatus>> {
    let resolved = state
        .content
        .resolve_status(&id, q.world_name.as_deref())
        .await?;

    let ent = resolved.ok_or_else(|| {
        ApiError::not_found("No active entity found for the provided id or pointer")
    })?;
    if let Some(status) = state.content.registered_status(&ent.entity_id).await? {
        return Ok(Json(status));
    }
    let m = state.manifests.get(&ent.entity_id).await;
    Ok(Json(entity_status_from(&ent.entity_id, &m, ent.is_world())))
}

pub fn entity_status_from(entity_id: &str, m: &AbManifests, _is_world: bool) -> EntityStatus {
    entity_status_from_bundles(entity_id, &m.bundles(false))
}

pub fn entity_status_from_bundles(entity_id: &str, bundles: &Bundles) -> EntityStatus {
    let asset_bundles = PlatformStatuses {
        mac: bundles.assets.mac,
        windows: bundles.assets.windows,
    };
    let complete = matches!(asset_bundles.mac, BuildStatus::Complete)
        && matches!(asset_bundles.windows, BuildStatus::Complete);
    EntityStatus {
        entity_id: entity_id.to_string(),
        catalyst: BuildStatus::Complete,
        complete,
        asset_bundles,
        lods: Some(PlatformStatuses {
            mac: bundles
                .lods
                .as_ref()
                .map(|lods| lods.mac)
                .unwrap_or(BuildStatus::Pending),
            windows: bundles
                .lods
                .as_ref()
                .map(|lods| lods.windows)
                .unwrap_or(BuildStatus::Pending),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::response::IntoResponse;
    use serde_json::json;

    use crate::registry::testutil::{entity, open_state, open_state_dual};

    fn write_manifest(root: &std::path::Path, id: &str, platform: &str, exit: i32) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{platform}.manifest.json")),
            serde_json::to_string(&json!({
                "version": "v41",
                "exitCode": exit,
                "date": "2024-03-15T12:34:56.789Z",
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn tmp_root(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("dclc_status_{tag}_{}", std::process::id()))
    }

    struct DurableSource(crate::registry::testutil::StubSource);

    #[crate::registry::async_trait]
    impl crate::registry::EntitySource for DurableSource {
        async fn resolve_pointers(
            &self,
            pointers: &[String],
        ) -> Result<Vec<crate::types::ActiveEntity>, ApiError> {
            self.0.resolve_pointers(pointers).await
        }
        async fn resolve_profiles(
            &self,
            addresses: &[String],
        ) -> Result<Vec<crate::types::ActiveEntity>, ApiError> {
            self.0.resolve_profiles(addresses).await
        }
        async fn resolve_world(
            &self,
            world: &str,
        ) -> Result<Vec<crate::types::ActiveEntity>, ApiError> {
            self.0.resolve_world(world).await
        }
        async fn registered_status(&self, id: &str) -> Result<Option<EntityStatus>, ApiError> {
            let mut bundles = Bundles::default();
            bundles.assets.mac = BuildStatus::Complete;
            bundles.assets.windows = BuildStatus::Complete;
            Ok(Some(entity_status_from_bundles(id, &bundles)))
        }
    }

    #[tokio::test]
    async fn durable_status_and_world_coverage_survive_absent_manifest_files() {
        use std::sync::Arc;
        let id = "bafkdurableworldstatus";
        let state = Arc::new(crate::registry::RegistryStateInner {
            content: Arc::new(DurableSource(crate::registry::testutil::StubSource(vec![
                entity(
                    id,
                    "scene",
                    &["0,0", "1,0"],
                    json!({"worldConfiguration":{"name":"world.dcl.eth"}}),
                ),
            ]))),
            manifests: crate::manifest_store::AbManifestStore::new(tmp_root("durable")),
            profile_images_url: String::new(),
            world_policy: Arc::new(crate::registry::OpenWorldPolicy),
        });
        assert_eq!(
            state.manifests.get(id).await.windows_status(),
            BuildStatus::Pending
        );
        let Json(status) = get_entity_status(
            State(state.clone()),
            Path(id.into()),
            Query(WorldNameQuery { world_name: None }),
        )
        .await
        .unwrap();
        assert!(status.complete);
        let Json(manifest) =
            crate::handlers::worlds::get_world_manifest(State(state), Path("world.dcl.eth".into()))
                .await
                .unwrap();
        assert_eq!(manifest.occupied, vec!["0,0", "1,0"]);
        assert_eq!(manifest.total, 2);
    }

    #[test]
    fn status_matches_registry_platform_contract_for_worlds_and_scenes() {
        use crate::manifest_store::PlatformManifest;
        let manifest = || PlatformManifest {
            version: None,
            build_date: None,
            exit_code: 0,
        };
        let mut manifests = AbManifests {
            mac: Some(manifest()),
            windows: Some(manifest()),
            ..Default::default()
        };
        for is_world in [false, true] {
            assert_eq!(
                serde_json::to_value(entity_status_from("entity", &manifests, is_world)).unwrap(),
                json!({
                    "entityId": "entity", "catalyst": "complete", "complete": true,
                    "assetBundles": { "mac": "complete", "windows": "complete" },
                    "lods": { "mac": "pending", "windows": "pending" }
                })
            );
        }
        manifests.windows.as_mut().unwrap().exit_code = 1;
        assert!(!entity_status_from("entity", &manifests, false).complete);
        manifests.windows = None;
        assert!(!entity_status_from("entity", &manifests, false).complete);
    }

    #[tokio::test]
    async fn status_by_id_reads_out_root_manifests() {
        let tmp = tmp_root("byid");
        let id = "bafkstatusentity";
        for platform in ["windows", "mac"] {
            write_manifest(&tmp, id, platform, 0);
        }
        let state = open_state(vec![entity(id, "scene", &["0,0"], json!({}))], &tmp);

        let Json(got) = get_entity_status(
            State(state.clone()),
            Path(id.to_string()),
            Query(WorldNameQuery { world_name: None }),
        )
        .await
        .unwrap();
        assert_eq!(got.entity_id, id);
        assert!(got.complete);
        assert!(matches!(got.asset_bundles.windows, BuildStatus::Complete));
        assert!(matches!(got.asset_bundles.mac, BuildStatus::Complete));
        assert!(got.lods.is_some());

        let missing = get_entity_status(
            State(state),
            Path("bafkunknownentity".to_string()),
            Query(WorldNameQuery { world_name: None }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(missing.into_response().status().as_u16(), 404);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn status_by_id_reads_jit_fallback_manifests() {
        let out_root = tmp_root("jitout");
        let jit_root = tmp_root("jitfb");
        let id = "bafkjitstatusentity";
        for platform in ["windows", "mac"] {
            write_manifest(&jit_root, id, platform, 0);
        }
        let state = open_state_dual(
            vec![entity(id, "scene", &["0,0"], json!({}))],
            &out_root,
            &jit_root,
        );

        let Json(got) = get_entity_status(
            State(state),
            Path(id.to_string()),
            Query(WorldNameQuery { world_name: None }),
        )
        .await
        .unwrap();
        assert_eq!(got.entity_id, id);
        assert!(got.complete);
        assert!(matches!(got.asset_bundles.windows, BuildStatus::Complete));
        assert!(matches!(got.asset_bundles.mac, BuildStatus::Complete));

        let _ = std::fs::remove_dir_all(&out_root);
        let _ = std::fs::remove_dir_all(&jit_root);
    }

    #[tokio::test]
    async fn status_id_precedes_world_filter_and_pointer_chooses_latest_in_scope() {
        let tmp = tmp_root("resolution");
        let mut old = entity("old", "scene", &["0,0"], json!({}));
        old.timestamp = 1;
        let mut latest = entity("latest", "scene", &["0,0"], json!({}));
        latest.timestamp = 2;
        let mut world = entity(
            "world",
            "scene",
            &["0,0"],
            json!({
                "worldConfiguration": {"name": "world.dcl.eth"}
            }),
        );
        world.timestamp = 3;
        let state = open_state(vec![world, old, latest], &tmp);
        for (selector, world_name, expected) in [
            ("0,0", None, "latest"),
            ("0,0", Some("WORLD.dcl.eth"), "world"),
            ("latest", Some("other.dcl.eth"), "latest"),
        ] {
            let Json(status) = get_entity_status(
                State(state.clone()),
                Path(selector.to_string()),
                Query(WorldNameQuery {
                    world_name: world_name.map(str::to_string),
                }),
            )
            .await
            .unwrap();
            assert_eq!(status.entity_id, expected);
        }
    }

    #[tokio::test]
    async fn status_world_name_filters_pointer_matches() {
        let tmp = tmp_root("world");
        let pointer = "myworld.dcl.eth";
        let hit = entity(
            "bafkworldhit12",
            "scene",
            &[pointer],
            json!({"worldConfiguration": {"name": "myworld.dcl.eth"}}),
        );
        let other = entity(
            "bafkworldother",
            "scene",
            &[pointer],
            json!({"worldConfiguration": {"name": "other.dcl.eth"}}),
        );
        let state = open_state(vec![other, hit], &tmp);

        let Json(got) = get_entity_status(
            State(state.clone()),
            Path(pointer.to_string()),
            Query(WorldNameQuery {
                world_name: Some("MYWORLD.dcl.eth".to_string()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(got.entity_id, "bafkworldhit12");
        assert!(got.lods.is_some());

        let miss = get_entity_status(
            State(state),
            Path(pointer.to_string()),
            Query(WorldNameQuery {
                world_name: Some("nosuch.dcl.eth".to_string()),
            }),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(miss.into_response().status().as_u16(), 404);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
