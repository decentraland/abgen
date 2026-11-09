//! Scene execution in a separate AWS Lambda function.
//!
//! Some runtimes can host the scene child of [`super::sandbox`] but cannot confine it:
//! AWS Lambda refuses both seccomp and `PR_SET_DUMPABLE`, so a child there shares the
//! parent's user, address-space visibility and environment through `/proc`, and the
//! self-test refuses to run scenes. In that case scenes run in a function of their own,
//! whose execution environment is a separate microVM with its own (minimal) role:
//!
//! - **caller** (the converter): with [`FUNCTION_ENV`] set, [`super::run_scene_placements`]
//!   sends the entity and its content server(s) to that function with a synchronous
//!   `Invoke` and projects the returned renderer stream exactly as a local run would.
//!   It treats the response as untrusted: only the stream, the evaluation error and the
//!   files the scene read are taken from it, and a file is kept only when the entity's
//!   own content lists it under the same hash.
//! - **function** (`abgen-lambda` started with `ABGEN_LAMBDA_MODE=scenerun`): [`serve`]
//!   fetches the scene's code and files from the content server and runs it in-process
//!   on [`super::QuickJsEngine`]. The binary posts the result and exits, so the next
//!   invocation starts a fresh process.
//!
//! The function's response is limited to 6 MB by Lambda; a larger renderer stream fails
//! that scene's LOD build.

use super::{capture_scene, CaptureOutcome, QuickJsEngine, SceneCapture};
use crate::catalyst::{CatalystClient, Scene};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

/// Name of the function that runs scenes. Unset: scenes run in a local child process.
pub const FUNCTION_ENV: &str = "ABGEN_SCENERUN_FUNCTION";
/// Overrides the Lambda endpoint (tests, local stacks).
pub const ENDPOINT_ENV: &str = "ABGEN_SCENERUN_ENDPOINT";
/// Seconds to wait for one scene, including the function's cold start (default 300).
pub const TIMEOUT_ENV: &str = "ABGEN_SCENERUN_TIMEOUT_S";

const DEFAULT_TIMEOUT_S: u64 = 300;
/// Marks a scenerun request or response and versions its shape.
const PROTOCOL_KEY: &str = "abgenScenerun";
const PROTOCOL_VERSION: u64 = 1;
const MAX_EVAL_ERROR_CHARS: usize = 2000;

pub struct RemoteRunner {
    invoker: crate::lambda_invoke::LambdaInvoker,
}

/// The configured runner, built once per process. `None` when [`FUNCTION_ENV`] is unset.
pub fn runner() -> Option<Result<&'static RemoteRunner>> {
    static RUNNER: OnceLock<Option<std::result::Result<RemoteRunner, String>>> = OnceLock::new();
    let slot = RUNNER.get_or_init(|| {
        let function = std::env::var(FUNCTION_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())?;
        Some(RemoteRunner::from_env(&function).map_err(|e| format!("{e:#}")))
    });
    slot.as_ref().map(|r| {
        r.as_ref()
            .map_err(|e| anyhow!("scene runner {FUNCTION_ENV} unusable: {e}"))
    })
}

impl RemoteRunner {
    fn from_env(function: &str) -> Result<RemoteRunner> {
        let region = std::env::var("AWS_REGION")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "us-east-1".to_string());
        let endpoint = std::env::var(ENDPOINT_ENV).ok();
        let timeout = match std::env::var(TIMEOUT_ENV) {
            Ok(v) if !v.trim().is_empty() => v
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|s| *s > 0)
                .ok_or_else(|| anyhow!("{TIMEOUT_ENV}={v:?} is not a positive integer"))?,
            _ => DEFAULT_TIMEOUT_S,
        };
        let invoker = crate::lambda_invoke::LambdaInvoker::new(
            function,
            &region,
            endpoint.as_deref(),
            Duration::from_secs(timeout),
        )?;
        tracing::info!(
            function,
            timeout_s = timeout,
            "scenes run in a separate function"
        );
        Ok(RemoteRunner { invoker })
    }

    /// Runs `ent` in the function and returns what it produced.
    pub fn capture(&self, client: &CatalystClient, ent: &Scene) -> Result<SceneCapture> {
        let response = self
            .invoker
            .invoke(&request(client, ent))
            .with_context(|| {
                format!("run scene {} in {}", ent.entity_id, self.invoker.function())
            })?;
        parse_response(ent, &response)
    }
}

/// The invoke payload: the entity as the content server lists it, and where to fetch it.
pub fn request(client: &CatalystClient, ent: &Scene) -> serde_json::Value {
    let content: Vec<serde_json::Value> = ent
        .content
        .iter()
        .map(|c| serde_json::json!({"file": c.file, "hash": c.hash}))
        .collect();
    serde_json::json!({
        PROTOCOL_KEY: PROTOCOL_VERSION,
        "contentServer": client.base_url(),
        "fallbackContentServers": client.fallback_bases(),
        "entity": {
            "id": ent.entity_id,
            "type": ent.entity_type,
            "pointers": ent.pointers,
            "content": content,
            "metadata": ent.metadata,
        },
    })
}

