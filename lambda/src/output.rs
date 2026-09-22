use crate::catalyst;
use crate::config::Config;
use crate::convert::EntityOutcome;
use abgen::live::Proxy;
use anyhow::{Context, Result};
use std::sync::Arc;

/// Scoped by bucket and by *both* version lanes, so a bump of either can't be suppressed by
/// a marker written before it; and recipe-scoped for the same reason, since a recipe bump
/// leaves both versions alone. The entity type is not known at this point, which is why both
/// lanes are in the key rather than the one that applies.
/// Only verdicts read back from S3 are cached — never our own fail-soft uploads.
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
        "abgen:converted:{bucket}:{}|{}{}:{entity_id}_{platform}",
        cfg.version,
        cfg.wearable_version,
        recipe_marker_scope()
    ))
}

/// Appended to the converted marker so a recipe bump cannot be answered out of a cache
/// filled before it. Empty while nothing is bumped, which keeps the keys already in Redis
/// exactly where they are.
fn recipe_marker_scope() -> String {
    use abgen::recipes::Recipe;
    if !abgen::recipes::any_bumped() {
        return String::new();
    }
    let table: Vec<String> = Recipe::ALL
        .iter()
        .filter(|r| r.generation() != abgen::recipes::BASELINE)
        .map(|r| format!("{}{}", r.name(), r.generation()))
        .collect();
    format!("+{}", table.join("."))
}

/// Whether a manifest read back from S3 says this platform is already built the way this
/// build would build it.
///
/// Three questions, all of which must answer yes: the conversion succeeded, it ran at a
/// version still in force, and the per-asset-type recipes it recorded are the ones in force
/// ([`abgen::recipes::recorded_is_current`]). The third is what lets a fix ship without a
/// version bump: the entity is reconverted, but every bundle whose recipes did not move
/// keeps its name and is reused off the CDN rather than rebuilt.
///
/// `versions` is both lanes, because the entity type is only known after the entity doc is
/// fetched and this gate deliberately runs before that — the whole point of it is to skip an
/// already-converted entity without paying a catalyst round-trip. Accepting either is safe
/// while the two are distinct strings: a manifest carries whichever version wrote it, so a
/// wearable's names its wearable lane and a scene's names its scene lane, and a bump of
/// either stops matching. Configure them equal and this collapses to the single-lane
/// behaviour it had before the split.
#[cfg(test)]
fn manifest_is_current(json: &serde_json::Value, versions: [&str; 2]) -> bool {
    current_lane(json, versions).is_some()
}

/// [`manifest_is_current`] with the answer's payload: the version lane the manifest
/// names, which is where its bundles live. `None` when the manifest is not current.
fn current_lane(json: &serde_json::Value, versions: [&str; 2]) -> Option<String> {
    let version = json.get("version").and_then(serde_json::Value::as_str)?;
    (json.get("exitCode").and_then(serde_json::Value::as_i64) == Some(0)
        && versions.contains(&version)
        && abgen::recipes::recorded_is_current(json.get("recipes")))
    .then(|| version.to_string())
}

/// `Some(lane)` when this platform is already converted the way this build would convert
/// it, where `lane` is the version lane its manifest names.
///
/// The lane is the manifest's and not the entity type's on purpose: it is what the
/// finished event reports to the registry, and the registry's record has to name the
/// prefix the bundles are actually under. A wearable converted before the lane split
/// sits under the scene lane with a manifest that says so, and stays current there
/// until a bump moves it; deriving its lane from its type would send clients to keys
/// that do not exist. The Redis marker carries the same lane as its value, so a marker
/// hit answers with it too — a marker from before lanes were recorded holds `"1"`,
/// which is not a lane in force and falls through to one manifest read that re-marks it.
pub fn converted_lane(
    proxy: &Arc<Proxy>,
    cfg: &Config,
    entity_id: &str,
    platform: &str,
) -> Option<String> {
    let lanes = [cfg.version.as_str(), cfg.wearable_version.as_str()];
    let marker = converted_marker_key(proxy, cfg, entity_id, platform);
    if let Some(key) = &marker {
        if let Some(lane) = abgen::rediscache::get(key) {
            if lanes.contains(&lane.as_str()) {
                return Some(lane);
            }
        }
    }
    let bytes = proxy.space_get_manifest(&format!("{entity_id}_{platform}"))?;
    let json = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    let lane = current_lane(&json, lanes)?;
    if let Some(key) = &marker {
        abgen::rediscache::mark_with(key, &lane);
    }
    Some(lane)
}

