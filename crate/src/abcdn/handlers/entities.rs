use super::*;

const MAX_POINTERS: usize = 200;

fn parse_pointers(body: &serde_json::Value) -> Result<Vec<String>, (StatusCode, &'static str)> {
    let pointers: Vec<String> = body
        .get("pointers")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if pointers.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "pointers must be a non-empty array",
        ));
    }
    if pointers.len() > MAX_POINTERS {
        return Err((StatusCode::BAD_REQUEST, "too many pointers"));
    }
    Ok(pointers)
}

struct ResolvedEntity {
    entity_id: String,
    entity_type: String,
    timestamp: i64,
    pointers: Vec<String>,
    content: Vec<(String, String)>,
    metadata: serde_json::Value,
    deployer: String,
}

impl ResolvedEntity {
    fn from_scene(s: crate::catalyst::Scene, timestamp: i64) -> Self {
        Self {
            entity_id: s.entity_id,
            entity_type: s.entity_type,
            timestamp,
            pointers: s.pointers,
            content: s.content.into_iter().map(|c| (c.file, c.hash)).collect(),
            metadata: s.metadata,
            deployer: String::new(),
        }
    }

    fn from_active(e: dcl_contents::types::ActiveEntity) -> Self {
        Self {
            entity_id: e.entity_id,
            entity_type: e.entity_type,
            timestamp: e.timestamp,
            pointers: e.pointers,
            content: e.content.into_iter().map(|c| (c.file, c.hash)).collect(),
            metadata: e.metadata,
            deployer: e
                .deployer_address
                .map(|d| d.to_lowercase())
                .unwrap_or_default(),
        }
    }
}

fn feed_hash_index(state: &AppState, ents: &[ResolvedEntity]) {
    if let Some(proxy) = &state.live_proxy {
        proxy.index_content_hashes(ents.iter().flat_map(|e| {
            e.content
                .iter()
                .map(|(_, h)| (h.clone(), e.entity_id.clone()))
        }));
    }
}

async fn resolve_entities(
    state: &AppState,
    pointers: Vec<String>,
) -> Result<Vec<ResolvedEntity>, Response> {
    let ents = fetch_entities(state, &pointers).await?;
    let ents = active_by_pointer(&pointers, ents);
    feed_hash_index(state, &ents);
    Ok(ents)
}

fn resolver_unavailable() -> Response {
    (StatusCode::BAD_GATEWAY, "entity resolver unavailable").into_response()
}

async fn fetch_entities(
    state: &AppState,
    pointers: &[String],
) -> Result<Vec<ResolvedEntity>, Response> {
    if let Some(cdb) = &state.content_db {
        return match cdb.resolve_pointers(pointers).await {
            Ok(ents) => Ok(ents
                .into_iter()
                .map(|e| ResolvedEntity {
                    entity_id: e.entity_id,
                    entity_type: e.entity_type,
                    timestamp: e.timestamp,
                    pointers: e.pointers,
                    content: e.content.into_iter().map(|c| (c.file, c.hash)).collect(),
                    metadata: e.metadata,
                    deployer: e
                        .deployer_address
                        .map(|d| d.to_lowercase())
                        .unwrap_or_default(),
                })
                .collect()),
            Err(e) => {
                tracing::warn!(error = %e, "folded index: content-db resolve_pointers failed");
                Err(resolver_unavailable())
            }
        };
    }

    if let Some(registry) = &state.contents_registry {
        return match registry.content.resolve_pointers(pointers).await {
            Ok(actives) => Ok(actives
                .into_iter()
                .map(ResolvedEntity::from_active)
                .collect()),
            Err(e) => {
                tracing::warn!(error = %e, "registry proxy resolve_pointers failed");
                Err(resolver_unavailable())
            }
        };
    }

    let st = state.clone();
    let pts = pointers.to_vec();
    Ok(tokio::task::spawn_blocking(move || {
        pts.iter()
            .filter_map(|p| {
                let s = st.content.resolve_scene(p).ok()?;
                Some(ResolvedEntity::from_scene(s, 0))
            })
            .collect()
    })
    .await
    .unwrap_or_default())
}

