use crate::config::Config;
use abgen::live::Proxy;
use abgen::lodgen::reuse::{self, record_for_build, Inputs, ReuseRecord};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Build a scene's LODs from its entity and publish them under its scene id, or republish
/// a previous build's when the reuse index says a build would produce the same bytes.
/// The same lodgen chain the abcdn serves JIT.
fn convert(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    entity_id: &str,
    content_server: &str,
) -> Result<serde_json::Value> {
    let (platforms, rejected) = supported_platforms(&cfg.platforms);
    if !rejected.is_empty() {
        eprintln!(
            "lods: {entity_id}: platform(s) {} have no LOD lane, skipping them",
            rejected.join(",")
        );
    }
    if platforms.is_empty() {
        return Ok(serde_json::json!({
            "entityId": entity_id, "skipped": "lods-no-supported-platform"
        }));
    }

    let staging = cfg
        .out_root
        .join("lod")
        .join(&*abgen::naming::fs_safe_component(entity_id));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).with_context(|| format!("mkdir {}", staging.display()))?;
    let guard = StagingGuard {
        path: staging.clone(),
        keep: cfg.keep_output,
    };

    let params = generate_params(cfg, entity_id, content_server, &platforms, &staging);
    let started = std::time::Instant::now();

    // Resolved once here and again inside `generate`; the second resolution is a content
    // cache hit. The inputs are needed on both paths: to look a previous build up, and to
    // file this one under when it builds.
    let (client, ent) = abgen::lodgen::resolve_scene(&params)?;
    let inputs = match reuse::inputs(&ent) {
        Ok(inputs) => inputs,
        Err(e) => {
            eprintln!("lods: {entity_id}: no reusable inputs ({e}); the state alone decides");
            None
        }
    };

    match try_reuse(
        cfg,
        proxy,
        &client,
        &ent,
        &platforms,
        &params,
        inputs.as_ref(),
    ) {
        Ok(Some(summary)) => {
            drop(guard);
            return Ok(summary);
        }
        Ok(None) => {}
        // Reuse is an optimization: a record that will not parse, an object that has gone
        // missing or a scene that will not execute must cost a rebuild, never a job.
        Err(e) => eprintln!("lods: {entity_id}: reuse check failed ({e}); building"),
    }

    let outcome = abgen::lodgen::generate(&params)
        .with_context(|| format!("generate LOD bundles for {entity_id}"))?;

    let failures = abgen::lodgen::gate_failures(&outcome.gate);
    if failures > 0 {
        let first = outcome
            .gate
            .iter()
            .find(|c| !c.ok)
            .map(|c| format!("{}: {}", c.label, c.detail))
            .unwrap_or_default();
        bail!("LOD self-gate failed for {entity_id} ({failures} check(s)): {first}");
    }

    let scene_dir = staging.join(&outcome.scene_id);
    let objects = abgen::lods::published_objects(&scene_dir, &cfg.lod_levels);
    let published = publish(cfg, proxy, &objects)?;
    if published.uploaded {
        let record = record_for_build(
            &outcome,
            &published.keys,
            &cfg.lod_levels,
            &platforms,
            inputs,
        );
        publish_record(proxy, &record);
    }

    let bundle_bytes: usize = outcome.levels.iter().map(|l| l.bundle_bytes).sum();
    eprintln!(
        "done: {entity_id} lods scene={} levels={} platforms={} bytes={bundle_bytes} \
         objects={} uploaded={} in {:.1}s",
        outcome.scene_id,
        outcome
            .levels
            .iter()
            .map(|l| l.level.to_string())
            .collect::<Vec<_>>()
            .join(","),
        platforms.join(","),
        objects.len(),
        published.uploaded,
        started.elapsed().as_secs_f64(),
    );
    drop(guard);

    let levels: Vec<(u32, usize)> = outcome
        .levels
        .iter()
        .map(|l| (l.level, l.bundle_bytes))
        .collect();
    let mut summary = success_summary(
        entity_id,
        &outcome.scene_id,
        &platforms,
        &levels,
        objects.len(),
        published.uploaded,
    );
    summary["lods"]["keys"] = serde_json::json!(published.keys);
    Ok(summary)
}