/// Whether an invocation payload is a scenerun request.
pub fn is_request(event: &serde_json::Value) -> bool {
    event.get(PROTOCOL_KEY).is_some()
}

/// The content servers a request asks the function to fetch from, for the caller to
/// check against its allowlist before [`serve`].
pub fn requested_content_servers(event: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = event
        .get("contentServer")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .into_iter()
        .collect();
    if let Some(list) = event
        .get("fallbackContentServers")
        .and_then(|v| v.as_array())
    {
        out.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_string));
    }
    out
}

/// Function side: runs the requested scene in-process and returns the response payload.
/// `check_server` vets every content server in the request before anything is fetched.
pub fn serve(
    event: &serde_json::Value,
    check_server: &dyn Fn(&str) -> Result<()>,
) -> Result<serde_json::Value> {
    let version = event.get(PROTOCOL_KEY).and_then(|v| v.as_u64());
    if version != Some(PROTOCOL_VERSION) {
        bail!("unsupported {PROTOCOL_KEY} version {version:?}");
    }
    let base = event
        .get("contentServer")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("scenerun request has no contentServer"))?;
    for server in requested_content_servers(event) {
        check_server(&server)?;
    }
    let mut client = CatalystClient::new(base);
    if let Some(list) = event
        .get("fallbackContentServers")
        .and_then(|v| v.as_array())
    {
        for fallback in list.iter().filter_map(|v| v.as_str()) {
            client = client.with_fallback_base(fallback.to_string());
        }
    }
    let entity = event
        .get("entity")
        .ok_or_else(|| anyhow!("scenerun request has no entity"))?;
    let ent = CatalystClient::parse_entity(entity)?;
    let mut capture = capture_scene(&QuickJsEngine, &client, &ent)?;
    // Every frame the scene sent, with `main.crdt` ahead of each, folds to far less;
    // only the folded state counts against the response limit.
    capture.outcome.stream = super::crdt::compact_stream(&capture.outcome.stream);
    Ok(response(&capture))
}

fn response(capture: &SceneCapture) -> serde_json::Value {
    serde_json::json!({
        PROTOCOL_KEY: PROTOCOL_VERSION,
        "sent": capture.outcome.sent,
        "stream": base64_encode(&capture.outcome.stream),
        "evalError": capture.outcome.eval_error,
        "filesRead": capture.files_read,
    })
}

