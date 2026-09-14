use crate::config::Config;
use abgen::live::Proxy;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// LOD jobs carry the legacy Unity generator's FBX source URLs; abgen has no FBX
/// importer and regenerates the LOD geometry from the scene entity instead
/// (the same lodgen chain the abcdn serves JIT), so those URLs are unused.
///
pub fn convert(
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

    match try_reuse(cfg, proxy, entity_id, content_server, &platforms, &params) {
        Ok(Some(summary)) => {
            drop(guard);
            return Ok(summary);
        }
        Ok(None) => {}
        // Reuse is an optimization: a registry that is down, a descriptor that will not
        // parse or an object that has gone missing must cost a rebuild, never a job.
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
        publish_state(proxy, &outcome.scene_id, &outcome.lod_state);
    }

    // Notify only after every generated object has been published.
    let finished: Vec<crate::notify::Finished> = platforms
        .iter()
        .map(|p| crate::notify::Finished {
            platform: p,
            status_code: 0,
        })
        .collect();
    let notified = crate::notify::send_finished(cfg, entity_id, content_server, true, &finished)?;
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
        notified,
    );
    summary["lods"]["keys"] = serde_json::json!(published.keys);
    Ok(summary)
}

/// Key the LOD state document is published under, beside the scene's descriptor.
///
/// This is the record a later deployment compares itself against, and the one to read when
/// two deployments disagree about whether they would build the same thing.
fn state_key(scene_id: &str) -> String {
    format!("lods-unity/manifests/{scene_id}_LODState.json")
}

/// Key the scene's descriptor is published under.
fn descriptor_key(scene_id: &str) -> String {
    format!(
        "lods-unity/manifests/{scene_id}{}",
        abgen::lodgen::placements::ISS_SUFFIX
    )
}

fn publish_state(proxy: &Arc<Proxy>, scene_id: &str, state: &serde_json::Value) {
    match serde_json::to_string_pretty(state) {
        Ok(text) => proxy.space_put_key(&state_key(scene_id), text.as_bytes()),
        // Only costs the next deployment its reuse; never the job.
        Err(e) => eprintln!("lods: {scene_id}: could not serialize the LOD state ({e})"),
    }
}