/// Run the LOD lane for a scene whose asset bundles a conversion job just finished with,
/// so one deployment's bundles and LODs land in one go. This is the only way LODs are
/// built: nothing else asks for them.
///
/// `None` when there is nothing to do: LODs are off, or the entity is not a scene. Otherwise
/// the LOD summary, flattened for nesting under the conversion summary's `lods` key. A LOD
/// failure is logged and reported there and never fails the job, whose bundles are already
/// published and notified; the scene's next deployment is the next attempt.
pub fn follow_up(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    entity_id: &str,
    content_server: &str,
) -> Option<serde_json::Value> {
    if !cfg.lods_enabled {
        return None;
    }
    let agent = crate::catalyst::agent();
    let entity = match crate::catalyst::fetch_entity(&agent, content_server, entity_id) {
        Ok(entity) => entity,
        Err(e) => {
            eprintln!("lods: {entity_id}: follow-up could not resolve the entity ({e:#})");
            return Some(follow_up_summary(Err(e)));
        }
    };
    if !is_scene(&entity) {
        return None;
    }
    let result = convert(cfg, proxy, entity_id, content_server);
    if let Err(e) = &result {
        eprintln!("lods: {entity_id}: LOD generation failed ({e:#}); the conversion stands");
    }
    let summary = follow_up_summary(result);
    let outcome = if summary.get("error").is_some() {
        "error"
    } else if summary.get("skipped").is_some() {
        "skipped"
    } else if summary.get("reusedBy").is_some() {
        "reused"
    } else {
        "converted"
    };
    metrics::counter!("abgen_lambda_lod_followup_total", "outcome" => outcome).increment(1);
    Some(summary)
}

pub fn is_scene(entity: &serde_json::Value) -> bool {
    entity.get("type").and_then(serde_json::Value::as_str) == Some("scene")
}

/// A LOD job's result as one object: the `lods` block with `exitCode`, `sceneId` and
/// folded in; a skip or an error as a single field.
pub fn follow_up_summary(result: Result<serde_json::Value>) -> serde_json::Value {
    match result {
        Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
        Ok(mut lod) => {
            if let Some(skipped) = lod.get("skipped") {
                return serde_json::json!({ "skipped": skipped });
            }
            let mut flat = lod
                .get_mut("lods")
                .map(serde_json::Value::take)
                .unwrap_or_else(|| serde_json::json!({}));
            for field in ["exitCode", "sceneId"] {
                if let Some(v) = lod.get(field) {
                    flat[field] = v.clone();
                }
            }
            flat
        }
    }
}

/// Key the scene's descriptor is published under.
fn descriptor_key(scene_id: &str) -> String {
    format!(
        "{}/{scene_id}{}",
        abgen::lods::MANIFEST_KEY_DIR,
        abgen::lodgen::placements::ISS_SUFFIX
    )
}