/// Caller side: reads a function response for `ent`, keeping only what it can check.
fn parse_response(ent: &Scene, value: &serde_json::Value) -> Result<SceneCapture> {
    let version = value.get(PROTOCOL_KEY).and_then(|v| v.as_u64());
    if version != Some(PROTOCOL_VERSION) {
        bail!("scene runner answered with {PROTOCOL_KEY} version {version:?}");
    }
    let stream = match value.get("stream").and_then(|v| v.as_str()) {
        Some("") | None => Vec::new(),
        Some(s) => crate::gltf::base64_decode(s)
            .ok_or_else(|| anyhow!("scene runner stream is not base64"))?,
    };
    let eval_error = value
        .get("evalError")
        .and_then(|v| v.as_str())
        .map(|e| e.chars().take(MAX_EVAL_ERROR_CHARS).collect::<String>());
    let content = ent.content_by_file();
    let mut files_read = BTreeMap::new();
    if let Some(map) = value.get("filesRead").and_then(|v| v.as_object()) {
        for (file, hash) in map {
            let Some(hash) = hash.as_str() else { continue };
            let file = file.to_lowercase();
            if content.get(&file).map(String::as_str) == Some(hash) {
                files_read.insert(file, hash.to_string());
            }
        }
    }
    Ok(SceneCapture {
        outcome: CaptureOutcome {
            sent: value.get("sent").and_then(|v| v.as_bool()).unwrap_or(false),
            stream,
            eval_error,
        },
        files_read,
    })
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalyst::ContentEntry;

    fn scene() -> Scene {
        Scene {
            entity_id: "bafkScene".into(),
            entity_type: "scene".into(),
            pointers: vec!["0,0".into()],
            content: vec![
                ContentEntry {
                    file: "Models/Scene.glb".into(),
                    hash: "hscene".into(),
                },
                ContentEntry {
                    file: "data.json".into(),
                    hash: "hdata".into(),
                },
            ],
            metadata: serde_json::json!({"runtimeVersion": "7", "main": "bin/index.js"}),
        }
    }

    #[test]
    fn base64_round_trips() {
        for len in 0..40usize {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let enc = base64_encode(&bytes);
            assert_eq!(enc.len() % 4, 0);
            let dec = if enc.is_empty() {
                Vec::new()
            } else {
                crate::gltf::base64_decode(&enc).unwrap()
            };
            assert_eq!(dec, bytes, "len {len}");
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn request_carries_the_entity_and_servers() {
        let client = CatalystClient::new("https://peer.example.org/content/")
            .with_fallback_base("https://worlds.example.org".into());
        let req = request(&client, &scene());
        assert!(is_request(&req));
        assert_eq!(
            requested_content_servers(&req),
            vec![
                "https://peer.example.org/content".to_string(),
                "https://worlds.example.org".to_string()
            ]
        );
        let back = CatalystClient::parse_entity(&req["entity"]).unwrap();
        assert_eq!(back.entity_id, "bafkScene");
        assert_eq!(back.content_by_file(), scene().content_by_file());
        assert_eq!(back.metadata, scene().metadata);
        assert!(!is_request(&serde_json::json!({"Records": []})));
    }

    #[test]
    fn response_round_trips_and_untrusted_fields_are_filtered() {
        let mut files_read = BTreeMap::new();
        files_read.insert("data.json".to_string(), "hdata".to_string());
        let capture = SceneCapture {
            outcome: CaptureOutcome {
                sent: true,
                stream: vec![1, 2, 3, 4, 5],
                eval_error: None,
            },
            files_read,
        };
        let mut value = response(&capture);
        // A hash the entity does not list, a file it does not ship, and a casing
        // variant of a real file: only the last survives, lowercased.
        value["filesRead"]["models/scene.glb"] = serde_json::json!("hother");
        value["filesRead"]["secret.txt"] = serde_json::json!("hx");
        value["filesRead"]["DATA.JSON"] = serde_json::json!("hdata");
        value["evalError"] = serde_json::json!("e".repeat(5000));
        let got = parse_response(&scene(), &value).unwrap();
        assert!(got.outcome.sent);
        assert_eq!(got.outcome.stream, vec![1, 2, 3, 4, 5]);
        assert_eq!(
            got.files_read.into_iter().collect::<Vec<_>>(),
            vec![("data.json".to_string(), "hdata".to_string())]
        );
        assert_eq!(got.outcome.eval_error.unwrap().len(), MAX_EVAL_ERROR_CHARS);
    }

    #[test]
    fn responses_without_the_protocol_marker_are_rejected() {
        let err = parse_response(&scene(), &serde_json::json!({"stream": ""})).unwrap_err();
        assert!(err.to_string().contains("version"), "{err}");
        let err = parse_response(
            &scene(),
            &serde_json::json!({PROTOCOL_KEY: 1, "stream": "!!not base64!!"}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("base64"), "{err}");
    }

    #[test]
    fn serve_checks_every_server_before_fetching() {
        let client = CatalystClient::new("https://peer.example.org/content")
            .with_fallback_base("http://169.254.169.254".into());
        let req = request(&client, &scene());
        let err = serve(&req, &|s: &str| {
            if s.starts_with("https://") {
                Ok(())
            } else {
                bail!("refused {s}")
            }
        })
        .unwrap_err();
        assert!(
            err.to_string().contains("refused http://169.254.169.254"),
            "{err}"
        );
        let err = serve(&serde_json::json!({PROTOCOL_KEY: 2}), &|_: &str| Ok(())).unwrap_err();
        assert!(err.to_string().contains("version"), "{err}");
    }

    /// The function runs the scene on the in-process engine against a local content
    /// store; the caller's projection of the response matches a direct local run.
    #[test]
    fn served_capture_projects_like_a_local_run() {
        let dir =
            std::env::temp_dir().join(format!("abgen_scenerun_remote_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = crate::local_store::LocalContentStore::new(&dir);
        let code = r#"
const engineApi = require('~system/EngineApi');
let done = false;
module.exports.onUpdate = async function () {
  if (done) return;
  done = true;
  await require('~system/Runtime').readFile({ fileName: 'data.json' });
  const src = 'models/scene.glb';
  const data = [10, src.length];
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
        store.write("hmain", code.as_bytes()).unwrap();
        store.write("hdata", b"{}").unwrap();
        let mut ent = scene();
        ent.content.push(ContentEntry {
            file: "bin/index.js".into(),
            hash: "hmain".into(),
        });
        let local = CatalystClient::from_args(
            crate::catalyst::DEFAULT_CATALYST,
            Some(dir.to_str().unwrap()),
        );
        let captured = capture_scene(&QuickJsEngine, &local, &ent).unwrap();
        let value = response(&captured);
        let parsed = parse_response(&ent, &value).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!parsed.outcome.stream.is_empty());
        assert_eq!(parsed.outcome.stream, captured.outcome.stream);
        assert_eq!(
            parsed.files_read.get("data.json").map(String::as_str),
            Some("hdata")
        );
        let placements = super::super::project_capture(&ent, parsed)
            .unwrap()
            .unwrap();
        assert_eq!(placements.placements.len(), 1);
        assert_eq!(placements.placements[0].glb_hash.as_deref(), Some("hscene"));
        assert_eq!(
            placements.files_read.get("data.json").map(String::as_str),
            Some("hdata")
        );
    }
}
