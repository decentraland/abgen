use std::collections::BTreeSet;

use axum::extract::{Path, State};
use axum::Json;

use crate::errors::{ApiError, ApiResult};
use crate::registry::RegistryAppState;
use crate::types::{BuildStatus, Coordinate, WorldManifest};

pub async fn get_world_manifest(
    State(state): State<RegistryAppState>,
    Path(world_name): Path<String>,
) -> ApiResult<Json<WorldManifest>> {
    if !is_world_name_valid(&world_name) {
        return Err(ApiError::bad_request("A valid world name is required"));
    }

    let ents = state.content.resolve_world(&world_name).await?;
    let denylist = state.world_policy.denylist().await?;

    let mut occupied_set: BTreeSet<String> = BTreeSet::new();
    for ent in &ents {
        if ent.entity_type != "scene" {
            continue;
        }
        if ent
            .world_name()
            .is_none_or(|n| !n.eq_ignore_ascii_case(&world_name))
        {
            continue;
        }
        if denylist.contains(&ent.entity_id) {
            continue;
        }

        let complete = match state.content.registered_status(&ent.entity_id).await? {
            Some(status) => status.complete,
            None => {
                let m = state.manifests.get(&ent.entity_id).await;
                matches!(m.windows_status(), BuildStatus::Complete)
                    && matches!(m.mac_status(), BuildStatus::Complete)
            }
        };
        if !complete {
            continue;
        }
        occupied_set.extend(ent.pointers.iter().cloned());
    }

    let occupied: Vec<String> = occupied_set.into_iter().collect();
    let total = occupied.len();

    let spawn = compute_spawn(&state, &world_name, &occupied).await?;

    Ok(Json(WorldManifest {
        occupied,
        spawn_coordinate: spawn,
        total,
    }))
}

