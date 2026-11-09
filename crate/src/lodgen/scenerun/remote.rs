//! Scene execution in a separate AWS Lambda function.
//!
//! Some runtimes can host the scene child of [`super::sandbox`] but not protect the parent
//! from it: AWS Lambda refuses both seccomp and `PR_SET_DUMPABLE`, so the self-test refuses
//! to run scenes in a process that holds the converter's credentials. There scenes run in
//! a function of their own, a separate execution environment whose role may only write its
//! own logs:
//!
//! - **caller** (the converter): with [`FUNCTION_ENV`] set, [`super::run_scene_placements`]
//!   sends the entity and its content server(s) to that function with a synchronous
//!   `Invoke` and projects the returned renderer state exactly as a local run would. The
//!   wait is capped by [`TIMEOUT_ENV`] and by the time left in the caller's own
//!   invocation. The response is untrusted: only the stream, the evaluation error and the
//!   files the scene read are taken from it, a file is kept only when the entity's own
//!   content lists it under the same hash, and both sides must fold the stream the same
//!   way ([`super::crdt::projection_id`]). Every invoke names the entity as its Lambda tenant
//!   (`X-Amz-Tenant-Id`), so on a function created with tenant isolation no execution
//!   environment serves two entities.
//! - **function** (`abgen-lambda` started with `ABGEN_LAMBDA_MODE=scenerun`): [`serve`]
//!   fetches the scene's code and files from the content server and runs the scene in the
//!   sandbox child ([`super::sandbox`]), under a deadline even when the request names
//!   none. The child gets an empty environment, no new processes and no new file
//!   descriptors (so no files or sockets), which the self-test proves before the first
//!   scene. Where the runtime also lets the child reach the runner's memory (AWS Lambda),
//!   scenes run only with `ABGEN_SANDBOX_PARENT_MEMORY=accept`: an escaped scene can then take
//!   over the runner's execution environment, and tenant isolation per entity is what keeps
//!   it from answering for any other entity. Exiting after each answer is hygiene on top.
//!
//! Lambda caps a synchronous response at 6 MB. The function sends the folded state rather
//! than every frame, and fails with a clear error when even that does not fit.

use super::{capture_scene_with, default_engine, CaptureOutcome, EngineLimits, SceneCapture};
use crate::catalyst::{CatalystClient, Scene};
use crate::lambda_invoke::{invocation_remaining, LambdaInvoker};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

/// Name of the function that runs scenes. Unset: scenes run in a local child process.
pub const FUNCTION_ENV: &str = "ABGEN_SCENERUN_FUNCTION";
/// Overrides the Lambda endpoint (tests, local stacks; plain http only for loopback).
pub const ENDPOINT_ENV: &str = "ABGEN_SCENERUN_ENDPOINT";
/// Seconds to wait for one scene, including the function's cold start (default 300). The
/// scene function's own timeout must not be longer, or it keeps running after the caller
/// has given up.
pub const TIMEOUT_ENV: &str = "ABGEN_SCENERUN_TIMEOUT_S";

const DEFAULT_TIMEOUT_S: u64 = 300;
/// Marks a scenerun request or response and versions its shape.
const PROTOCOL_KEY: &str = "abgenScenerun";
const PROTOCOL_VERSION: u64 = 2;
const MAX_EVAL_ERROR_CHARS: usize = 2000;

/// Time the caller keeps for itself after the scene (projection, LOD build, upload).
const CALLER_RESERVE: Duration = Duration::from_secs(30);
/// Below this, the caller does not start a call at all.
const MIN_CALL_BUDGET: Duration = Duration::from_secs(10);
/// Part of the call budget that is not scene time: cold start, fetching the scene, the
/// sandbox's kill grace and the answer.
const CALL_OVERHEAD: Duration = Duration::from_secs(60);
const MIN_SCENE_DEADLINE: Duration = Duration::from_secs(5);
/// The function's scene deadline when neither the request nor
/// `ABGEN_LOD_SUBPROC_TIMEOUT_S` sets a shorter one.
const DEFAULT_SCENE_DEADLINE: Duration = Duration::from_secs(180);
/// Time the function keeps after the scene deadline: the sandbox kills a child that
/// ignores the deadline 30 s after it, then the answer still has to go out. A scene
/// function with a timeout under about a minute therefore has no time left for scenes.
const RUNNER_RESERVE: Duration = Duration::from_secs(45);
/// What the function may answer with: Lambda's 6 MB less some room.
const MAX_RESPONSE_BYTES: usize = 6_000_000;