/// Publish a previous build's LOD bundles under this entity's names instead of building
/// them, when a build would produce the same bytes.
///
/// Two content-addressed lookups, cheapest first:
///
/// 1. By inputs. The runtime's output is a function of the code, the `main.crdt` snapshot
///    and the parcels; a record filed under this deployment's inputs digest means the same
///    placements without executing anything. What those placements resolve to is checked
///    against the record's dependency list, so a texture re-uploaded under the same name
///    still misses.
/// 2. By state. Execute the scene, derive the LOD state — placements, their content hashes
///    and the SDK primitives — and look that up. A code change that moved nothing lands
///    here and still reuses. This costs the scene execution and nothing downstream of it:
///    no asset download, no assemble, no atlas, no simplify, no bundling.
///
/// Returns `None` whenever anything is missing or unequal, which always means "build".
#[allow(clippy::too_many_arguments)]
fn try_reuse(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    client: &abgen::catalyst::CatalystClient,
    ent: &abgen::catalyst::Scene,
    platforms: &[String],
    params: &abgen::lodgen::GenerateParams,
    inputs: Option<&Inputs>,
) -> Result<Option<serde_json::Value>> {
    let started = std::time::Instant::now();
    if !proxy.space_configured() {
        return Ok(None);
    }
    let entity_id = ent.entity_id.as_str();
    let scene_id = ent.entity_id.to_lowercase();
    let content = ent.content_by_file();
    let levels = cfg.lod_levels.as_slice();

    if let Some(inputs) = inputs {
        let digest = reuse::inputs_digest(inputs);
        if let Some(record) = read_record(proxy, &reuse::inputs_index_key(&digest)) {
            match record.accepts_inputs(inputs, &content, levels, platforms) {
                // Same inputs, so the same descriptor apart from the entity it names: rename
                // the previous one rather than executing the scene to derive it again.
                Ok(()) => match renamed_descriptor(proxy, &record.built_by, &scene_id) {
                    Some(doc) => {
                        return republish(
                            cfg,
                            proxy,
                            ent,
                            platforms,
                            record,
                            doc,
                            Some(inputs),
                            "inputs",
                            started,
                        )
                    }
                    None => eprintln!(
                        "lods: {entity_id}: {} published no descriptor to rename; deriving one",
                        record.built_by
                    ),
                },
                Err(why) => {
                    eprintln!("lods: {entity_id}: inputs record {digest} does not apply: {why}")
                }
            }
        }
    }

    let (doc, primitives) = abgen::lodgen::descriptor_for(client, ent, &params.iss)?;
    let state = abgen::lodgen::lod_state(&doc, &primitives);
    let digest = abgen::lodgen::state_digest(&state);
    let Some(record) = read_record(proxy, &reuse::state_index_key(&digest)) else {
        return Ok(None);
    };
    match record.accepts_state(&state, &content, levels, platforms) {
        Ok(()) => republish(
            cfg, proxy, ent, platforms, record, doc, inputs, "state", started,
        ),
        Err(why) => {
            eprintln!("lods: {entity_id}: state record {digest} does not apply: {why}");
            Ok(None)
        }
    }
}

fn read_record(proxy: &Arc<Proxy>, key: &str) -> Option<ReuseRecord> {
    let bytes = proxy.space_get_key(key)?;
    match serde_json::from_slice(&bytes) {
        Ok(record) => Some(record),
        Err(e) => {
            eprintln!("lods: {key} does not parse ({e}); ignoring it");
            None
        }
    }
}

/// The descriptor `built_by` published, renamed to `scene_id`. Same placements, so the same
/// document apart from the entity it names.
fn renamed_descriptor(
    proxy: &Arc<Proxy>,
    built_by: &str,
    scene_id: &str,
) -> Option<serde_json::Value> {
    let bytes = proxy.space_get_key(&descriptor_key(&built_by.to_lowercase()))?;
    let mut doc: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    doc.as_object_mut()?
        .insert("sceneId".to_string(), serde_json::json!(scene_id));
    Some(doc)
}

/// One object to carry from a previous build's scene to this one.
#[derive(Debug, PartialEq)]
pub struct CopyItem {
    pub level: u32,
    pub from: String,
    pub to: String,
    /// Bundles must exist or the reuse is off; the published glb is carried when present.
    pub required: bool,
}

/// Every per-scene object a build publishes, as `(from, to)` keys between two scene ids.
pub fn copy_items(
    from_scene: &str,
    to_scene: &str,
    levels: &[u32],
    platforms: &[String],
) -> Vec<CopyItem> {
    let mut out = Vec::new();
    for &level in levels {
        for platform in platforms {
            out.push(CopyItem {
                level,
                from: format!(
                    "LOD/{level}/{}",
                    abgen::lods::lod_bundle_name(from_scene, level, platform)
                ),
                to: format!(
                    "LOD/{level}/{}",
                    abgen::lods::lod_bundle_name(to_scene, level, platform)
                ),
                required: true,
            });
        }
        out.push(CopyItem {
            level,
            from: format!(
                "{}/{}",
                abgen::lods::GLB_KEY_DIR,
                abgen::lods::published_glb_name(from_scene, level)
            ),
            to: format!(
                "{}/{}",
                abgen::lods::GLB_KEY_DIR,
                abgen::lods::published_glb_name(to_scene, level)
            ),
            required: false,
        });
    }
    out
}