fn is_world_name_valid(name: &str) -> bool {
    let Some(mut head) = name.strip_suffix(".eth") else {
        return false;
    };

    if let Some(h) = head.strip_suffix(".dcl") {
        head = h;
    }
    !head.is_empty() && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

async fn compute_spawn(
    state: &RegistryAppState,
    world_name: &str,
    occupied: &[String],
) -> Result<Coordinate, ApiError> {
    if occupied.is_empty() {
        return Ok(Coordinate { x: 0, y: 0 });
    }
    let bounds = bounds_from_parcels(occupied);
    if let Some((x, y)) = state.world_policy.spawn_override(world_name).await? {
        let c = Coordinate { x, y };
        if in_bounds(&c, &bounds) {
            return Ok(c);
        }
    }
    Ok(centroid_parcel(occupied))
}

fn bounds_from_parcels(parcels: &[String]) -> (i64, i64, i64, i64) {
    let mut it = parcels.iter().filter_map(|p| parse_coordinate(p));
    let Some(first) = it.next() else {
        return (0, 0, 0, 0);
    };
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (first.x, first.y, first.x, first.y);
    for c in it {
        min_x = min_x.min(c.x);
        min_y = min_y.min(c.y);
        max_x = max_x.max(c.x);
        max_y = max_y.max(c.y);
    }
    (min_x, min_y, max_x, max_y)
}

fn in_bounds(c: &Coordinate, b: &(i64, i64, i64, i64)) -> bool {
    c.x >= b.0 && c.x <= b.2 && c.y >= b.1 && c.y <= b.3
}

fn centroid_parcel(parcels: &[String]) -> Coordinate {
    let coords: Vec<Coordinate> = parcels.iter().filter_map(|p| parse_coordinate(p)).collect();
    if coords.is_empty() {
        return Coordinate { x: 0, y: 0 };
    }
    let n = coords.len() as f64;
    let cx = coords.iter().map(|c| c.x as f64).sum::<f64>() / n;
    let cy = coords.iter().map(|c| c.y as f64).sum::<f64>() / n;
    let mut best = coords[0];
    let mut best_d = f64::INFINITY;
    for c in &coords {
        let d = (c.x as f64 - cx).powi(2) + (c.y as f64 - cy).powi(2);
        if d < best_d {
            best_d = d;
            best = *c;
        }
    }
    best
}

fn parse_coordinate(s: &str) -> Option<Coordinate> {
    let mut it = s.split(',');
    let x = it.next()?.trim().parse().ok()?;
    let y = it.next()?.trim().parse().ok()?;
    Some(Coordinate { x, y })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(ss: &[&str]) -> Vec<String> {
        ss.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn world_name_validation_matches_upstream_regex() {
        for ok in [
            "foo.eth",
            "foo.dcl.eth",
            "my-world.dcl.eth",
            "UPPER.eth",
            "a.eth",
            "1.dcl.eth",
        ] {
            assert!(is_world_name_valid(ok), "should be valid: {ok}");
        }
        for bad in [
            "invalid",
            "notaworld.com",
            "",
            "bad_name.eth",
            "foo.bar.eth",
            ".eth",
            ".dcl.eth",
            "foo.dcl.dcl.eth",
            "foo..eth",
            "foo.ETH",
            "foo.DCL.eth",
        ] {
            assert!(!is_world_name_valid(bad), "should be invalid: {bad}");
        }
    }

    #[test]
    fn parse_coordinate_handles_signs_whitespace_and_garbage() {
        assert_eq!(parse_coordinate("1,2").map(|c| (c.x, c.y)), Some((1, 2)));
        assert_eq!(
            parse_coordinate("-5,-9").map(|c| (c.x, c.y)),
            Some((-5, -9))
        );
        assert_eq!(
            parse_coordinate(" 3 , 4 ").map(|c| (c.x, c.y)),
            Some((3, 4))
        );
        assert!(parse_coordinate("x,y").is_none());
        assert!(parse_coordinate("1").is_none());
        assert!(parse_coordinate("").is_none());
    }

    #[test]
    fn bounds_are_min_max_over_all_parcels() {
        assert_eq!(
            bounds_from_parcels(&v(&["0,0", "10,4", "-2,6"])),
            (-2, 0, 10, 6)
        );
        assert_eq!(bounds_from_parcels(&v(&["5,5"])), (5, 5, 5, 5));
        assert_eq!(bounds_from_parcels(&[]), (0, 0, 0, 0));
    }

    #[test]
    fn in_bounds_is_inclusive_rectangle() {
        let b = (-2, 0, 10, 6);
        assert!(in_bounds(&Coordinate { x: -2, y: 0 }, &b));
        assert!(in_bounds(&Coordinate { x: 10, y: 6 }, &b));
        assert!(in_bounds(&Coordinate { x: 4, y: 3 }, &b));
        assert!(!in_bounds(&Coordinate { x: -3, y: 3 }, &b));
        assert!(!in_bounds(&Coordinate { x: 4, y: 7 }, &b));
    }

    #[test]
    fn centroid_returns_occupied_parcel_closest_to_average() {
        let c = centroid_parcel(&v(&["0,0", "4,0", "2,2"]));
        assert_eq!((c.x, c.y), (2, 2));

        let c = centroid_parcel(&v(&["7,-3"]));
        assert_eq!((c.x, c.y), (7, -3));

        let c = centroid_parcel(&[]);
        assert_eq!((c.x, c.y), (0, 0));

        let set = v(&["0,0", "1,0", "5,5"]);
        let c = centroid_parcel(&set);
        assert!(set.contains(&format!("{},{}", c.x, c.y)));
    }

    mod manifest {
        use std::collections::HashSet;
        use std::sync::Arc;

        use axum::extract::{Path, State};
        use axum::response::IntoResponse;
        use serde_json::json;

        use super::super::get_world_manifest;
        use crate::registry::testutil::{entity, open_state, state_with, StubPolicy};
        use crate::registry::{RegistryAppState, WorldPolicy};
        use crate::types::ActiveEntity;

        fn world_scene(id: &str, name: &str) -> ActiveEntity {
            entity(
                id,
                "scene",
                &["1,1", "1,2"],
                json!({
                    "worldConfiguration": {"name": name},
                    "scene": {"base": "1,1", "parcels": ["1,1", "1,2"]},
                }),
            )
        }

        fn tmp_root(tag: &str) -> std::path::PathBuf {
            std::env::temp_dir().join(format!("dclc_worlds_{tag}_{}", std::process::id()))
        }

        fn ready_state(entities: Vec<ActiveEntity>, root: &std::path::Path) -> RegistryAppState {
            prepare_manifests(&entities, root);
            open_state(entities, root)
        }

        fn ready_state_with_policy(
            entities: Vec<ActiveEntity>,
            policy: Arc<dyn WorldPolicy>,
            root: &std::path::Path,
        ) -> RegistryAppState {
            prepare_manifests(&entities, root);
            state_with(entities, policy, root)
        }

        fn prepare_manifests(entities: &[ActiveEntity], root: &std::path::Path) {
            for entity in entities {
                let dir = root.join(&entity.entity_id);
                std::fs::create_dir_all(&dir).unwrap();
                for platform in ["mac", "windows"] {
                    std::fs::write(
                        dir.join(format!("{platform}.manifest.json")),
                        r#"{"version":"v1","exitCode":0}"#,
                    )
                    .unwrap();
                }
            }
        }

        #[tokio::test]
        async fn pending_world_is_empty_until_both_platforms_complete() {
            let tmp = tmp_root("pending");
            let scene = world_scene("bafkworldpending", "myworld.dcl.eth");
            let dir = tmp.join(&scene.entity_id);
            let _ = std::fs::remove_dir_all(&tmp);
            let state = open_state(vec![scene.clone()], &tmp);
            let axum::Json(manifest) =
                get_world_manifest(State(state), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!(manifest.total, 0);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("mac.manifest.json"), r#"{"exitCode":0}"#).unwrap();
            let state = open_state(vec![scene.clone()], &tmp);
            let axum::Json(manifest) =
                get_world_manifest(State(state), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!(manifest.total, 0);
            let state = ready_state(vec![scene], &tmp);
            let axum::Json(manifest) =
                get_world_manifest(State(state), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!(manifest.total, 2);
            let _ = std::fs::remove_dir_all(&tmp);
        }

        #[tokio::test]
        async fn happy_path_returns_occupied_and_centroid_spawn() {
            let tmp = tmp_root("happy");
            let state = ready_state(vec![world_scene("bafkworldhappy", "myworld.dcl.eth")], &tmp);

            let axum::Json(m) =
                get_world_manifest(State(state), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!(m.occupied, vec!["1,1".to_string(), "1,2".to_string()]);
            assert_eq!((m.spawn_coordinate.x, m.spawn_coordinate.y), (1, 1));
            assert_eq!(m.total, 2);
        }

        #[tokio::test]
        async fn denylisted_scene_yields_empty_manifest() {
            let tmp = tmp_root("deny");
            let state = ready_state_with_policy(
                vec![world_scene("bafkworlddenied", "myworld.dcl.eth")],
                Arc::new(StubPolicy {
                    denylist: HashSet::from(["bafkworlddenied".to_string()]),
                    spawn: None,
                }),
                &tmp,
            );

            let axum::Json(manifest) =
                get_world_manifest(State(state), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!(
                serde_json::to_value(manifest).unwrap(),
                json!({
                    "occupied": [], "spawn_coordinate": { "x": 0, "y": 0 }, "total": 0
                })
            );
        }

        #[tokio::test]
        async fn unknown_world_returns_empty_manifest_and_invalid_name_400() {
            let tmp = tmp_root("miss");
            let state = ready_state(Vec::new(), &tmp);

            let axum::Json(missing) = get_world_manifest(
                State(state.clone()),
                Path("nosuchworld.dcl.eth".to_string()),
            )
            .await
            .unwrap();
            assert_eq!(
                serde_json::to_value(missing).unwrap(),
                json!({
                    "occupied": [], "spawn_coordinate": { "x": 0, "y": 0 }, "total": 0
                })
            );

            let invalid = get_world_manifest(State(state), Path("not_a_world".to_string()))
                .await
                .err()
                .unwrap();
            assert_eq!(invalid.into_response().status().as_u16(), 400);
        }

        #[tokio::test]
        async fn spawn_uses_pointer_centroid_instead_of_metadata_base_and_parcels() {
            let tmp = tmp_root("centroid");
            let mut scene = world_scene("bafkworldcentroid", "myworld.dcl.eth");
            scene.pointers = vec!["0,0".into(), "4,0".into(), "2,2".into()];
            scene.metadata["scene"] = json!({
                "base": "0,0", "parcels": ["0,0"]
            });
            let state = ready_state(vec![scene], &tmp);
            let axum::Json(manifest) =
                get_world_manifest(State(state), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!(
                (manifest.spawn_coordinate.x, manifest.spawn_coordinate.y),
                (2, 2)
            );
        }

        #[tokio::test]
        async fn shared_parcel_survives_denylist_and_stored_spawn_may_be_between_parcels() {
            let tmp = tmp_root("shared");
            let mut denied = world_scene("bafkworlddeniedshared", "myworld.dcl.eth");
            denied.pointers = vec!["0,0".into(), "1,1".into()];
            let mut allowed = world_scene("bafkworldallowedshared", "myworld.dcl.eth");
            allowed.pointers = vec!["1,1".into(), "2,2".into()];
            let state = ready_state_with_policy(
                vec![denied, allowed],
                Arc::new(StubPolicy {
                    denylist: HashSet::from(["bafkworlddeniedshared".into()]),
                    spawn: Some((1, 2)),
                }),
                &tmp,
            );
            let axum::Json(manifest) =
                get_world_manifest(State(state), Path("myworld.dcl.eth".into()))
                    .await
                    .unwrap();
            assert_eq!(
                serde_json::to_value(manifest).unwrap(),
                json!({
                    "occupied": ["1,1", "2,2"], "spawn_coordinate": {"x": 1, "y": 2}, "total": 2
                })
            );
            let _ = std::fs::remove_dir_all(tmp);
        }

        #[tokio::test]
        async fn spawn_override_applies_only_in_bounds() {
            let tmp = tmp_root("spawn");
            let mk = |spawn| {
                ready_state_with_policy(
                    vec![world_scene("bafkworldspawn", "myworld.dcl.eth")],
                    Arc::new(StubPolicy {
                        denylist: HashSet::new(),
                        spawn,
                    }),
                    &tmp,
                )
            };

            let axum::Json(m) =
                get_world_manifest(State(mk(Some((1, 2)))), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!((m.spawn_coordinate.x, m.spawn_coordinate.y), (1, 2));

            let axum::Json(m) =
                get_world_manifest(State(mk(Some((9, 9)))), Path("myworld.dcl.eth".to_string()))
                    .await
                    .unwrap();
            assert_eq!((m.spawn_coordinate.x, m.spawn_coordinate.y), (1, 1));
        }
    }
}