fn active_by_pointer(pointers: &[String], ents: Vec<ResolvedEntity>) -> Vec<ResolvedEntity> {
    use std::collections::{HashMap, HashSet};

    let wanted: Vec<String> = pointers.iter().map(|p| p.trim().to_lowercase()).collect();
    let lowered: Vec<Vec<String>> = ents
        .iter()
        .map(|e| e.pointers.iter().map(|p| p.trim().to_lowercase()).collect())
        .collect();
    let ids: Vec<String> = ents.iter().map(|e| e.entity_id.to_lowercase()).collect();

    let mut winner: HashMap<&str, usize> = HashMap::new();
    for w in &wanted {
        for i in 0..ents.len() {
            if !(lowered[i].iter().any(|p| p == w) || ids[i] == *w) {
                continue;
            }
            let better = match winner.get(w.as_str()) {
                None => true,
                Some(&j) => {
                    (ents[i].timestamp, &ents[i].entity_id)
                        > (ents[j].timestamp, &ents[j].entity_id)
                }
            };
            if better {
                winner.insert(w.as_str(), i);
            }
        }
    }

    let keep: HashSet<usize> = winner.into_values().collect();
    ents.into_iter()
        .enumerate()
        .filter_map(|(i, e)| keep.contains(&i).then_some(e))
        .collect()
}

pub(super) fn valid_world_name(name: &str) -> bool {
    resolver::is_safe_component(name)
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

async fn resolve_world_entities(
    state: &AppState,
    name: &str,
    pointers: &[String],
) -> Result<Vec<ResolvedEntity>, Response> {
    let Some(url) = state.worlds_content_url.clone() else {
        tracing::warn!(
            world = %name,
            "world_name given but the worlds content lane is disabled — falling back to pointer resolution"
        );
        return resolve_entities(state, pointers.to_vec()).await;
    };
    if !valid_world_name(name) {
        return Ok(Vec::new());
    }
    let name_q = name.to_string();
    let secs = crate::worlds::SERVE_FETCH_TIMEOUT_SECS;
    let fetched = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<serde_json::Value>> {
        let scenes = crate::worlds::resolve_world_bounded(&url, &name_q, secs)?;
        Ok(scenes
            .iter()
            .filter_map(|s| match crate::worlds::fetch_scene_entity(s, secs) {
                Ok(v) => Some(v),
                Err(e) => {
                    tracing::warn!(
                        entity = %s.entity_id,
                        error = %format!("{e:#}"),
                        "world scene entity fetch failed"
                    );
                    None
                }
            })
            .collect())
    })
    .await;
    let raw_entities = match fetched {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::warn!(world = %name, error = %format!("{e:#}"), "world resolution failed");
            return Err(resolver_unavailable());
        }
        Err(e) => {
            tracing::error!(error = %e, "world resolution worker panicked");
            return Err(resolver_unavailable());
        }
    };
    let wanted: std::collections::HashSet<String> =
        pointers.iter().map(|p| p.trim().to_lowercase()).collect();
    let mut out = Vec::new();
    for v in raw_entities {
        let Ok(scene) = crate::catalyst::CatalystClient::parse_entity(&v) else {
            continue;
        };
        if !scene
            .pointers
            .iter()
            .any(|p| wanted.contains(&p.trim().to_lowercase()))
        {
            continue;
        }
        let timestamp = v.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);
        out.push(ResolvedEntity::from_scene(scene, timestamp));
    }
    feed_hash_index(state, &out);
    Ok(out)
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
            let rel = format!("{plat}.manifest.json");
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

/// versions/bundles/status for one entity, ready to embed in a registry response.
type AbRecordJson = (serde_json::Value, serde_json::Value, serde_json::Value);

fn is_parcel_pointer(p: &str) -> bool {
    match p.split_once(',') {
        Some((x, y)) => x.trim().parse::<i64>().is_ok() && y.trim().parse::<i64>().is_ok(),
        None => false,
    }
}

fn upstream_registry_fetch(
    base: &str,
    pointers: &[String],
) -> anyhow::Result<Vec<serde_json::Value>> {
    use std::io::Read as _;
    let url = format!("{base}/entities/active");
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();
    let resp = agent
        .post(&url)
        .header("User-Agent", crate::catalyst::UA)
        .header("Content-Type", "application/json")
        .send(serde_json::json!({ "pointers": pointers }).to_string())?;
    let mut buf: Vec<u8> = Vec::new();
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    let parsed: serde_json::Value = serde_json::from_slice(&buf)?;
    parsed
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{url}: response is not an array"))
}