pub struct RemoteRunner {
    invoker: LambdaInvoker,
    timeout: Duration,
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

/// [`TIMEOUT_ENV`]: unset or blank is the default, anything else a positive integer.
fn parse_timeout(value: Option<&str>) -> Result<Duration> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(Duration::from_secs(DEFAULT_TIMEOUT_S)),
        Some(v) => v
            .parse::<u64>()
            .ok()
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .ok_or_else(|| anyhow!("{TIMEOUT_ENV}={v:?} is not a positive integer")),
    }
}

/// How long one call may take: the configured timeout, capped by what is left of the
/// caller's own invocation after [`CALLER_RESERVE`].
fn call_budget(timeout: Duration, invocation_left: Option<Duration>) -> Result<Duration> {
    let budget = match invocation_left {
        Some(left) => timeout.min(left.saturating_sub(CALLER_RESERVE)),
        None => timeout,
    };
    if budget < MIN_CALL_BUDGET {
        bail!(
            "only {}s left for the scene in this invocation, not starting it",
            budget.as_secs()
        );
    }
    Ok(budget)
}

/// The scene deadline the caller asks for: its call budget less [`CALL_OVERHEAD`].
fn requested_scene_deadline(budget: Duration) -> Duration {
    budget.saturating_sub(CALL_OVERHEAD).max(MIN_SCENE_DEADLINE)
}

/// The deadline the function runs a scene under: the shortest of what the request asks,
/// the configured one (or [`DEFAULT_SCENE_DEADLINE`]) and what its own invocation still
/// allows after [`RUNNER_RESERVE`]. Never unbounded.
fn scene_deadline(
    requested: Option<Duration>,
    configured: Option<Duration>,
    invocation_left: Option<Duration>,
) -> Result<Duration> {
    let mut deadline = configured.unwrap_or(DEFAULT_SCENE_DEADLINE);
    if let Some(requested) = requested {
        deadline = deadline.min(requested);
    }
    if let Some(left) = invocation_left {
        deadline = deadline.min(left.saturating_sub(RUNNER_RESERVE));
    }
    if deadline < Duration::from_secs(1) {
        bail!(
            "no time left in this invocation to run the scene (the function keeps {}s after \
             the scene deadline; give it a timeout of at least a minute)",
            RUNNER_RESERVE.as_secs()
        );
    }
    Ok(deadline)
}

impl RemoteRunner {
    fn from_env(function: &str) -> Result<RemoteRunner> {
        let region = std::env::var("AWS_REGION")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "us-east-1".to_string());
        let endpoint = std::env::var(ENDPOINT_ENV).ok();
        let timeout = parse_timeout(std::env::var(TIMEOUT_ENV).ok().as_deref())?;
        let invoker = LambdaInvoker::new(function, &region, endpoint.as_deref())?;
        tracing::info!(
            function,
            timeout_s = timeout.as_secs(),
            "scenes run in a separate function"
        );
        Ok(RemoteRunner { invoker, timeout })
    }

    /// Runs `ent` in the function and returns what it produced.
    pub fn capture(&self, client: &CatalystClient, ent: &Scene) -> Result<SceneCapture> {
        let budget = call_budget(self.timeout, invocation_remaining())
            .with_context(|| format!("run scene {}", ent.entity_id))?;
        let payload = request(client, ent, requested_scene_deadline(budget));
        // One tenant per entity: the scene function is created with tenant isolation, so an
        // execution environment that ran this entity never runs another one.
        let response = self
            .invoker
            .invoke(&payload, budget, Some(&ent.entity_id))
            .with_context(|| {
                format!("run scene {} in {}", ent.entity_id, self.invoker.function())
            })?;
        parse_response(ent, &response)
    }
}

