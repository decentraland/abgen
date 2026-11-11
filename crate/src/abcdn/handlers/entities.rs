use super::*;
use crate::registry::ports::records::RegistryRecord;
use dcl_contents::errors::{ApiError, ApiResult};
use dcl_contents::types::ActiveEntity;

fn parse_pointers(body: &serde_json::Value) -> ApiResult<Vec<String>> {
    let values = body
        .get("pointers")
        .and_then(|v| v.as_array())
        .filter(|values| !values.is_empty())
        .ok_or_else(|| ApiError::bad_request("No pointers provided"))?;
    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| ApiError::bad_request("pointers must be strings"))
        })
        .collect()
}

#[cfg(test)]
pub(super) fn valid_world_name(name: &str) -> bool {
    resolver::is_safe_component(name)
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

struct ResolvedEntity {
    entity_id: String,
    content: Vec<(String, String)>,
}

impl ResolvedEntity {
    fn from_active(entity: &ActiveEntity) -> Self {
        Self {
            entity_id: entity.entity_id.clone(),
            content: entity
                .content
                .iter()
                .map(|file| (file.file.clone(), file.hash.clone()))
                .collect(),
        }
    }
}

fn entity_buildable(content: &[(String, String)]) -> bool {
    content.iter().any(|(f, _)| super::index::is_convertible(f))
}

pub(super) struct PendingGuard(pub(super) std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

fn eager_build_index(state: &AppState, entities: &[ResolvedEntity]) {
    let ib = &state.index_build;
    if !ib.eager {
        return;
    }
    let Some(proxy) = state.live_proxy.clone() else {
        return;
    };
    let mut candidates: Vec<(String, String)> = Vec::new();
    for e in entities {
        if !entity_buildable(&e.content) {
            continue;
        }
        for platform in &ib.platforms {
            candidates.push((e.entity_id.clone(), platform.clone()));
        }
    }
    if candidates.is_empty() {
        return;
    }

    let sem = ib.sem.clone();
    let pending = ib.pending.clone();
    let max_queue = ib.max_queue;
    let deadline = ib.deadline;
    let st = state.clone();

    tokio::spawn(async move {
        let mut handles = Vec::new();
        for (ent, plat) in candidates {
            let key = format!("{ent}:{plat}");
            if st.jit_fail_cache.get(&key).await.is_some() {
                continue;
            }
            let rel = if plat == crate::bvwebgpu::BVW_PLATFORM {
                format!("{plat}/{}", crate::bvwebgpu::pack_file_name(&ent))
            } else {
                format!("{plat}.manifest.json")
            };
            let warm = st.out_root.join(&ent).join(&rel);
            let jit = st.jit_root.join(&ent).join(&rel);
            let (w, j) = (warm, jit.clone());
            let already = tokio::task::spawn_blocking(move || w.is_file() || j.is_file())
                .await
                .unwrap_or(false);
            if already {
                continue;
            }
            if max_queue > 0 && pending.load(std::sync::atomic::Ordering::Relaxed) >= max_queue {
                metrics::counter!("abgen_index_jit_skipped_total").increment(1);
                continue;
            }
            pending.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let guard = PendingGuard(pending.clone());
            let sem = sem.clone();
            let px = proxy.clone();
            let st = st.clone();
            handles.push(tokio::spawn(async move {
                let _guard = guard;
                let Ok(_permit) = sem.acquire_owned().await else {
                    return;
                };
                let _ = jit_build_entity(&st, &px, &ent, &plat, Some(jit), "index").await;
            }));
        }
        let deadline = tokio::time::Instant::now() + deadline;
        for h in handles {
            if tokio::time::timeout_at(deadline, h).await.is_err() {
                break;
            }
        }
    });
}

/// Refresh native conversion facts from this node's Catalyst history and local
/// converter output. Content bodies remain owned by Catalyst, not Registry.
async fn refresh_candidates(
    state: &AppState,
    pointers: &[String],
    world: Option<&str>,
) -> ApiResult<Vec<ActiveEntity>> {
    let store = state
        .registry_store
        .as_ref()
        .filter(|store| store.enabled())
        .ok_or_else(|| {
            ApiError::not_implemented("registry persistence requires the ab_registry DB")
        })?;
    let registry = state
        .contents_registry
        .as_ref()
        .ok_or_else(|| ApiError::upstream("local Catalyst source is unavailable"))?;
    let mut entities = match world {
        Some(world) => registry.content.resolve_world_current(world).await?,
        None => registry.content.resolve_candidates(pointers, None).await?,
    };
    entities.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then(a.entity_id.cmp(&b.entity_id))
    });
    for entity in &entities {
        store
            .refresh_and_rotate(entity, &registry.manifests)
            .await?;
    }
    // Eager conversion targets the current entity for each pointer, not all
    // historical versions retained for the ready-version fallback.
    let mut covered = std::collections::HashSet::new();
    let buildable: Vec<_> = entities
        .iter()
        .rev()
        .filter(|entity| {
            entity
                .pointers
                .iter()
                .any(|p| pointers.iter().any(|q| p.eq_ignore_ascii_case(q)))
        })
        .filter(|entity| {
            let mut any_new = false;
            for pointer in &entity.pointers {
                any_new |= covered.insert(pointer.clone());
            }
            any_new
        })
        .map(ResolvedEntity::from_active)
        .collect();
    eager_build_index(state, &buildable);
    Ok(entities)
}

