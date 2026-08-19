use crate::config::Config;
use abgen::live::Proxy;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const LEVELS: [u32; 2] = [0, 1];

/// LOD jobs carry the legacy Unity generator's FBX source URLs; abgen has no FBX
/// importer and regenerates the LOD geometry from the scene entity instead
/// (the same lodgen chain the abcdn serves JIT), so those URLs are unused.
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
    let objects = abgen::lods::published_objects(&scene_dir, &LEVELS);
    let uploaded = publish(cfg, proxy, &objects)?;

    let bundle_bytes: usize = outcome.levels.iter().map(|l| l.bundle_bytes).sum();
    eprintln!(
        "done: {entity_id} lods scene={} levels={} platforms={} bytes={bundle_bytes} \
         objects={} uploaded={uploaded} in {:.1}s",
        outcome.scene_id,
        outcome
            .levels
            .iter()
            .map(|l| l.level.to_string())
            .collect::<Vec<_>>()
            .join(","),
        platforms.join(","),
        objects.len(),
        started.elapsed().as_secs_f64(),
    );
    drop(guard);

    Ok(serde_json::json!({
        "entityId": entity_id,
        "sceneId": outcome.scene_id,
        "lods": {
            "platforms": platforms,
            "levels": outcome.levels.iter().map(|l| serde_json::json!({
                "level": l.level,
                "bundleBytes": l.bundle_bytes,
            })).collect::<Vec<_>>(),
            "objects": objects.len(),
            "uploaded": uploaded,
        },
    }))
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
        levels: LEVELS.to_vec(),
        catalyst: content_server.to_string(),
        workdir: Some(staging.join("work")),
        cache: Some(PathBuf::from(&cfg.cache_dir).join("lod-content")),
        ..Default::default()
    }
}

fn publish(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    objects: &[abgen::lods::PublishedObject],
) -> Result<bool> {
    if !proxy.space_configured() {
        eprintln!(
            "output: no space configured (set ABGEN_S3_ENDPOINT/ABGEN_S3_BUCKET) — \
             {} LOD object(s) left under {}",
            objects.len(),
            cfg.out_root.display(),
        );
        return Ok(false);
    }
    for obj in objects {
        let bytes =
            std::fs::read(&obj.path).with_context(|| format!("read {}", obj.path.display()))?;
        proxy.space_put_key(&obj.key, &bytes, obj.content_type);
    }
    Ok(true)
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
        assert_eq!(p.levels, vec![0, 1]);
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
