//! Publishing — almost entirely delegated to abgen's native "space" (its
//! built-in SigV4 S3 integration, battle-tested by the JIT server).
//!
//! With `use_space` on, `build_entity_into_corpus` itself:
//!   * probes `{version}/assets/{bundleName}` per digest-named scene glb and
//!     skips rebuilding files that already exist (per-file asset reuse — the
//!     consumer-server's `cachedHashes` equivalent; reused names still appear
//!     in the manifest),
//!   * uploads each newly built bundle DURING the build (write-through) —
//!     canonical `{version}/assets/…` for digest-named files,
//!     entity-scoped `{version}/{entityId}/…` otherwise,
//!   * uploads the manifest to `manifest/{entityId}_{platform}.json`.
//!
//! That is exactly the prod key layout. What this module still owns:
//!   * the entity-level already-converted check (manifest GET + exitCode/
//!     version compare — the consumer's `shouldIgnoreConversion`),
//!   * scene sources (`main.crdt`, `scene.json`, `metadata.main`) to
//!     `{version}/{entityId}/…` for the desktop explorer (#7625).
//!
//! Caching/content-type headers: the space PUTs plain content types and no
//! Cache-Control. Cache policy lives at the CDN layer instead — the
//! CloudFront distribution must set long TTLs for `{version}/…` (names are
//! content-addressed/immutable) and TTL 0 for `manifest/…`. No `.br`
//! variants (no client of this pipeline fetches them).

use crate::catalyst;
use crate::config::Config;
use crate::convert::EntityOutcome;
use abgen::live::Proxy;
use anyhow::Result;
use std::sync::Arc;

/// Mirrors the consumer-server's `shouldIgnoreConversion`: a platform counts
/// as already converted only when its manifest exists, parses, has
/// `exitCode == 0` (a tolerated-failure 12 gets another chance) and carries
/// the current AB version. Any fetch/parse problem means "convert". With no
/// space configured this is always false.
pub fn platform_converted(
    proxy: &Arc<Proxy>,
    cfg: &Config,
    entity_id: &str,
    platform: &str,
) -> bool {
    let Some(bytes) = proxy.space_get_manifest(&format!("{entity_id}_{platform}")) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    json.get("exitCode").and_then(serde_json::Value::as_i64) == Some(0)
        && json.get("version").and_then(serde_json::Value::as_str) == Some(cfg.version.as_str())
}

/// Bundles and manifests were already written through by the build; this
/// publishes the remaining scene-source files and reports what happened.
pub fn publish(
    cfg: &Config,
    agent: &ureq::Agent,
    proxy: &Arc<Proxy>,
    entity_doc: &serde_json::Value,
    outcome: &EntityOutcome,
) -> Result<serde_json::Value> {
    let total_bundles: usize = outcome.platforms.iter().map(|p| p.built.len()).sum();
    if !proxy.space_configured() {
        eprintln!(
            "output: no space configured (set ABGEN_S3_ENDPOINT/ABGEN_S3_BUCKET) — \
             corpus left at {} ({total_bundles} bundle(s))",
            cfg.out_root.display(),
        );
        return Ok(serde_json::json!({
            "uploaded": false,
            "local": cfg.out_root.display().to_string(),
        }));
    }

    let entity_type = entity_doc
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("scene");
    let mut scene_sources = 0usize;
    if entity_type == "scene" && outcome.exit_code() == 0 {
        scene_sources = upload_scene_sources(cfg, agent, proxy, entity_doc, outcome);
    }

    eprintln!(
        "output: {} — bundles+manifests written through by the build \
         ({total_bundles} manifest entr{} across {} platform(s)), {scene_sources} scene source(s)",
        outcome.entity_id,
        if total_bundles == 1 { "y" } else { "ies" },
        outcome.platforms.len(),
    );
    Ok(serde_json::json!({
        "uploaded": true,
        "manifestEntries": total_bundles,
        // "Attempted" is honest: space_put_key is fire-and-forget (logs its
        // own failures), so a failed PUT still counts here — best-effort,
        // like prod's scene-source uploader.
        "sceneSourcesAttempted": scene_sources,
    }))
}

/// `main.crdt`, `scene.json` and the entity's declared main script, fetched
/// from the catalyst and re-published entity-scoped. Best-effort per file
/// (mirrors prod, which logs and continues): a missing source file must not
/// fail a finished conversion.
fn upload_scene_sources(
    cfg: &Config,
    agent: &ureq::Agent,
    proxy: &Arc<Proxy>,
    entity_doc: &serde_json::Value,
    outcome: &EntityOutcome,
) -> usize {
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
        // space_put_key logs failures itself (best-effort, like prod).
        proxy.space_put_key(&key, &bytes, content_type);
        count += 1;
    }
    count
}