/// Publish the previous deployment's LOD bundles for this entity instead of building them,
/// when the two deployments would produce the same geometry.
///
/// The expensive half of a LOD build is everything after placements: downloading every
/// asset, assembling, atlasing, simplifying and bundling per platform. Deriving the
/// descriptor stops before all of it, so this check costs one scene execution and a few
/// small reads, against a full build that costs minutes.
///
/// Equality is decided by the LOD state document, not by the descriptor alone: the
/// descriptor lists glTF placements but not the scene's SDK primitives, and a scene whose
/// only change is a primitive would otherwise reuse bundles that no longer match it.
///
/// Returns `None` whenever anything is missing or unequal, which always means "build".
fn try_reuse(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    entity_id: &str,
    content_server: &str,
    platforms: &[String],
    params: &abgen::lodgen::GenerateParams,
) -> Result<Option<serde_json::Value>> {
    let started = std::time::Instant::now();
    let Some(registry) = cfg.ab_registry_url.as_deref() else {
        return Ok(None);
    };
    if !proxy.space_configured() {
        return Ok(None);
    }
    let agent = crate::catalyst::agent();
    let entity = crate::catalyst::fetch_entity(&agent, content_server, entity_id)?;
    let pointers: Vec<String> = entity
        .get("pointers")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let world_name = entity
        .pointer("/metadata/worldConfiguration/name")
        .and_then(serde_json::Value::as_str);

    let Some(active) =
        crate::bundle_registry::active_entity(&agent, registry, &pointers, world_name)?
    else {
        return Ok(None);
    };
    // The registry still answers with the deployment this job supersedes. If it has already
    // moved on to this entity there is no earlier build to copy from.
    if active.entity_id.eq_ignore_ascii_case(entity_id) {
        return Ok(None);
    }
    if !active.lods_complete_for(platforms) {
        return Ok(None);
    }

    let previous = active.entity_id.to_lowercase();
    let Some(bytes) = proxy.space_get_key(&state_key(&previous)) else {
        // Built before the state document was published.
        return Ok(None);
    };
    let published: serde_json::Value = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", state_key(&previous)))?;
    let scene_id = entity_id.to_lowercase();

    // Identical content listings mean identical files, including the scene's code and the
    // `main.crdt` its runtime starts from, so there is nothing left for a build to do
    // differently. Both listings are already in hand — the registry returned the previous
    // one alongside the entity — which makes this the one check that costs nothing.
    let content_digest = crate::bundle_registry::content_digest(entity.get("content"));
    let same_content = !content_digest.is_empty() && content_digest == active.content_digest;

    let (doc, state) = if same_content {
        // Same files, so the same descriptor apart from the entity it names. Rename the
        // previous one rather than executing the scene to derive a document already known.
        let Some(bytes) = proxy.space_get_key(&descriptor_key(&previous)) else {
            return Ok(None);
        };
        let mut doc: serde_json::Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", descriptor_key(&previous)))?;
        let Some(obj) = doc.as_object_mut() else {
            return Ok(None);
        };
        obj.insert("sceneId".to_string(), serde_json::json!(scene_id));
        (doc, published)
    } else {
        // Files differ, so derive this deployment's state and compare it with what the
        // previous one recorded. Still far short of a build: placements only, with nothing
        // downstream of them. A code change that leaves the geometry alone lands here and
        // still reuses; only a real difference in the state falls through to a build.
        let (_, doc, primitives) = abgen::lodgen::descriptor_only(params)?;
        let state = abgen::lodgen::lod_state(&doc, &primitives);
        if state != published {
            return Ok(None);
        }
        (doc, state)
    };

    // Same geometry: copy the bundles across under this entity's names. The bundle's own
    // metadata still names the previous scene's prefab as its main asset, which is what the
    // explorer loads by — it reads the name out of the bundle, never off the file name.
    let mut keys: Vec<String> = Vec::new();
    let mut levels: Vec<(u32, usize)> = Vec::new();
    for &level in &cfg.lod_levels {
        let mut level_bytes = 0usize;
        for platform in platforms {
            let from = format!(
                "LOD/{level}/{}",
                abgen::lods::lod_bundle_name(&previous, level, platform)
            );
            let to = format!(
                "LOD/{level}/{}",
                abgen::lods::lod_bundle_name(&scene_id, level, platform)
            );
            let Some(bytes) = proxy.space_get_key(&from) else {
                eprintln!("lods: {entity_id}: {from} is missing; building instead");
                return Ok(None);
            };
            level_bytes += bytes.len();
            proxy.space_put_key(&to, &bytes);
            keys.push(to);
        }
        levels.push((level, level_bytes));
        let glb_from = format!(
            "{}/{}",
            abgen::lods::PUBLISHED_GLB_DIR,
            abgen::lods::published_glb_name(&previous, level)
        );
        if let Some(bytes) = proxy.space_get_key(&glb_from) {
            let glb_to = format!(
                "{}/{}",
                abgen::lods::PUBLISHED_GLB_DIR,
                abgen::lods::published_glb_name(&scene_id, level)
            );
            proxy.space_put_key(&glb_to, &bytes);
            keys.push(glb_to);
        }
    }

    // The descriptor is the one thing that is not copied: it names its own scene, so the
    // freshly derived document is published rather than the previous entity's.
    let iss_key = descriptor_key(&scene_id);
    proxy.space_put_key(&iss_key, serde_json::to_string_pretty(&doc)?.as_bytes());
    keys.push(iss_key);
    publish_state(proxy, &scene_id, &state);

    let notified = crate::notify::send_finished(
        cfg,
        entity_id,
        content_server,
        true,
        &platforms
            .iter()
            .map(|p| crate::notify::Finished {
                platform: p,
                status_code: 0,
            })
            .collect::<Vec<_>>(),
    )?;
    eprintln!(
        "reused: {entity_id} lods scene={scene_id} from={previous} by={} levels={} platforms={} \
         bytes={} objects={} in {:.1}s",
        if same_content { "content" } else { "geometry" },
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

    let mut summary = success_summary(
        entity_id,
        &scene_id,
        platforms,
        &levels,
        keys.len(),
        true,
        notified,
    );
    summary["lods"]["reusedFrom"] = serde_json::json!(previous);
    summary["lods"]["reusedBy"] = serde_json::json!(if same_content {
        "content"
    } else {
        "geometry"
    });
    summary["lods"]["keys"] = serde_json::json!(keys);
    Ok(Some(summary))
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
    notified: bool,
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
        "notified": notified,
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
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config {
            platforms: vec!["windows".to_string(), "mac".to_string()],
            version: "v49".to_string(),
            cache_dir: "/tmp/cache".to_string(),
            default_content_server: "https://peer.decentraland.org/content".to_string(),
            out_root: PathBuf::from("/tmp/out"),
            keep_output: false,
            allowed_content_server_hosts: None,
            http_secret: None,
            lods_enabled: true,
            max_receive_count: 3,
            lod_levels: crate::config::default_levels(),
            ab_registry_url: None,
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
            ab_registry_url: None,
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
            true,
        );
        assert_eq!(s["exitCode"], 0);
        assert_eq!(s["notified"], true);
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
