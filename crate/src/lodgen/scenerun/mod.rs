pub mod crdt;

#[cfg(not(target_arch = "wasm32"))]
mod driver;

#[cfg(not(target_arch = "wasm32"))]
mod engine;

#[cfg(not(target_arch = "wasm32"))]
pub use engine::QuickJsEngine;

#[cfg(all(
    feature = "engine-v8",
    not(target_arch = "wasm32"),
    not(all(target_os = "windows", target_env = "gnu"))
))]
mod engine_v8;

#[cfg(all(
    feature = "engine-v8",
    not(target_arch = "wasm32"),
    not(all(target_os = "windows", target_env = "gnu"))
))]
pub use engine_v8::V8Engine;

#[cfg(not(target_arch = "wasm32"))]
pub struct EngineLimits {
    pub memory_bytes: usize,
    pub stack_bytes: usize,
    pub deadline: Option<std::time::Duration>,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for EngineLimits {
    fn default() -> Self {
        EngineLimits {
            memory_bytes: 256 << 20,
            stack_bytes: 1 << 20,
            deadline: crate::lodgen::simplify::subproc_deadline(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub const SDK6_ADAPTION_LAYER_URL: &str =
    "https://renderer-artifacts.decentraland.org/sdk6-adaption-layer/main/index.js";

#[cfg(not(target_arch = "wasm32"))]
pub const SDK6_ADAPTION_URL_ENV: &str = "ABGEN_LOD_SDK6_ADAPTION_URL";

#[cfg(not(target_arch = "wasm32"))]
pub type ReadFileFn = Box<dyn Fn(&str) -> anyhow::Result<(Vec<u8>, String)> + Send + 'static>;

#[cfg(not(target_arch = "wasm32"))]
pub struct SceneJob {
    pub code: String,
    pub main_crdt: Option<Vec<u8>>,
    pub read_file: ReadFileFn,
    pub limits: EngineLimits,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, Default)]
pub struct CaptureOutcome {
    pub sent: bool,
    pub stream: Vec<u8>,
}

#[cfg(not(target_arch = "wasm32"))]
pub trait SceneEngine {
    fn run_capture(&self, job: SceneJob) -> anyhow::Result<CaptureOutcome>;
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn initial_state_parts(main_crdt: Option<&[u8]>) -> Vec<Vec<u8>> {
    let stream = crdt::synthetic_initial_state(None);
    let mut parts = Vec::new();
    let mut off = 0usize;
    while off + 8 <= stream.len() {
        let len = u32::from_le_bytes(stream[off..off + 4].try_into().unwrap()) as usize;
        if len < 8 || off + len > stream.len() {
            break;
        }
        parts.push(stream[off..off + len].to_vec());
        off += len;
    }
    if let Some(bytes) = main_crdt {
        parts.push(bytes.to_vec());
    }
    parts
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_sdk6_adaption_layer() -> anyhow::Result<String> {
    use anyhow::Context;
    let source = std::env::var(SDK6_ADAPTION_URL_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| SDK6_ADAPTION_LAYER_URL.to_string());
    if !source.starts_with("http://") && !source.starts_with("https://") {
        return std::fs::read_to_string(&source)
            .with_context(|| format!("read sdk6 adaption layer from {source}"));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(120)))
        .build()
        .into();
    let resp = agent
        .get(&source)
        .header("User-Agent", crate::catalyst::UA)
        .call()
        .map_err(|e| anyhow::anyhow!("GET {source}: {e}"))?;
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_body().into_reader().read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(all(
    feature = "engine-v8",
    not(target_arch = "wasm32"),
    not(all(target_os = "windows", target_env = "gnu"))
))]
fn default_engine() -> impl SceneEngine {
    V8Engine
}

#[cfg(all(
    not(target_arch = "wasm32"),
    not(target_arch = "wasm32"),
    not(all(
        feature = "engine-v8",
        not(all(target_os = "windows", target_env = "gnu"))
    ))
))]
fn default_engine() -> impl SceneEngine {
    QuickJsEngine
}

#[cfg(not(target_arch = "wasm32"))]
pub fn run_scene_placements(
    client: &crate::catalyst::CatalystClient,
    ent: &crate::catalyst::Scene,
) -> anyhow::Result<Option<crate::lodgen::placements::ManifestPlacements>> {
    run_scene_with(&default_engine(), client, ent)
}

#[cfg(not(target_arch = "wasm32"))]
fn run_scene_with(
    engine: &dyn SceneEngine,
    client: &crate::catalyst::CatalystClient,
    ent: &crate::catalyst::Scene,
) -> anyhow::Result<Option<crate::lodgen::placements::ManifestPlacements>> {
    use anyhow::{anyhow, Context};
    let sdk7 = ent.metadata.get("runtimeVersion").and_then(|v| v.as_str()) == Some("7");
    let content = ent.content_by_file();
    let (code, main_crdt) = if sdk7 {
        let main_crdt = content
            .get("main.crdt")
            .map(|hash| client.fetch_content(hash))
            .transpose()
            .with_context(|| format!("fetch main.crdt for scene {}", ent.entity_id))?;
        let main = ent
            .metadata
            .get("main")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("scene {} metadata has no main entry", ent.entity_id))?;
        let bundle_hash = content
            .get(&main.to_lowercase())
            .ok_or_else(|| anyhow!("scene {} content does not list main {main}", ent.entity_id))?;
        let bundle = client
            .fetch_content(bundle_hash)
            .with_context(|| format!("fetch scene bundle {main} for {}", ent.entity_id))?;
        (String::from_utf8_lossy(&bundle).into_owned(), main_crdt)
    } else {
        // sdk6 scenes run the adaption layer, which loads the scene's own
        // game.js and assets through Runtime.readFile
        (fetch_sdk6_adaption_layer()?, None)
    };
    let read_content = content.clone();
    let read_client = client.clone();
    let read_file: ReadFileFn = Box::new(move |name| {
        let hash = read_content
            .get(&name.to_lowercase())
            .ok_or_else(|| anyhow!("scene content does not list file {name}"))?;
        Ok((read_client.fetch_content(hash)?, hash.clone()))
    });
    let job = SceneJob {
        code,
        main_crdt,
        read_file,
        limits: EngineLimits::default(),
    };
    let outcome = engine.run_capture(job)?;
    if outcome.stream.is_empty() {
        return Ok(None);
    }
    Ok(Some(crdt::placements_from_crdt(&outcome.stream, &content)))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn initial_state_parts_mirror_the_getstate_array() {
        let parts = initial_state_parts(None);
        assert_eq!(parts.len(), 4);
        let mut joined = Vec::new();
        for p in &parts {
            joined.extend_from_slice(p);
        }
        assert_eq!(joined, crdt::synthetic_initial_state(None));
        let tail = [1u8, 2, 3];
        let with = initial_state_parts(Some(&tail));
        assert_eq!(with.len(), 5);
        assert_eq!(with[4], tail);
    }

    const FAKE_ADAPTION_LAYER: &str = r#"
const engineApi = require('~system/EngineApi');
let done = false;
module.exports.onUpdate = async function () {
  if (done) return;
  done = true;
  const rf = await require('~system/Runtime').readFile({ fileName: 'Models/Scene.glb' });
  if (rf.hash !== 'hscene' || new Uint8Array(rf.content).length !== 2) return;
  const src = 'models/scene.glb';
  const data = [0x0a, src.length];
  for (let i = 0; i < src.length; i++) data.push(src.charCodeAt(i));
  const len = 24 + data.length;
  const buf = new ArrayBuffer(len);
  const v = new DataView(buf);
  v.setUint32(0, len, true);
  v.setUint32(4, 1, true);
  v.setUint32(8, 600, true);
  v.setUint32(12, 1041, true);
  v.setUint32(16, 1, true);
  v.setUint32(20, data.length, true);
  new Uint8Array(buf, 24).set(data);
  await engineApi.crdtSendToRenderer({ data: new Uint8Array(buf) });
};
"#;

    #[test]
    fn sdk6_lane_runs_a_local_adaption_layer() {
        let dir = std::env::temp_dir().join(format!("abgen_sdk6_lane_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let layer = dir.join("adaption.js");
        std::fs::write(&layer, FAKE_ADAPTION_LAYER).unwrap();
        std::env::set_var(SDK6_ADAPTION_URL_ENV, &layer);
        crate::local_store::LocalContentStore::new(&dir)
            .write("hscene", &[1, 2])
            .unwrap();
        let client = crate::catalyst::CatalystClient::from_args(
            crate::catalyst::DEFAULT_CATALYST,
            Some(dir.to_str().unwrap()),
        );
        let ent = crate::catalyst::Scene {
            entity_id: "test-sdk6".into(),
            entity_type: "scene".into(),
            pointers: vec![],
            content: vec![crate::catalyst::ContentEntry {
                file: "Models/Scene.glb".into(),
                hash: "hscene".into(),
            }],
            metadata: serde_json::json!({"runtimeVersion": "6"}),
        };
        let got = run_scene_placements(&client, &ent);
        std::env::remove_var(SDK6_ADAPTION_URL_ENV);
        let _ = std::fs::remove_dir_all(&dir);
        let full = got.unwrap().expect("sdk6 scene should emit placements");
        assert_eq!(full.placements.len(), 1);
        let p = &full.placements[0];
        assert_eq!(p.glb_file.as_deref(), Some("models/scene.glb"));
        assert_eq!(p.glb_hash.as_deref(), Some("hscene"));
    }

    const SDK7_GOLDEN_COORDS: &str = "-150,150";
    const SDK7_GOLDEN_ENTITY: &str = "bafkreiau2nk2wuki5tw42runje2grjqza7tcfvzabc554jqsxc42ax3nqm";
    const SDK7_GOLDEN_PLACEMENTS: &str =
        include_str!("../testdata/golden_sdk7_-150_150.placements.json");
    const SDK6_GOLDEN_COORDS: &str = "100,100";
    const SDK6_GOLDEN_ENTITY: &str = "QmVDhg6mQyBBnyk36N6YWHH8dbLYM8kpUaH2VxmwZKFj6T";
    const SDK6_GOLDEN_PLACEMENTS: &str =
        include_str!("../testdata/golden_sdk6_100_100.placements.json");
    // the capture base GOLDENS.md records
    const GOLDEN_CATALYST: &str = "https://peer.decentraland.org/content";

    fn golden_scene(
        coords: &str,
        pinned_entity: &str,
    ) -> (crate::catalyst::CatalystClient, crate::catalyst::Scene) {
        let client = crate::catalyst::CatalystClient::new(GOLDEN_CATALYST);
        let ent = client.resolve_scene(coords).unwrap();
        assert_eq!(
            ent.entity_id, pinned_entity,
            "scene redeployed; re-capture goldens ({coords})"
        );
        (client, ent)
    }

    fn assert_golden(coords: &str, pinned_entity: &str, golden: &str) {
        let (client, ent) = golden_scene(coords, pinned_entity);
        let full = run_scene_placements(&client, &ent)
            .unwrap()
            .unwrap_or_default();
        let got = serde_json::to_string_pretty(&full.placements).unwrap();
        assert_eq!(got.trim(), golden.trim(), "{coords}");
    }

    #[test]
    #[ignore = "network: resolves the golden scene on peer.decentraland.org"]
    fn golden_sdk7_minus150_150() {
        assert_golden(
            SDK7_GOLDEN_COORDS,
            SDK7_GOLDEN_ENTITY,
            SDK7_GOLDEN_PLACEMENTS,
        );
    }

    #[test]
    #[ignore = "network: resolves the golden scene and fetches the live adaption layer"]
    fn golden_sdk6_100_100() {
        assert_golden(
            SDK6_GOLDEN_COORDS,
            SDK6_GOLDEN_ENTITY,
            SDK6_GOLDEN_PLACEMENTS,
        );
    }

    #[test]
    fn golden_manifests_cross_check_the_compose_seam() {
        use crate::lodgen::placements::parse_lod_manifest_full;
        use std::collections::HashMap;
        let sdk7 = parse_lod_manifest_full(
            include_bytes!("../testdata/golden_sdk7_-150_150.manifest.json"),
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&sdk7.placements)
                .unwrap()
                .trim(),
            SDK7_GOLDEN_PLACEMENTS.trim()
        );
        let mut content = HashMap::new();
        content.insert(
            "models/scene.glb".to_string(),
            "QmQgQtuAg9qsdrmLwnFiLRAYZ6Du4Dp7Yh7bw7ELn7AqkD".to_string(),
        );
        let sdk6 = parse_lod_manifest_full(
            include_bytes!("../testdata/golden_sdk6_100_100.manifest.json"),
            &content,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_string_pretty(&sdk6.placements)
                .unwrap()
                .trim(),
            SDK6_GOLDEN_PLACEMENTS.trim()
        );
        assert_eq!(sdk6.unresolved_src, 0);
    }

    #[test]
    #[ignore = "network+bench: times the golden-scene frame loops against the subprocess budget"]
    fn bench_golden_scene_frame_loop() {
        use std::collections::HashMap;
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};
        let budget =
            crate::lodgen::simplify::subproc_deadline().unwrap_or(Duration::from_secs(900));
        struct Row {
            scene: &'static str,
            engine: &'static str,
            cold: Duration,
            warm: Duration,
        }
        let mut rows: Vec<Row> = Vec::new();
        for (coords, pinned) in [
            (SDK7_GOLDEN_COORDS, SDK7_GOLDEN_ENTITY),
            (SDK6_GOLDEN_COORDS, SDK6_GOLDEN_ENTITY),
        ] {
            let (client, ent) = golden_scene(coords, pinned);
            let sdk7 = ent.metadata.get("runtimeVersion").and_then(|v| v.as_str()) == Some("7");
            let content = ent.content_by_file();
            let (code, main_crdt) = if sdk7 {
                let main = ent.metadata.get("main").and_then(|v| v.as_str()).unwrap();
                let bundle = client
                    .fetch_content(content.get(&main.to_lowercase()).unwrap())
                    .unwrap();
                let main_crdt = content
                    .get("main.crdt")
                    .map(|h| client.fetch_content(h).unwrap());
                (String::from_utf8_lossy(&bundle).into_owned(), main_crdt)
            } else {
                (fetch_sdk6_adaption_layer().unwrap(), None)
            };
            let cache: Arc<Mutex<HashMap<String, (Vec<u8>, String)>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let make_job = || {
                let cache = cache.clone();
                let content = content.clone();
                let client = client.clone();
                SceneJob {
                    code: code.clone(),
                    main_crdt: main_crdt.clone(),
                    read_file: Box::new(move |name| {
                        let key = name.to_lowercase();
                        if let Some(hit) = cache.lock().unwrap().get(&key) {
                            return Ok(hit.clone());
                        }
                        let hash = content
                            .get(&key)
                            .ok_or_else(|| {
                                anyhow::anyhow!("scene content does not list file {name}")
                            })?
                            .clone();
                        let bytes = client.fetch_content(&hash)?;
                        cache
                            .lock()
                            .unwrap()
                            .insert(key, (bytes.clone(), hash.clone()));
                        Ok((bytes, hash))
                    }),
                    limits: EngineLimits::default(),
                }
            };
            // priming run: fills the content cache so timed runs stay off the network
            QuickJsEngine.run_capture(make_job()).unwrap();
            let engines: Vec<(&'static str, Box<dyn SceneEngine>)> = vec![
                ("quickjs", Box::new(QuickJsEngine)),
                #[cfg(all(
                    feature = "engine-v8",
                    not(all(target_os = "windows", target_env = "gnu"))
                ))]
                ("v8", Box::new(V8Engine)),
            ];
            for (name, engine) in engines {
                let timed = || {
                    let t = Instant::now();
                    engine.run_capture(make_job()).unwrap();
                    t.elapsed()
                };
                let cold = timed();
                let warm = (0..3).map(|_| timed()).min().unwrap();
                rows.push(Row {
                    scene: coords,
                    engine: name,
                    cold,
                    warm,
                });
            }
        }
        println!(
            "frame-loop capture vs the npm subprocess budget: {}s \
             (env {} or the 900s the goldens were captured under)",
            budget.as_secs(),
            crate::lodgen::simplify::SUBPROC_TIMEOUT_ENV
        );
        println!(
            "{:<10} {:<8} {:>10} {:>10} {:>12}",
            "scene", "engine", "cold_ms", "warm_ms", "cold/budget"
        );
        for r in &rows {
            println!(
                "{:<10} {:<8} {:>10.1} {:>10.1} {:>11.3}%",
                r.scene,
                r.engine,
                r.cold.as_secs_f64() * 1000.0,
                r.warm.as_secs_f64() * 1000.0,
                100.0 * r.cold.as_secs_f64() / budget.as_secs_f64()
            );
        }
        let over: Vec<String> = rows
            .iter()
            .filter(|r| r.engine == "quickjs" && r.cold > budget / 2)
            .map(|r| {
                format!(
                    "{} quickjs cold {:?} exceeds half the {:?} budget",
                    r.scene, r.cold, budget
                )
            })
            .collect();
        assert!(over.is_empty(), "{}", over.join("; "));
    }
}