/// The invoke payload: the entity as the content server lists it, where to fetch it, how
/// long the scene may run and how the caller folds the result.
pub fn request(
    client: &CatalystClient,
    ent: &Scene,
    scene_deadline: Duration,
) -> serde_json::Value {
    let content: Vec<serde_json::Value> = ent
        .content
        .iter()
        .map(|c| serde_json::json!({"file": c.file, "hash": c.hash}))
        .collect();
    serde_json::json!({
        PROTOCOL_KEY: PROTOCOL_VERSION,
        "projection": super::crdt::projection_id(),
        "sceneDeadlineMs": u64::try_from(scene_deadline.as_millis()).unwrap_or(u64::MAX),
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

/// Function side: runs the requested scene in the sandbox child and returns the response
/// payload. `check_server` vets every content server in the request before anything is
/// fetched; `invocation_left` is the time the function's own invocation still has
/// ([`crate::lambda_invoke::invocation_remaining`] on Lambda).
pub fn serve(
    event: &serde_json::Value,
    check_server: &dyn Fn(&str) -> Result<()>,
    invocation_left: Option<Duration>,
) -> Result<serde_json::Value> {
    let version = event.get(PROTOCOL_KEY).and_then(|v| v.as_u64());
    if version != Some(PROTOCOL_VERSION) {
        bail!("unsupported {PROTOCOL_KEY} version {version:?}");
    }
    check_projection(event.get("projection"), "caller")?;
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
    let requested = event
        .get("sceneDeadlineMs")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);
    let limits = EngineLimits {
        deadline: Some(scene_deadline(
            requested,
            crate::lodgen::simplify::subproc_deadline(),
            invocation_left,
        )?),
        ..EngineLimits::default()
    };
    let mut capture = capture_scene_with(&default_engine(), &client, &ent, limits)?;
    // Every frame the scene sent, with `main.crdt` ahead of each, folds to far less;
    // only the folded state counts against the response limit.
    capture.outcome.stream = super::crdt::compact_stream(&capture.outcome.stream);
    sized_response(&ent, &capture)
}

/// [`response`], or an error naming the scene when it would not fit a Lambda response.
fn sized_response(ent: &Scene, capture: &SceneCapture) -> Result<serde_json::Value> {
    let value = response(capture);
    let size = serde_json::to_vec(&value)?.len();
    if size > MAX_RESPONSE_BYTES {
        bail!(
            "scene {} folds to a {size}-byte response, over the {MAX_RESPONSE_BYTES} bytes a \
             Lambda response may carry",
            ent.entity_id
        );
    }
    Ok(value)
}

/// Both sides must fold the stream the same way; `who` names the other side.
fn check_projection(theirs: Option<&serde_json::Value>, who: &str) -> Result<()> {
    let ours = super::crdt::projection_id();
    match theirs.and_then(|v| v.as_str()) {
        Some(theirs) if theirs == ours => Ok(()),
        theirs => bail!(
            "{who} folds renderer state as {theirs:?}, this build as {ours:?}: run the same \
             abgen version on both functions"
        ),
    }
}

fn response(capture: &SceneCapture) -> serde_json::Value {
    serde_json::json!({
        PROTOCOL_KEY: PROTOCOL_VERSION,
        "projection": super::crdt::projection_id(),
        "sent": capture.outcome.sent,
        "stream": crate::gltf::base64_encode(&capture.outcome.stream),
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
    check_projection(value.get("projection"), "scene runner")?;
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

#[cfg(test)]
mod tests {
    use super::super::{capture_scene, QuickJsEngine};
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
    fn request_carries_the_entity_and_servers() {
        let client = CatalystClient::new("https://peer.example.org/content/")
            .with_fallback_base("https://worlds.example.org".into());
        let req = request(&client, &scene(), Duration::from_secs(90));
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
            &serde_json::json!({
                PROTOCOL_KEY: PROTOCOL_VERSION,
                "projection": super::super::crdt::projection_id(),
                "stream": "!!not base64!!",
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("base64"), "{err}");
    }

    #[test]
    fn serve_checks_every_server_before_fetching() {
        let client = CatalystClient::new("https://peer.example.org/content")
            .with_fallback_base("http://169.254.169.254".into());
        let req = request(&client, &scene(), Duration::from_secs(90));
        let err = serve(
            &req,
            &|s: &str| {
                if s.starts_with("https://") {
                    Ok(())
                } else {
                    bail!("refused {s}")
                }
            },
            None,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("refused http://169.254.169.254"),
            "{err}"
        );
        let err = serve(
            &serde_json::json!({PROTOCOL_KEY: 99}),
            &|_: &str| Ok(()),
            None,
        )
        .unwrap_err();
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

    #[test]
    fn request_carries_deadline_and_projection() {
        let client = CatalystClient::new("https://peer.example.org/content");
        let req = request(&client, &scene(), Duration::from_millis(1500));
        assert_eq!(req["sceneDeadlineMs"], 1500);
        assert_eq!(req["projection"], super::super::crdt::projection_id());
    }

    #[test]
    fn projection_mismatches_are_refused_both_ways() {
        let client = CatalystClient::new("https://peer.example.org/content");
        let mut req = request(&client, &scene(), Duration::from_secs(90));
        req["projection"] = serde_json::json!("lww0:1");
        let err = serve(&req, &|_: &str| Ok(()), None).unwrap_err();
        assert!(err.to_string().contains("lww0:1"), "{err}");

        let mut value = response(&SceneCapture::default());
        value["projection"] = serde_json::json!("lww9:1,2");
        let err = parse_response(&scene(), &value).unwrap_err();
        assert!(err.to_string().contains("lww9:1,2"), "{err}");
        value.as_object_mut().unwrap().remove("projection");
        assert!(parse_response(&scene(), &value).is_err());
    }

    #[test]
    fn timeout_setting_is_a_positive_integer() {
        assert_eq!(parse_timeout(None).unwrap(), Duration::from_secs(300));
        assert_eq!(parse_timeout(Some("  ")).unwrap(), Duration::from_secs(300));
        assert_eq!(
            parse_timeout(Some(" 45 ")).unwrap(),
            Duration::from_secs(45)
        );
        for bad in ["0", "-3", "1.5", "5m"] {
            let err = parse_timeout(Some(bad)).unwrap_err();
            assert!(err.to_string().contains(TIMEOUT_ENV), "{bad}: {err}");
        }
    }

    #[test]
    fn the_call_fits_the_callers_invocation() {
        let s = Duration::from_secs;
        assert_eq!(call_budget(s(270), None).unwrap(), s(270));
        assert_eq!(call_budget(s(270), Some(s(900))).unwrap(), s(270));
        assert_eq!(call_budget(s(270), Some(s(100))).unwrap(), s(70));
        let err = call_budget(s(270), Some(s(35))).unwrap_err();
        assert!(err.to_string().contains("not starting"), "{err}");
        assert_eq!(requested_scene_deadline(s(270)), s(210));
        assert_eq!(requested_scene_deadline(s(20)), MIN_SCENE_DEADLINE);
    }

    #[test]
    fn the_scene_always_runs_under_a_deadline() {
        let s = Duration::from_secs;
        assert_eq!(
            scene_deadline(None, None, None).unwrap(),
            DEFAULT_SCENE_DEADLINE
        );
        assert_eq!(
            scene_deadline(Some(s(210)), Some(s(180)), None).unwrap(),
            s(180)
        );
        assert_eq!(
            scene_deadline(Some(s(30)), Some(s(180)), None).unwrap(),
            s(30)
        );
        assert_eq!(
            scene_deadline(Some(s(210)), Some(s(180)), Some(s(100))).unwrap(),
            s(55)
        );
        assert!(scene_deadline(Some(s(210)), None, Some(s(45))).is_err());
    }

    #[test]
    fn an_oversized_response_is_a_clear_error() {
        // Bytes that base64 cannot shrink: past the limit once encoded.
        let stream: Vec<u8> = (0..MAX_RESPONSE_BYTES)
            .map(|i| (i * 131 % 251) as u8)
            .collect();
        let capture = SceneCapture {
            outcome: CaptureOutcome {
                sent: true,
                stream,
                eval_error: None,
            },
            files_read: BTreeMap::new(),
        };
        let err = sized_response(&scene(), &capture).unwrap_err();
        assert!(err.to_string().contains("bafkScene"), "{err}");
        assert!(err.to_string().contains("Lambda response"), "{err}");
        let small = SceneCapture::default();
        assert!(sized_response(&scene(), &small).is_ok());
    }
}