fn versions_wire(record: &RegistryRecord) -> serde_json::Value {
    serde_json::json!({"assets": {
        "mac": record.versions.assets.mac,
        "windows": record.versions.assets.windows,
    }})
}

fn bundles_wire(record: &RegistryRecord) -> serde_json::Value {
    let mut value = serde_json::json!({"assets": {
        "mac": record.bundles.assets.mac,
        "windows": record.bundles.assets.windows,
    }});
    if let Some(lods) = &record.bundles.lods {
        value["lods"] = serde_json::json!({"mac":lods.mac,"windows":lods.windows});
    }
    value
}

fn distinct_pointer_sets(records: Vec<RegistryRecord>) -> Vec<RegistryRecord> {
    let mut seen = std::collections::HashSet::new();
    records
        .into_iter()
        .filter(|record| seen.insert(record.pointers.clone()))
        .collect()
}

// Worlds supplies the authoritative current catalogue. A historical ready
// conversion can cover a pending replacement, but cannot revive removed parcels.
fn current_world_records(
    records: Vec<RegistryRecord>,
    entities: &[ActiveEntity],
    world: Option<&str>,
) -> Vec<RegistryRecord> {
    if world.is_none() {
        return records;
    }
    let occupied: std::collections::HashSet<_> = entities
        .iter()
        .flat_map(|entity| entity.pointers.iter())
        .collect();
    records
        .into_iter()
        .filter(|record| {
            !record.pointers.is_empty()
                && record
                    .pointers
                    .iter()
                    .all(|pointer| occupied.contains(pointer))
        })
        .collect()
}

pub async fn post_entities_versions(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let pointers = parse_pointers(&body)?;
    let world = query
        .get("world_name")
        .map(String::as_str)
        .filter(|w| !w.is_empty());
    let entities = refresh_candidates(&state, &pointers, world).await?;
    let store = state
        .registry_store
        .as_ref()
        .expect("refresh requires registry store");
    // The pinned Registry versions endpoint does not exclude denylisted IDs.
    let records = distinct_pointer_sets(current_world_records(
        store.records_by_pointers(&pointers, world, false).await?,
        &entities,
        world,
    ));
    Ok(Json(
        records
            .iter()
            .map(|record| {
                serde_json::json!({
                    "pointers":record.pointers,"versions":versions_wire(record),
                    "bundles":bundles_wire(record),"status":record.status,
                })
            })
            .collect(),
    ))
}

pub async fn post_entities_active(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<Vec<serde_json::Value>>> {
    let pointers = parse_pointers(&body)?;
    let world = query
        .get("world_name")
        .map(String::as_str)
        .filter(|w| !w.is_empty());
    let entities = refresh_candidates(&state, &pointers, world).await?;
    let registry = state
        .contents_registry
        .as_ref()
        .expect("refresh requires content source");
    let store = state
        .registry_store
        .as_ref()
        .expect("refresh requires registry store");
    let records = distinct_pointer_sets(current_world_records(
        store.records_by_pointers(&pointers, world, true).await?,
        &entities,
        world,
    ));
    let mut result = Vec::new();
    for record in records {
        let entity = match entities.iter().find(|e| e.entity_id == record.entity_id) {
            Some(entity) => entity.clone(),
            None => registry
                .content
                .resolve_one(&record.entity_id)
                .await?
                .ok_or_else(|| {
                    ApiError::upstream("registered entity is unavailable from Catalyst")
                })?,
        };
        result.push(serde_json::json!({"id":entity.entity_id,"type":entity.entity_type,
            "timestamp":entity.timestamp.to_string(),"pointers":entity.pointers,"content":entity.content,
            "metadata":entity.metadata,"deployer":entity.deployer_address.unwrap_or_default(),
            "status":record.status,"bundles":bundles_wire(&record),"versions":versions_wire(&record)}));
    }
    Ok(Json(result))
}

#[cfg(test)]
#[path = "entities_contract_tests.rs"]
mod persisted_contract_tests;

#[cfg(test)]
mod contract_tests {
    use super::*;
    #[test]
    fn selectors_follow_registry_pointers_contract() {
        for body in [
            serde_json::json!({}),
            serde_json::json!({"pointers":[]}),
            serde_json::json!({"ids":["id"]}),
        ] {
            assert!(parse_pointers(&body).is_err());
        }
        assert_eq!(
            parse_pointers(&serde_json::json!({"pointers":["0,0"],"ids":["ignored"]})).unwrap(),
            vec!["0,0"]
        );
        assert!(parse_pointers(&serde_json::json!({"pointers":[1]})).is_err());
    }
}