/// The configured platforms already converted the way this build would convert them,
/// each with the lane its manifest names — the platforms a job skips, and what their
/// finished events report.
pub fn already_converted(
    proxy: &Arc<Proxy>,
    cfg: &Config,
    entity_id: &str,
) -> Vec<(String, String)> {
    cfg.platforms
        .iter()
        .filter_map(|platform| {
            converted_lane(proxy, cfg, entity_id, platform).map(|lane| (platform.clone(), lane))
        })
        .collect()
}

/// The entity's type as the catalyst reports it, `scene` when the document does not
/// say — the same reading `Proxy::version_for` routes lanes by.
pub fn entity_type(entity_doc: &serde_json::Value) -> &str {
    entity_doc
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("scene")
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

    let mut scene_sources = 0usize;
    if entity_type(entity_doc) == "scene" && outcome.exit_code() == 0 {
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
/// for a conversion — `converted_lane` requires `exitCode == 0`. Written under the
/// scene lane whatever the entity is: this path runs when the job failed, possibly
/// before the entity document was ever fetched, so the type is not known here; and a
/// tombstone names no bundles, so no client resolves anything through its lane.
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

/// What the tombstone pass did with one platform.
pub struct TombstoneOutcome {
    pub platform: String,
    /// `Some(lane)` when a current manifest was found and left alone, naming the lane it
    /// lives under; `None` when a tombstone was written in its place.
    pub converted_lane: Option<String>,
}

/// One tombstone per platform without a good manifest; errors propagate —
/// a tombstone we cannot land must still reach the DLQ.
pub fn publish_failure_tombstones(
    cfg: &Config,
    proxy: &Arc<Proxy>,
    entity_id: &str,
    content_server: &str,
) -> Result<Vec<TombstoneOutcome>> {
    let mut outcomes = Vec::with_capacity(cfg.platforms.len());
    let mut tombstoned = 0u64;
    for platform in &cfg.platforms {
        let converted_lane = converted_lane(proxy, cfg, entity_id, platform);
        if converted_lane.is_none() {
            let bytes = failure_manifest(cfg, content_server, proxy.date());
            proxy
                .space_put_manifest_strict(&format!("{entity_id}_{platform}"), &bytes)
                .with_context(|| format!("tombstone manifest for {entity_id} {platform}"))?;
            tombstoned += 1;
        }
        outcomes.push(TombstoneOutcome {
            platform: platform.clone(),
            converted_lane,
        });
    }
    metrics::counter!("abgen_lambda_tombstones_total").increment(tombstoned);
    Ok(outcomes)
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
            wearable_version: "v49w".to_string(),
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
            lod_levels: vec![1],
        }
    }

    /// Points the S3 space at a fake server for the duration of the returned guard, which
    /// also holds the process-wide env lock so tests do not race on the variables.
    fn point_space_at(host: &str) -> (std::sync::MutexGuard<'static, ()>, EnvGuard) {
        let lock = crate::convert::TEST_SPACE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("ABGEN_S3_ENDPOINT", format!("http://{host}"));
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIATEST");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test-secret");
        (lock, EnvGuard)
    }

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
        // Recorded at this build's own generations, so the recipe question answers yes and
        // this test stays about which platforms get tombstoned.
        let good = serde_json::json!({
            "version": "v49", "files": ["x", "dcl"], "exitCode": 0,
            "contentServerUrl": "cs", "date": "d",
            "recipes": abgen::recipes::recorded_generations(&abgen::recipes::Recipe::ALL),
        })
        .to_string()
        .into_bytes();
        // The mac route serves an unparseable (empty) manifest on GET, so the
        // platform reads as unconverted; the same route accepts the PUT.
        let (host, seen) = serve(vec![
            ("/manifest/bafkpart_windows.json".to_string(), 200, good),
            ("/manifest/bafkpart_mac.json".to_string(), 200, Vec::new()),
        ]);
        let _space = point_space_at(&host);

        let cfg = cfg("tombstone-partial");
        let proxy = crate::convert::make_proxy(&cfg, "http://127.0.0.1:9");
        let outcomes = super::publish_failure_tombstones(
            &cfg,
            &proxy,
            "bafkpart",
            "https://peer.decentraland.org/content",
        )
        .unwrap();
        let summary: Vec<(&str, Option<&str>)> = outcomes
            .iter()
            .map(|o| (o.platform.as_str(), o.converted_lane.as_deref()))
            .collect();
        // windows keeps its manifest and reports the lane that manifest names; mac
        // gets a tombstone.
        assert_eq!(summary, vec![("windows", Some("v49")), ("mac", None)]);

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
    fn a_manifest_is_current_only_at_this_version_and_these_recipes() {
        let good = serde_json::json!({"version": "v49", "files": ["x"], "exitCode": 0});
        // No recipes block means "written before recipes existed": current exactly while
        // nothing has been bumped since, stale the moment anything has.
        assert_eq!(
            super::manifest_is_current(&good, ["v49", "v49w"]),
            !abgen::recipes::any_bumped()
        );
        assert!(!super::manifest_is_current(&good, ["v50", "v50w"]));

        let failed = serde_json::json!({"version": "v49", "files": [], "exitCode": 12});
        assert!(!super::manifest_is_current(&failed, ["v49", "v49w"]));

        let skin = abgen::recipes::Recipe::Skin;
        let with_recipes = |gen: u64| {
            let mut m = good.clone();
            let mut block = serde_json::Map::new();
            block.insert(skin.name().to_string(), serde_json::Value::from(gen));
            m["recipes"] = serde_json::Value::Object(block);
            m
        };

        // A block naming a generation this build does not have is stale, whatever the
        // version says — that is the whole point of shipping a fix without bumping it.
        assert!(!super::manifest_is_current(
            &with_recipes(u64::from(skin.generation()) + 1),
            ["v49", "v49w"]
        ));
        // A block recording exactly what is in force is current.
        assert!(super::manifest_is_current(
            &with_recipes(u64::from(skin.generation())),
            ["v49", "v49w"]
        ));

        // AB_VERSION stays total. Recipes are a conjunct, never a substitute: a manifest
        // whose recipes are perfectly current is still stale at a bumped AB_VERSION, so a
        // version bump reconverts everything exactly as it did before recipes existed.
        assert!(!super::manifest_is_current(
            &with_recipes(u64::from(skin.generation())),
            ["v50", "v50w"]
        ));
    }

    /// Regression for the v0.19.0 registry records: every wearable's finished event said
    /// `AB_VERSION` while its bundles sat under `WEARABLE_AB_VERSION`, and the registry
    /// (which stores the event's version verbatim, status 13 included) sent every client
    /// to 404s. This walks the skip path from the manifest on S3 to the event body and
    /// pins the version to the manifest's lane — never to a config default.
    #[test]
    fn the_registry_hears_the_lane_the_manifest_names_not_the_config_default() {
        let recipes = abgen::recipes::recorded_generations(&abgen::recipes::Recipe::ALL);
        // A wearable published after the lane split: both platforms under the wearable
        // lane, which is not the scene lane `cfg.version` holds.
        let wearable_manifest = |platform: &str| {
            serde_json::json!({
                "version": "v49w", "files": [format!("qmhash_{platform}"), "dcl"],
                "exitCode": 0, "contentServerUrl": "cs", "date": "d", "recipes": recipes,
            })
            .to_string()
            .into_bytes()
        };
        let (host, _seen) = serve(vec![
            (
                "/manifest/bafkwear_windows.json".to_string(),
                200,
                wearable_manifest("windows"),
            ),
            (
                "/manifest/bafkwear_mac.json".to_string(),
                200,
                wearable_manifest("mac"),
            ),
        ]);
        let _space = point_space_at(&host);

        let cfg = cfg("registry-lane");
        assert_ne!(
            cfg.version, cfg.wearable_version,
            "the test needs two lanes"
        );
        let proxy = crate::convert::make_proxy(&cfg, "http://127.0.0.1:9");

        // The skip path: every platform is already converted, and the job notifies
        // without ever fetching the entity — so the lane can only come from the manifest.
        let already = super::already_converted(&proxy, &cfg, "bafkwear");
        assert_eq!(already.len(), 2, "{already:?}");
        let finished: Vec<crate::notify::Finished> = already
            .iter()
            .map(|(p, lane)| crate::notify::Finished::already_converted(p, lane))
            .collect();
        let events = crate::notify::finished_events("bafkwear", "cs", 0, &finished);
        assert_eq!(events.len(), 2);
        for event in &events {
            let version = event["metadata"]["version"].as_str().unwrap();
            assert_eq!(
                version, "v49w",
                "{} must report the lane its manifest names: {event}",
                event["metadata"]["platform"]
            );
            assert_ne!(
                version, cfg.version,
                "the scene lane is the config default, not where this wearable is"
            );
        }

        // The build path routes by entity type the same way the writer does, so a fresh
        // wearable conversion reports the wearable lane too.
        let wearable_doc = serde_json::json!({ "type": "wearable" });
        assert_eq!(
            proxy.version_for(super::entity_type(&wearable_doc)),
            cfg.wearable_version
        );
        let scene_doc = serde_json::json!({ "type": "scene" });
        assert_eq!(
            proxy.version_for(super::entity_type(&scene_doc)),
            cfg.version
        );
    }

    #[test]
    fn a_current_manifest_names_the_lane_its_bundles_live_under() {
        let recipes = abgen::recipes::recorded_generations(&abgen::recipes::Recipe::ALL);
        let pre_split_wearable = serde_json::json!({
            "version": "v49", "exitCode": 0, "recipes": recipes,
        });
        let post_split_wearable = serde_json::json!({
            "version": "v49w", "exitCode": 0, "recipes": recipes,
        });
        let failed = serde_json::json!({ "version": "v49w", "exitCode": 5 });
        // The lane is read off the manifest, not inferred from the entity: a wearable
        // still sitting under the scene lane reports the scene lane.
        assert_eq!(
            super::current_lane(&pre_split_wearable, ["v49", "v49w"]).as_deref(),
            Some("v49")
        );
        assert_eq!(
            super::current_lane(&post_split_wearable, ["v49", "v49w"]).as_deref(),
            Some("v49w")
        );
        assert_eq!(super::current_lane(&failed, ["v49", "v49w"]), None);
        assert_eq!(
            super::current_lane(&post_split_wearable, ["v49", "v50w"]),
            None
        );
    }

    #[test]
    fn the_two_version_lanes_invalidate_independently() {
        // The point of the split: a scene manifest survives a wearable-lane bump, and a
        // wearable manifest survives a scene-lane bump. Neither drags the other.
        // Both carry this build's generations: the recipe question is settled, so what is
        // left under test is purely the two version lanes.
        let scene = serde_json::json!({
            "version": "v49", "files": ["x"], "exitCode": 0,
            "recipes": abgen::recipes::recorded_generations(&abgen::recipes::Recipe::ALL),
        });
        let wearable = serde_json::json!({
            "version": "v49w", "files": ["x"], "exitCode": 0,
            "recipes": abgen::recipes::recorded_generations(&abgen::recipes::Recipe::ALL),
        });

        assert!(super::manifest_is_current(&scene, ["v49", "v49w"]));
        assert!(super::manifest_is_current(&wearable, ["v49", "v49w"]));

        // Wearable lane bumped, scene lane untouched.
        assert!(super::manifest_is_current(&scene, ["v49", "v50w"]));
        assert!(!super::manifest_is_current(&wearable, ["v49", "v50w"]));

        // Scene lane bumped, wearable lane untouched.
        assert!(!super::manifest_is_current(&scene, ["v50", "v49w"]));
        assert!(super::manifest_is_current(&wearable, ["v50", "v49w"]));
    }

    #[test]
    fn the_converted_marker_is_scoped_to_the_recipe_table() {
        // Empty while nothing is bumped, so no key already in Redis moves on adoption.
        assert_eq!(
            super::recipe_marker_scope().is_empty(),
            !abgen::recipes::any_bumped()
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