/// Copy `record`'s bundles under this entity's names, publish its descriptor and point both
/// indexes at it. `None` when a bundle the record promised is not there.
///
/// The copied bundle keeps the previous scene's prefab name in its own metadata, which is
/// what the explorer loads by: it reads the main asset's name out of the bundle, never off
/// the file name.
#[allow(clippy::too_many_arguments)]
fn republish(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    ent: &abgen::catalyst::Scene,
    platforms: &[String],
    mut record: ReuseRecord,
    doc: serde_json::Value,
    inputs: Option<&Inputs>,
    by: &str,
    started: std::time::Instant,
) -> Result<Option<serde_json::Value>> {
    let entity_id = ent.entity_id.as_str();
    let scene_id = ent.entity_id.to_lowercase();
    let from_scene = record.built_by.to_lowercase();
    let mut keys: Vec<String> = Vec::new();
    let mut levels: Vec<(u32, usize)> = cfg.lod_levels.iter().map(|&l| (l, 0)).collect();
    for item in copy_items(&from_scene, &scene_id, &cfg.lod_levels, platforms) {
        let Some(bytes) = proxy.space_get_key(&item.from) else {
            if item.required {
                eprintln!(
                    "lods: {entity_id}: {} is missing; building instead",
                    item.from
                );
                return Ok(None);
            }
            continue;
        };
        if item.required {
            if let Some(slot) = levels.iter_mut().find(|(l, _)| *l == item.level) {
                slot.1 += bytes.len();
            }
        }
        // A re-run of the same entity finds its own objects; verified, not rewritten.
        if item.from != item.to {
            proxy.space_put_key(&item.to, &bytes);
        }
        keys.push(item.to);
    }

    // The descriptor is the one object not copied: it names its own scene.
    let iss_key = descriptor_key(&scene_id);
    proxy.space_put_key(&iss_key, serde_json::to_string_pretty(&doc)?.as_bytes());
    keys.push(iss_key);

    // This deployment now holds the bytes too, and it is the newest to; point both indexes
    // at it so a cleanup of older deployments does not turn the next job into a build.
    record.built_by = scene_id.clone();
    record.keys = keys.clone();
    record.inputs = inputs.cloned();
    record.inputs_digest = inputs.map(reuse::inputs_digest);
    publish_record(proxy, &record);

    eprintln!(
        "reused: {entity_id} lods scene={scene_id} from={from_scene} by={by} levels={} \
         platforms={} bytes={} objects={} in {:.1}s",
        levels
            .iter()
            .map(|(l, _)| l.to_string())
            .collect::<Vec<_>>()
            .join(","),
        platforms.join(","),
        levels.iter().map(|(_, b)| b).sum::<usize>(),
        keys.len(),
        started.elapsed().as_secs_f64(),
    );

    let mut summary = success_summary(entity_id, &scene_id, platforms, &levels, keys.len(), true);
    summary["lods"]["reusedFrom"] = serde_json::json!(from_scene);
    summary["lods"]["reusedBy"] = serde_json::json!(by);
    summary["lods"]["keys"] = serde_json::json!(keys);
    Ok(Some(summary))
}

/// File `record` under both of its content addresses. A failure here only costs a later
/// deployment its reuse, never this job.
fn publish_record(proxy: &Arc<Proxy>, record: &ReuseRecord) {
    let text = match serde_json::to_string_pretty(record) {
        Ok(text) => text,
        Err(e) => {
            eprintln!(
                "lods: {}: could not serialize the reuse record ({e})",
                record.built_by
            );
            return;
        }
    };
    proxy.space_put_key(
        &reuse::state_index_key(&record.state_digest),
        text.as_bytes(),
    );
    if let Some(digest) = &record.inputs_digest {
        proxy.space_put_key(&reuse::inputs_index_key(digest), text.as_bytes());
    }
}

