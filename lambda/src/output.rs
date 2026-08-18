//! Publishing conversion output to the CDN bucket, mirroring the Node
//! consumer-server's layout exactly:
//!
//!   * scene bundles (asset-reuse/canonical): `{version}/assets/{bundleName}`
//!   * wearable/emote bundles (entity-scoped): `{version}/{entityId}/{bundleName}`
//!   * manifests: `manifest/{entityId}_{platform}.json` — abgen's corpus
//!     manifest is shape-identical to prod's ({version, files, exitCode,
//!     contentServerUrl, date}), uploaded verbatim
//!   * scene sources (clean scene conversions only): `main.crdt`,
//!     `scene.json` and `entity.metadata.main` to `{version}/{entityId}/…` —
//!     the desktop explorer fetches these from the CDN (issue #7625)
//!
//! Headers mirror prod: bundles `application/wasm` + immutable caching (odd
//! but load-bearing — it is what makes the edge cache them), manifests
//! `application/json` + no-cache. Deliberately NOT uploaded: the `.br`
//! brotli siblings prod stores next to every file — no client of this
//! pipeline fetches them (verified 2026-08: unity-explorer and aang-renderer
//! have zero `.br` references); edge compression can cover a future web
//! consumer.

use crate::config::Config;
use crate::convert::EntityOutcome;
use crate::{catalyst, s3};
use anyhow::{Context, Result};

const BUNDLE_CONTENT_TYPE: &str = "application/wasm";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const MANIFEST_CONTENT_TYPE: &str = "application/json";
const MANIFEST_CACHE: &str = "private, max-age=0, no-cache";

/// The S3 client for the configured bucket, or `None` when publishing is
/// disabled (no `S3_BUCKET` — local-only runs).
pub fn client_from(cfg: &Config) -> Result<Option<s3::S3Client>> {
    match &cfg.s3_bucket {
        Some(bucket) => Ok(Some(s3::S3Client::new(
            bucket,
            &cfg.s3_region,
            cfg.s3_endpoint.as_deref(),
            cfg.s3_acl.as_deref(),
        )?)),
        None => Ok(None),
    }
}

