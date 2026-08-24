use crate::catalyst;
use crate::config::Config;
use crate::convert::EntityOutcome;
use abgen::live::Proxy;
use anyhow::{Context, Result};
use std::sync::Arc;

/// Bucket+version-scoped so an `AB_VERSION` bump can't suppress reconversion;
/// only verdicts read back from S3 are cached — never our own fail-soft uploads.
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

/// Prod's manifest shape with no files and UNEXPECTED_ERROR; never mistaken
/// for a conversion — `platform_converted` requires `exitCode == 0`.
fn failure_manifest(cfg: &Config, content_server: &str, date: &str) -> Vec<u8> {
    serde_json::json!({
        "version": cfg.version,
        "files": [],
        "exitCode": crate::notify::STATUS_UNEXPECTED_ERROR,
        "contentServerUrl": content_server,
        "date": date,
    })
    .to_string()
    .into_bytes()
}

/// One tombstone per platform without a good manifest; errors propagate —
/// a tombstone we cannot land must still reach the DLQ.
pub fn publish_failure_tombstones(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    entity_id: &str,
    content_server: &str,
) -> Result<Vec<String>> {
    let mut tombstoned = Vec::new();
    for platform in &cfg.platforms {
        if platform_converted(proxy, cfg, entity_id, platform) {
            continue;
        }
        let bytes = failure_manifest(cfg, content_server, proxy.date());
        proxy
            .space_put_manifest_strict(&format!("{entity_id}_{platform}"), &bytes)
            .with_context(|| format!("tombstone manifest for {entity_id} {platform}"))?;
        tombstoned.push(platform.clone());
    }
    metrics::counter!("abgen_lambda_tombstones_total").increment(tombstoned.len() as u64);
    Ok(tombstoned)
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
        let key = format!("{}/{}/{file}", cfg.version, outcome.entity_id);
        proxy.space_put_key(&key, &bytes);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::valid_key_component;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn cfg(tag: &str) -> crate::config::Config {
        crate::config::Config {
            platforms: vec!["windows".to_string(), "mac".to_string()],
            version: "v49".to_string(),
            cache_dir: std::env::temp_dir()
                .join(format!("abgen-output-test-{tag}-{}", std::process::id()))
                .to_string_lossy()
                .into_owned(),
            default_content_server: String::new(),
            out_root: std::path::PathBuf::new(),
            keep_output: false,
            allowed_content_server_hosts: None,
            http_secret: None,
            lods_enabled: false,
            max_receive_count: 3,
        }
    }

    /// Trimmed copy of `abgen::live::stub::serve` (that one is `cfg(test)`
    /// and invisible to this crate).
    fn serve(routes: Vec<(String, u16, Vec<u8>)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen2 = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut stream) = conn else { break };
                let Ok(clone) = stream.try_clone() else {
                    continue;
                };
                let mut reader = BufReader::new(clone);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                let mut content_len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() {
                        break;
                    }
                    let ht = h.trim();
                    if ht.is_empty() {
                        break;
                    }
                    if let Some(v) = ht.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                }
                if content_len > 0 {
                    let mut body = vec![0u8; content_len];
                    let _ = reader.read_exact(&mut body);
                }
                seen2.lock().unwrap().push(format!("{method} {path}"));
                let (code, body) = routes
                    .iter()
                    .find(|(p, _, _)| path == *p)
                    .map(|(_, c, b)| (*c, b.clone()))
                    .unwrap_or((404, Vec::new()));
                let _ = write!(
                    stream,
                    "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (format!("127.0.0.1:{}", addr.port()), seen)
    }

    #[test]
    fn tombstones_only_unconverted_platforms() {
        let good = serde_json::json!({
            "version": "v49", "files": ["x", "dcl"], "exitCode": 0,
            "contentServerUrl": "cs", "date": "d"
        })
        .to_string()
        .into_bytes();
        // The mac route serves an unparseable (empty) manifest on GET, so the
        // platform reads as unconverted; the same route accepts the PUT.
        let (host, seen) = serve(vec![
            ("/manifest/bafkpart_windows.json".to_string(), 200, good),
            ("/manifest/bafkpart_mac.json".to_string(), 200, Vec::new()),
        ]);
        let _env = crate::convert::TEST_SPACE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        struct EnvGuard;
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for k in [
                    "ABGEN_S3_ENDPOINT",
                    "AWS_ACCESS_KEY_ID",
                    "AWS_SECRET_ACCESS_KEY",
                ] {
                    std::env::remove_var(k);
                }
            }
        }
        let _guard = EnvGuard;
        std::env::set_var("ABGEN_S3_ENDPOINT", format!("http://{host}"));
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIATEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test-secret");

        let cfg = cfg("tombstone-partial");
        let proxy = crate::convert::make_proxy(&cfg, "http://127.0.0.1:9");
        let tombstoned = super::publish_failure_tombstones(
            &cfg,
            &proxy,
            "bafkpart",
            "https://peer.decentraland.org/content",
        )
        .unwrap();
        assert_eq!(tombstoned, vec!["mac".to_string()]);

        let log = seen.lock().unwrap().clone();
        assert!(
            log.contains(&"PUT /manifest/bafkpart_mac.json".to_string()),
            "{log:?}"
        );
        assert!(
            !log.iter()
                .any(|l| l == "PUT /manifest/bafkpart_windows.json"),
            "{log:?}"
        );
    }

    #[test]
    fn failure_manifest_json_is_pinned() {
        let cfg = cfg("pinned");
        let bytes =
            super::failure_manifest(&cfg, "https://peer.decentraland.org/content", "2026-08-24");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "{\"version\":\"v49\",\"files\":[],\"exitCode\":5,\
             \"contentServerUrl\":\"https://peer.decentraland.org/content\",\
             \"date\":\"2026-08-24\"}"
        );
    }

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