/// The success summary a converted LOD job returns. Carries a top-level
/// `exitCode: 0` so `job_outcome` classifies it as `converted` — a summary
/// without one counts as `failed` in the job metrics.
fn success_summary(
    entity_id: &str,
    scene_id: &str,
    platforms: &[String],
    levels: &[(u32, usize)],
    objects: usize,
    uploaded: bool,
) -> serde_json::Value {
    serde_json::json!({
        "entityId": entity_id,
        "sceneId": scene_id,
        "exitCode": 0,
        "lods": {
            "platforms": platforms,
            "levels": levels.iter().map(|&(level, bundle_bytes)| serde_json::json!({
                "level": level,
                "bundleBytes": bundle_bytes,
            })).collect::<Vec<_>>(),
            "objects": objects,
            "uploaded": uploaded,
        },
    })
}

pub fn supported_platforms(configured: &[String]) -> (Vec<String>, Vec<String>) {
    let mut supported: Vec<String> = Vec::new();
    let mut rejected: Vec<String> = Vec::new();
    for p in configured {
        if abgen::lods::validate_lod_platform(p).is_ok() {
            if !supported.contains(p) {
                supported.push(p.clone());
            }
        } else if !rejected.contains(p) {
            rejected.push(p.clone());
        }
    }
    (supported, rejected)
}

pub fn generate_params(
    cfg: &Config,
    entity_id: &str,
    content_server: &str,
    platforms: &[String],
    staging: &Path,
) -> abgen::lodgen::GenerateParams {
    abgen::lodgen::GenerateParams {
        scene: entity_id.to_string(),
        out_dir: staging.to_string_lossy().into_owned(),
        platforms: platforms.to_vec(),
        levels: cfg.lod_levels.clone(),
        catalyst: content_server.to_string(),
        workdir: Some(staging.join("work")),
        cache: Some(PathBuf::from(&cfg.cache_dir).join("lod-content")),
        iss: "auto".to_string(),
        ..Default::default()
    }
}

pub struct Published {
    pub uploaded: bool,
    pub keys: Vec<String>,
}

fn publish(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    objects: &[abgen::lods::PublishedObject],
) -> Result<Published> {
    let keys: Vec<String> = objects.iter().map(|o| o.key.clone()).collect();
    if !proxy.space_configured() {
        eprintln!(
            "output: no space configured (set ABGEN_S3_ENDPOINT/ABGEN_S3_BUCKET) — \
             {} LOD object(s) left under {}",
            objects.len(),
            cfg.out_root.display(),
        );
        return Ok(Published {
            uploaded: false,
            keys,
        });
    }
    for (obj, key) in objects.iter().zip(keys.iter()) {
        let bytes =
            std::fs::read(&obj.path).with_context(|| format!("read {}", obj.path.display()))?;
        // Content-Type and Cache-Control are derived from the key.
        proxy.space_put_key(key, &bytes);
    }
    Ok(Published {
        uploaded: true,
        keys,
    })
}