/// Mirrors the consumer-server's `shouldIgnoreConversion`: a platform counts
/// as already converted only when its manifest exists, parses, has
/// `exitCode == 0` (a tolerated-failure 12 gets another chance) and carries
/// the current AB version. Any fetch/parse problem means "convert".
pub fn platform_converted(
    client: &s3::S3Client,
    cfg: &Config,
    entity_id: &str,
    platform: &str,
) -> bool {
    let key = format!("manifest/{entity_id}_{platform}.json");
    let bytes = match client.get_object(&key) {
        Ok(Some(b)) => b,
        Ok(None) => return false,
        Err(e) => {
            eprintln!("probe: {key}: {e:#} — converting to be safe");
            return false;
        }
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    json.get("exitCode").and_then(serde_json::Value::as_i64) == Some(0)
        && json.get("version").and_then(serde_json::Value::as_str) == Some(cfg.version.as_str())
}

pub fn publish(
    cfg: &Config,
    agent: &ureq::Agent,
    client: Option<&s3::S3Client>,
    entity_doc: &serde_json::Value,
    outcome: &EntityOutcome,
) -> Result<serde_json::Value> {
    let (Some(bucket), Some(client)) = (&cfg.s3_bucket, client) else {
        let total: usize = outcome.platforms.iter().map(|p| p.built.len()).sum();
        eprintln!(
            "output: no S3_BUCKET configured — corpus left at {} ({total} file(s))",
            cfg.out_root.display(),
        );
        return Ok(serde_json::json!({
            "uploaded": false,
            "local": cfg.out_root.display().to_string(),
        }));
    };

    let entity_type = entity_doc
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("scene");
    let is_scene = entity_type == "scene";
    // Scenes share canonical digest-named keys across entities; everything
    // else is entity-scoped. Matches the consumer's useAssetReuse gate, which
    // is scene-only, and abgen's own digest-naming gate.
    let bundle_prefix = if is_scene {
        format!("{}/assets", cfg.version)
    } else {
        format!("{}/{}", cfg.version, outcome.entity_id)
    };

    let mut uploaded = 0usize;
    for p in &outcome.platforms {
        for name in &p.built {
            let stored = p.dir.join(&*abgen::naming::fs_safe_component(name));
            let bytes = std::fs::read(&stored)
                .with_context(|| format!("read built bundle {}", stored.display()))?;
            client
                .put_object(
                    &format!("{bundle_prefix}/{name}"),
                    &bytes,
                    BUNDLE_CONTENT_TYPE,
                    IMMUTABLE,
                )
                .with_context(|| format!("upload bundle {name}"))?;
            uploaded += 1;
        }
        let manifest_bytes = std::fs::read(&p.manifest_path)
            .with_context(|| format!("read manifest {}", p.manifest_path.display()))?;
        client
            .put_object(
                &format!("manifest/{}_{}.json", outcome.entity_id, p.platform),
                &manifest_bytes,
                MANIFEST_CONTENT_TYPE,
                MANIFEST_CACHE,
            )
            .with_context(|| format!("upload manifest for {}", p.platform))?;
        uploaded += 1;
    }

    let mut scene_sources = 0usize;
    if is_scene && outcome.exit_code() == 0 {
        scene_sources = upload_scene_sources(cfg, agent, client, entity_doc, outcome)?;
    }

    eprintln!(
        "output: uploaded {uploaded} object(s) + {scene_sources} scene source(s) \
         for {} to s3://{bucket}/{bundle_prefix}/…",
        outcome.entity_id,
    );
    Ok(serde_json::json!({
        "uploaded": true,
        "bucket": bucket,
        "objects": uploaded,
        "sceneSources": scene_sources,
        "bundlePrefix": bundle_prefix,
    }))
}

/// `main.crdt`, `scene.json` and the entity's declared main script, fetched
/// from the catalyst and re-published entity-scoped. Best-effort per file
/// (mirrors prod, which logs and continues): a missing source file must not
/// fail a finished conversion.
fn upload_scene_sources(
    cfg: &Config,
    agent: &ureq::Agent,
    client: &s3::S3Client,
    entity_doc: &serde_json::Value,
    outcome: &EntityOutcome,
) -> Result<usize> {
    let mut wanted: Vec<String> = vec!["main.crdt".to_string(), "scene.json".to_string()];
    if let Some(main) = entity_doc
        .pointer("/metadata/main")
        .and_then(serde_json::Value::as_str)
    {
        wanted.push(main.to_string());
    }

    let empty = Vec::new();
    let content = entity_doc
        .get("content")
        .and_then(serde_json::Value::as_array)
        .unwrap_or(&empty);
    let hash_for = |file: &str| -> Option<&str> {
        content.iter().find_map(|c| {
            (c.get("file").and_then(serde_json::Value::as_str) == Some(file))
                .then(|| c.get("hash").and_then(serde_json::Value::as_str))
                .flatten()
        })
    };

    let mut count = 0usize;
    for file in &wanted {
        let Some(hash) = hash_for(file) else {
            eprintln!("output: {file} not in entity content, skipping scene-source upload");
            continue;
        };
        let url = format!(
            "{}/contents/{hash}",
            outcome.content_server.trim_end_matches('/')
        );
        let bytes = match catalyst::get_bytes(agent, &url) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("output: failed to fetch scene source {file}: {e:#}");
                continue;
            }
        };
        let content_type = if file.ends_with(".js") {
            "application/javascript"
        } else if file.ends_with(".json") {
            "application/json"
        } else {
            "application/octet-stream"
        };
        let key = format!("{}/{}/{file}", cfg.version, outcome.entity_id);
        match client.put_object(&key, &bytes, content_type, IMMUTABLE) {
            Ok(()) => count += 1,
            Err(e) => eprintln!("output: failed to upload scene source {file}: {e:#}"),
        }
    }
    Ok(count)
}
