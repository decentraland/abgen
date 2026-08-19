use crate::catalyst;
use crate::config::Config;
use crate::convert::EntityOutcome;
use abgen::live::Proxy;
use anyhow::Result;
use std::sync::Arc;

/// Bucket+version-scoped so an `AB_VERSION` bump can't suppress reconversion;
/// only verdicts read back from S3 are cached — never our own fail-soft uploads.
/// Unlike content-addressed probe keys this caches a MUTABLE artifact: a
/// reconversion done outside this lambda leaves a stale marker for up to the
/// TTL. In-lambda force jobs clear markers before and after rebuilding, which
/// narrows but does not close that window.
pub fn converted_marker_key(
    proxy: &Arc<Proxy>,
    cfg: &Config,
    entity_id: &str,
    platform: &str,
) -> Option<String> {
    if !abgen::rediscache::enabled() {
        return None;
    }
    let bucket = proxy.space_bucket()?;
    Some(format!(
        "abgen:converted:{bucket}:{}:{entity_id}_{platform}",
        cfg.version
    ))
}

pub fn platform_converted(
    proxy: &Arc<Proxy>,
    cfg: &Config,
    entity_id: &str,
    platform: &str,
) -> bool {
    let marker = converted_marker_key(proxy, cfg, entity_id, platform);
    if let Some(key) = &marker {
        if abgen::rediscache::hit(key) {
            return true;
        }
    }
    let Some(bytes) = proxy.space_get_manifest(&format!("{entity_id}_{platform}")) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let converted = json.get("exitCode").and_then(serde_json::Value::as_i64) == Some(0)
        && json.get("version").and_then(serde_json::Value::as_str) == Some(cfg.version.as_str());
    if converted {
        if let Some(key) = &marker {
            abgen::rediscache::mark(key);
        }
    }
    converted
}

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
        "sceneSourcesAttempted": scene_sources,
    }))
}

/// Entity-supplied file names end up in S3 object keys, and `uri_encode_key`
/// preserves '/' and '.', so a hostile name could escape the
/// `{version}/{entityId}/` prefix.
fn valid_key_component(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.bytes().any(|b| b.is_ascii_control())
        && !name.split('/').any(|seg| seg == "..")
}

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

    if !valid_key_component(&outcome.entity_id) {
        eprintln!(
            "output: unsafe entity id {:?}, skipping scene-source upload",
            outcome.entity_id
        );
        return 0;
    }

    let mut count = 0usize;
    for file in &wanted {
        if !valid_key_component(file) {
            eprintln!("output: unsafe scene-source name {file:?}, skipping");
            continue;
        }
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
        proxy.space_put_key(&key, &bytes, content_type);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::valid_key_component;

    #[test]
    fn accepts_ordinary_names() {
        for name in [
            "main.crdt",
            "scene.json",
            "bin/game.js",
            "assets/models/tree.glb",
            "bafkreia1b2c3",
            "file with spaces.png",
            "trailing/",
            "a..b/c",
            "...three-dots",
        ] {
            assert!(valid_key_component(name), "should accept {name:?}");
        }
    }

    #[test]
    fn rejects_escaping_names() {
        for name in [
            "",
            "..",
            "../secret",
            "a/../../b",
            "bin/..",
            "/etc/passwd",
            "a\\b",
            "..\\up",
            "a\nb",
            "a\0b",
            "\x1b[2Jclear",
        ] {
            assert!(!valid_key_component(name), "should reject {name:?}");
        }
    }
}