/// Resolves the asset-bundle record for each entity: locally built bundles
/// win; entities without local bundles are answered with the upstream
/// registry's record (the versions the production CDN actually serves) when
/// one is configured; only entities the upstream does not know — and every
/// entity when no upstream registry is configured — fall back to this
/// server's own version, the promise that a request will JIT-build them.
/// Entities absent from the map get no record, mirroring how the production
/// registry omits unconverted entities.
async fn ab_records_for(
    state: &AppState,
    ents: &[ResolvedEntity],
) -> std::collections::HashMap<String, AbRecordJson> {
    use std::collections::HashMap;

    let st = state.clone();
    let probe: Vec<(String, bool)> = ents
        .iter()
        .map(|e| (e.entity_id.clone(), entity_buildable(&e.content)))
        .collect();
    // (local bundles present?, record with the JIT-promise fabrication)
    let local: HashMap<String, (bool, Option<AbRecordJson>)> =
        tokio::task::spawn_blocking(move || {
            probe
                .into_iter()
                .map(|(id, buildable)| {
                    let nofab = super::index::entity_ab_record(
                        &st.out_root,
                        &st.bundle_index,
                        &id,
                        false,
                        &st.ab_version,
                        &st.ab_date,
                    );
                    let fab = super::index::entity_ab_record(
                        &st.out_root,
                        &st.bundle_index,
                        &id,
                        buildable,
                        &st.ab_version,
                        &st.ab_date,
                    )
                    .map(|(v, b, s)| (v, b, serde_json::Value::from(s)));
                    (id, (nofab.is_some(), fab))
                })
                .collect()
        })
        .await
        .unwrap_or_default();

    let registry = state.upstream_ab_registry.clone();
    let mut out: HashMap<String, AbRecordJson> = HashMap::new();
    let mut ask_upstream: Vec<&ResolvedEntity> = Vec::new();
    // Entities the upstream registry authoritatively does not know keep no
    // record at all; `true` here means "decided, possibly with no record".
    let mut decided: HashMap<String, bool> = HashMap::new();

    for e in ents {
        let (has_local, fab) = match local.get(&e.entity_id) {
            Some(v) => v.clone(),
            None => continue,
        };
        if has_local {
            if let Some(rec) = fab {
                out.insert(e.entity_id.clone(), rec);
            }
            decided.insert(e.entity_id.clone(), true);
            continue;
        }
        // The local corpus (preview-server entities) can never exist upstream.
        if registry.is_none() || e.entity_id.starts_with("b64-") {
            if let Some(rec) = fab {
                out.insert(e.entity_id.clone(), rec);
            }
            decided.insert(e.entity_id.clone(), true);
            continue;
        }
        match state.upstream_registry_cache.get(&e.entity_id).await {
            Some(Some(rec)) => {
                out.insert(e.entity_id.clone(), (rec.versions, rec.bundles, rec.status));
                decided.insert(e.entity_id.clone(), true);
            }
            Some(None) => {
                decided.insert(e.entity_id.clone(), true);
            }
            None => ask_upstream.push(e),
        }
    }

    if let (Some(base), false) = (registry, ask_upstream.is_empty()) {
        let mut pointers: Vec<String> = ask_upstream
            .iter()
            .flat_map(|e| e.pointers.iter())
            .filter(|p| !is_parcel_pointer(p))
            .cloned()
            .collect();
        pointers.sort();
        pointers.dedup();
        let fetched = if pointers.is_empty() {
            Ok(Vec::new())
        } else {
            tokio::task::spawn_blocking(move || upstream_registry_fetch(&base, &pointers))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("registry fetch panicked: {e}")))
        };
        match fetched {
            Ok(records) => {
                let by_id: HashMap<&str, &serde_json::Value> = records
                    .iter()
                    .filter_map(|r| r.get("id").and_then(|i| i.as_str()).map(|i| (i, r)))
                    .collect();
                for e in &ask_upstream {
                    let asked = e.pointers.iter().any(|p| !is_parcel_pointer(p));
                    let rec = by_id.get(e.entity_id.as_str()).and_then(|r| {
                        Some(crate::abcdn::state::UpstreamAbRecord {
                            versions: r.get("versions")?.clone(),
                            bundles: r.get("bundles").cloned().unwrap_or_default(),
                            status: r.get("status").cloned().unwrap_or_default(),
                        })
                    });
                    if !asked && rec.is_none() {
                        // none of its pointers were forwarded — not an
                        // authoritative absence, keep today's behavior
                        continue;
                    }
                    state
                        .upstream_registry_cache
                        .insert(e.entity_id.clone(), rec.clone())
                        .await;
                    if let Some(rec) = rec {
                        out.insert(e.entity_id.clone(), (rec.versions, rec.bundles, rec.status));
                    }
                    decided.insert(e.entity_id.clone(), true);
                }
            }
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "upstream ab registry fetch failed; answering with local records");
            }
        }
    }

    // Anything left undecided (upstream fetch failed) keeps today's behavior.
    for e in ents {
        if decided.contains_key(&e.entity_id) {
            continue;
        }
        if let Some((_, Some(rec))) = local.get(&e.entity_id).cloned() {
            out.insert(e.entity_id.clone(), rec);
        }
    }

    out
}