// Staged LOD trees are large; drop them on every exit path (including the error
// ones) so a warm container does not accumulate them.
struct StagingGuard {
    path: PathBuf,
    keep: bool,
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod reuse_tests {
    use super::*;

    /// Every key the lambda publishes lives under the one `LOD/` root, and the two
    /// independent code paths that name the same object agree on its key.
    ///
    /// This is the property the offline generator depends on. `abgen-lod qualify-corpus`
    /// builds its publish tree from `published_objects` alone, then a run's output is synced
    /// to the same bucket a lambda writes to; if the lambda named any object differently, a
    /// scene built offline and a scene built by the lambda would land in different places and
    /// neither reuse nor the client would find both.
    ///
    /// The cross-check that matters is `descriptor_key` against `published_objects`: a fresh
    /// build publishes the ISS descriptor by walking the scene directory, while the reuse
    /// path publishes it by formatting a key from the scene id. Those are separate spellings
    /// of one layout and nothing but this test holds them together.
    #[test]
    fn every_published_key_lives_under_one_lod_root() {
        let sid = "bafkscene";
        let levels = [1u32];
        let plats = vec!["windows".to_string(), "mac".to_string()];

        // A finished scene directory, in the shape a build leaves behind.
        let base = std::env::temp_dir().join(format!("abgen-layout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scene = base.join(sid);
        std::fs::create_dir_all(scene.join("LOD/1")).unwrap();
        std::fs::create_dir_all(scene.join(abgen::lods::PUBLISHED_GLB_DIR)).unwrap();
        std::fs::write(scene.join("LOD/1/bafkscene_1_windows"), b"w").unwrap();
        std::fs::write(scene.join("LOD/1/bafkscene_1_mac"), b"m").unwrap();
        std::fs::write(scene.join("bafkscene_InitialSceneState.json"), b"{}").unwrap();
        std::fs::write(
            scene
                .join(abgen::lods::PUBLISHED_GLB_DIR)
                .join("bafkscene_1.glb"),
            b"g",
        )
        .unwrap();

        let objects = abgen::lods::published_objects(&scene, &levels);
        let keys: Vec<&str> = objects.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "LOD/1/bafkscene_1_mac",
                "LOD/1/bafkscene_1_windows",
                "LOD/lods-unity/manifests/bafkscene_InitialSceneState.json",
                "LOD/lods-unity/lods/bafkscene_1.glb",
            ],
            "the fresh-build key set is the published layout"
        );

        // The reuse path names the descriptor itself rather than walking the directory.
        // It must agree with the key the fresh path produced for the same file.
        let from_walk = keys
            .iter()
            .find(|k| k.contains("/manifests/"))
            .expect("a descriptor key");
        assert_eq!(
            &descriptor_key(sid),
            from_walk,
            "descriptor_key and published_objects disagree on the ISS key"
        );

        // Same for the reuse path's copy targets.
        let items = copy_items("bafkprev", sid, &levels, &plats);
        for item in &items {
            assert!(
                keys.contains(&item.to.as_str()),
                "copy target {} is not a key a fresh build would publish",
                item.to
            );
            assert!(
                item.from.starts_with("LOD/") && item.to.starts_with("LOD/"),
                "copy item escapes the LOD root: {} -> {}",
                item.from,
                item.to
            );
        }

        // The reuse records themselves.
        let index_keys = [
            reuse::inputs_index_key("abcd"),
            reuse::state_index_key("abcd"),
        ];
        assert_eq!(
            index_keys,
            [
                "LOD/lod-reuse/by-inputs/abcd.json".to_string(),
                "LOD/lod-reuse/by-state/abcd.json".to_string(),
            ]
        );

        // Nothing the lambda writes for a LOD build sits outside the single root, and the
        // metadata still resolves per family now that the prefixes are nested.
        for key in keys.iter().map(|k| k.to_string()).chain(index_keys) {
            assert!(key.starts_with("LOD/"), "key outside the LOD root: {key}");
            let h = abgen::space::object_headers(&key);
            let want = if key.starts_with("LOD/lod-reuse/") {
                ("application/json", "private, max-age=0, no-cache")
            } else if key.ends_with(".glb") {
                ("model/gltf-binary", "public, max-age=31536000")
            } else if key.ends_with(".json") {
                ("application/json", "public, max-age=31536000")
            } else {
                ("application/wasm", "public,max-age=31536000,immutable")
            };
            assert_eq!((h.content_type, h.cache_control), want, "headers for {key}");
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_items_name_every_per_scene_object_between_two_scenes() {
        let plats = vec!["windows".to_string(), "mac".to_string()];
        let items = copy_items("bafkPrev", "bafkNext", &[1], &plats);
        assert_eq!(
            items,
            vec![
                CopyItem {
                    level: 1,
                    from: "LOD/1/bafkprev_1_windows".into(),
                    to: "LOD/1/bafknext_1_windows".into(),
                    required: true,
                },
                CopyItem {
                    level: 1,
                    from: "LOD/1/bafkprev_1_mac".into(),
                    to: "LOD/1/bafknext_1_mac".into(),
                    required: true,
                },
                CopyItem {
                    level: 1,
                    from: "LOD/lods-unity/lods/bafkprev_1.glb".into(),
                    to: "LOD/lods-unity/lods/bafknext_1.glb".into(),
                    required: false,
                },
            ]
        );
        // Two levels, one platform: bundles are required, glbs are carried when present.
        let both = copy_items("a", "b", &[0, 1], &plats[..1]);
        assert_eq!(both.iter().filter(|i| i.required).count(), 2);
        assert_eq!(both.iter().filter(|i| !i.required).count(), 2);
        assert!(both
            .iter()
            .all(|i| i.from.contains("/a") && i.to.contains("/b")));
    }

    #[test]
    fn a_build_files_itself_under_both_digests() {
        let state =
            serde_json::json!({"generation": "1", "descriptor": {"assets": []}, "primitives": []});
        let outcome = abgen::lodgen::GenerateOutcome {
            entity_id: "bafkScene".into(),
            scene_id: "bafkscene".into(),
            source_tris: 0,
            placement_stats: Default::default(),
            unresolved_srcs: vec!["models/gone.glb".into()],
            levels: Vec::new(),
            gate: Vec::new(),
            log: Vec::new(),
            lod_state: state.clone(),
            dependencies: [("models/tree.glb", "bafktree")]
                .into_iter()
                .map(|(f, h)| (f.to_string(), h.to_string()))
                .collect(),
        };
        let keys = vec!["LOD/1/bafkscene_1_windows".to_string()];
        let inputs = Inputs {
            generation: abgen::lodgen::LOD_GENERATION.to_string(),
            runtime_version: "7".into(),
            base: "0,0".into(),
            parcels: vec!["0,0".into()],
            main_crdt: None,
            code: [("bin/index.js".to_string(), "bafkcode".to_string())]
                .into_iter()
                .collect(),
        };
        let record = record_for_build(
            &outcome,
            &keys,
            &[1],
            &["windows".to_string()],
            Some(inputs.clone()),
        );
        assert_eq!(record.built_by, "bafkscene");
        assert_eq!(record.keys, keys);
        assert_eq!(record.state, state);
        assert_eq!(record.state_digest, abgen::lodgen::state_digest(&state));
        assert_eq!(
            record.inputs_digest.as_deref(),
            Some(reuse::inputs_digest(&inputs).as_str())
        );
        assert_eq!(record.unresolved, vec!["models/gone.glb".to_string()]);
        assert_eq!(record.dependencies["models/tree.glb"], "bafktree");
        // The state key is what a later job derives; the inputs key what it hashes for free.
        assert_eq!(
            reuse::state_index_key(&record.state_digest),
            format!("LOD/lod-reuse/by-state/{}.json", record.state_digest)
        );

        // An SDK6 build has no inputs and is filed by state only.
        let by_state_only = record_for_build(&outcome, &keys, &[1], &["windows".to_string()], None);
        assert!(by_state_only.inputs.is_none());
        assert!(by_state_only.inputs_digest.is_none());
    }
}

#[cfg(test)]
mod follow_up_tests {
    use super::*;

    #[test]
    fn only_scenes_get_a_lod_follow_up() {
        assert!(is_scene(
            &serde_json::json!({"type": "scene", "id": "bafk"})
        ));
        assert!(!is_scene(&serde_json::json!({"type": "wearable"})));
        assert!(!is_scene(&serde_json::json!({"id": "bafk"})));
    }

    #[test]
    fn follow_up_is_off_without_enable_lods() {
        let cfg = Config {
            lods_enabled: false,
            ..tests::cfg()
        };
        let proxy = crate::convert::make_proxy(&cfg, "https://c/content");
        assert!(follow_up(&cfg, &proxy, "bafk", "https://c/content").is_none());
    }

    #[test]
    fn follow_up_summary_flattens_the_lod_block_and_keeps_skips_and_errors_small() {
        let converted = success_summary(
            "bafkE",
            "bafke",
            &["windows".to_string()],
            &[(1, 9)],
            4,
            true,
        );
        let flat = follow_up_summary(Ok(converted));
        assert_eq!(flat["exitCode"], 0);
        assert_eq!(flat["sceneId"], "bafke");
        assert!(flat.get("notified").is_none(), "LODs notify nobody: {flat}");
        assert_eq!(flat["objects"], 4);
        assert_eq!(flat["levels"][0]["bundleBytes"], 9);
        assert!(flat.get("lods").is_none(), "no double nesting: {flat}");

        let mut reused =
            success_summary("bafkE", "bafke", &["mac".to_string()], &[(1, 9)], 4, true);
        reused["lods"]["reusedBy"] = serde_json::json!("inputs");
        assert_eq!(follow_up_summary(Ok(reused))["reusedBy"], "inputs");

        let skipped =
            serde_json::json!({"entityId": "bafkE", "skipped": "lods-no-supported-platform"});
        assert_eq!(
            follow_up_summary(Ok(skipped)),
            serde_json::json!({"skipped": "lods-no-supported-platform"})
        );
        let err = follow_up_summary(Err(anyhow::anyhow!("gate failed")));
        assert_eq!(err["error"], "gate failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn cfg() -> Config {
        Config {
            platforms: vec!["windows".to_string(), "mac".to_string()],
            version: "v49".to_string(),
            wearable_version: "v49".to_string(),
            cache_dir: "/tmp/cache".to_string(),
            default_content_server: "https://peer.decentraland.org/content".to_string(),
            out_root: PathBuf::from("/tmp/out"),
            keep_output: false,
            allowed_content_server_hosts: None,
            http_secret: None,
            lods_enabled: true,
            max_receive_count: 3,
            lod_levels: crate::config::default_levels(),
        }
    }

    #[test]
    fn keeps_only_platforms_with_a_lod_lane() {
        let (ok, rejected) = supported_platforms(&[
            "windows".to_string(),
            "webgl".to_string(),
            "mac".to_string(),
            "windows".to_string(),
        ]);
        assert_eq!(ok, vec!["windows".to_string(), "mac".to_string()]);
        assert_eq!(rejected, vec!["webgl".to_string()]);

        let (ok, rejected) = supported_platforms(&["webgl".to_string()]);
        assert!(ok.is_empty());
        assert_eq!(rejected.len(), 1);
    }

    #[test]
    fn generate_params_target_the_production_lod_lane() {
        let staging = PathBuf::from("/tmp/out/lod/bafkscene");
        let p = generate_params(
            &cfg(),
            "bafkscene",
            "https://peer.decentraland.org/content",
            &["windows".to_string()],
            &staging,
        );
        assert_eq!(p.scene, "bafkscene");
        assert_eq!(p.out_dir, "/tmp/out/lod/bafkscene");
        assert_eq!(p.platforms, vec!["windows".to_string()]);
        assert_eq!(p.levels, vec![1]);
        assert_eq!(p.catalyst, "https://peer.decentraland.org/content");
        assert_eq!(p.workdir.as_deref(), Some(staging.join("work").as_path()));
        assert_eq!(
            p.cache.as_deref(),
            Some(Path::new("/tmp/cache").join("lod-content").as_path())
        );
        assert!(p.tri_cap_auto);
        assert!(p.crop);
        assert_eq!(p.iss, "auto");
    }

    #[test]
    fn levels_follow_the_config() {
        let staging = PathBuf::from("/tmp/out/lod/bafkscene");
        let both = Config {
            lod_levels: vec![0, 1],
            ..cfg()
        };
        let p = generate_params(&both, "bafkscene", "https://c/content", &[], &staging);
        assert_eq!(p.levels, vec![0, 1]);
    }

    #[test]
    fn success_summary_counts_as_converted_in_job_metrics() {
        let s = success_summary(
            "bafkscene",
            "sid",
            &["windows".to_string(), "mac".to_string()],
            &[(0, 1024), (1, 512)],
            7,
            true,
        );
        assert_eq!(s["exitCode"], 0);
        assert_eq!(s["lods"]["levels"][1]["bundleBytes"], 512);
        // The regression this guards: a success summary without a top-level
        // exitCode is classified "failed" by the job metrics.
        assert_eq!(crate::job_outcome(&Ok(s)), "converted");
    }

    #[test]
    fn staging_guard_removes_the_tree_unless_output_is_kept() {
        let base = std::env::temp_dir().join(format!("lambda-lod-guard-{}", std::process::id()));
        for keep in [false, true] {
            let path = base.join(if keep { "keep" } else { "drop" });
            std::fs::create_dir_all(path.join("LOD/1")).unwrap();
            drop(StagingGuard {
                path: path.clone(),
                keep,
            });
            assert_eq!(path.exists(), keep);
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