pub async fn post_entities_versions(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let pointers = match parse_pointers(&body) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let ents = match resolve_entities(&state, pointers).await {
        Ok(e) => e,
        Err(resp) => return resp,
    };
    eager_build_index(&state, &ents);

    let recs = ab_records_for(&state, &ents).await;
    let out: Vec<serde_json::Value> = ents
        .into_iter()
        .filter_map(|e| {
            let (versions, bundles, status) = recs.get(&e.entity_id)?.clone();
            Some(serde_json::json!({
                "pointers": e.pointers,
                "versions": versions,
                "bundles": bundles,
                "status": status,
            }))
        })
        .collect();

    Json(out).into_response()
}

pub async fn post_entities_active(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let pointers = match parse_pointers(&body) {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let world = query
        .get("world_name")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let ents = match world {
        Some(name) => resolve_world_entities(&state, &name, &pointers).await,
        None => resolve_entities(&state, pointers).await,
    };
    let ents = match ents {
        Ok(e) => e,
        Err(resp) => return resp,
    };
    eager_build_index(&state, &ents);
    entities_active_records(&state, ents).await
}

async fn entities_active_records(state: &AppState, ents: Vec<ResolvedEntity>) -> Response {
    let recs = ab_records_for(state, &ents).await;
    let out = {
        ents.into_iter()
            .filter_map(|e| {
                let (versions, bundles, status) = recs.get(&e.entity_id)?.clone();
                let content: Vec<serde_json::Value> = e
                    .content
                    .iter()
                    .map(|(f, h)| serde_json::json!({ "file": f, "hash": h }))
                    .collect();
                Some(serde_json::json!({
                    "id": e.entity_id,
                    "type": e.entity_type,
                    "timestamp": e.timestamp,
                    "pointers": e.pointers,
                    "content": content,
                    "metadata": e.metadata,
                    "deployer": e.deployer,
                    "status": status,
                    "bundles": bundles,
                    "versions": versions,
                }))
            })
            .collect::<Vec<_>>()
    };

    Json(out).into_response()
}

#[cfg(test)]
mod active_tests {
    use super::{active_by_pointer, ResolvedEntity};

    fn ent(id: &str, ts: i64, pointers: &[&str]) -> ResolvedEntity {
        ResolvedEntity {
            entity_id: id.to_string(),
            entity_type: "scene".to_string(),
            timestamp: ts,
            pointers: pointers.iter().map(|p| p.to_string()).collect(),
            content: Vec::new(),
            metadata: serde_json::Value::Null,
            deployer: String::new(),
        }
    }

    #[test]
    fn collapses_overlapping_deployments_to_newest() {
        let ents = vec![
            ent("older", 100, &["-3,-2", "0,0"]),
            ent("newest", 300, &["-3,-2", "0,0"]),
            ent("mid", 200, &["-3,-2", "0,0"]),
        ];
        let out = active_by_pointer(&["-3,-2".to_string()], ents);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entity_id, "newest");
    }

    #[test]
    fn keeps_one_winner_per_distinct_pointer() {
        let ents = vec![
            ent("a", 100, &["-3,-2"]),
            ent("a2", 200, &["-3,-2"]),
            ent("b", 100, &["5,5"]),
        ];
        let mut out = active_by_pointer(&["-3,-2".to_string(), "5,5".to_string()], ents);
        out.sort_by(|x, y| x.entity_id.cmp(&y.entity_id));
        let ids: Vec<_> = out.iter().map(|e| e.entity_id.as_str()).collect();
        assert_eq!(ids, vec!["a2", "b"]);
    }

    #[test]
    fn resolves_entity_id_style_request() {
        let ents = vec![ent("bafyEntity", 100, &["-3,-2"])];
        let out = active_by_pointer(&["bafyentity".to_string()], ents);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].entity_id, "bafyEntity");
    }
}
