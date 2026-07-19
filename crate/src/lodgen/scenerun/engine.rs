use super::driver::{self, EngineSession, SCENE_THREAD_STACK};
use super::{initial_state_parts, CaptureOutcome, ReadFileFn, SceneEngine, SceneJob};
use anyhow::{anyhow, bail, Result};
use rquickjs::{
    Array, ArrayBuffer, CatchResultExt, CaughtError, Context, Ctx, Error, Exception, Function,
    Object, Runtime, TypedArray,
};
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub struct QuickJsEngine;

impl SceneEngine for QuickJsEngine {
    fn run_capture(&self, job: SceneJob) -> Result<CaptureOutcome> {
        // scene JS recursion must not ride the caller's (possibly small) stack
        let worker = std::thread::Builder::new()
            .name("abgen-scenerun".into())
            .stack_size(SCENE_THREAD_STACK)
            .spawn(move || run_on_thread(job))
            .map_err(|e| anyhow!("spawn scene runtime thread: {e}"))?;
        worker
            .join()
            .map_err(|_| anyhow!("scene runtime thread panicked"))?
    }
}

fn run_on_thread(job: SceneJob) -> Result<CaptureOutcome> {
    let SceneJob {
        code,
        main_crdt,
        read_file,
        limits,
    } = job;
    let runtime = Runtime::new().map_err(|e| anyhow!("quickjs runtime: {e}"))?;
    runtime.set_memory_limit(limits.memory_bytes);
    runtime.set_max_stack_size(limits.stack_bytes);
    let expired = Arc::new(AtomicBool::new(false));
    if let Some(budget) = limits.deadline {
        let deadline = Instant::now() + budget;
        let flag = expired.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || {
            let hit = Instant::now() >= deadline;
            if hit {
                flag.store(true, Ordering::Relaxed);
            }
            hit
        })));
    }
    let context = Context::full(&runtime).map_err(|e| anyhow!("quickjs context: {e}"))?;
    let capture = Rc::new(RefCell::new(CaptureOutcome::default()));
    context.with(|ctx| -> Result<()> {
        install_host(&ctx, main_crdt, read_file, capture.clone())
            .catch(&ctx)
            .map_err(|e| anyhow!("install scene host: {e}"))
    })?;
    let mut session = QuickJsSession {
        runtime: &runtime,
        context: &context,
        expired: &expired,
    };
    driver::drive(&mut session, &code)?;
    runtime.set_interrupt_handler(None);
    drop(context);
    drop(runtime);
    Ok(match Rc::try_unwrap(capture) {
        Ok(cell) => cell.into_inner(),
        Err(shared) => shared.borrow().clone(),
    })
}

struct QuickJsSession<'a> {
    runtime: &'a Runtime,
    context: &'a Context,
    expired: &'a AtomicBool,
}

impl EngineSession for QuickJsSession<'_> {
    fn eval(&mut self, code: &str) -> Result<(), String> {
        self.context.with(|ctx| {
            ctx.eval::<(), _>(code)
                .catch(&ctx)
                .map_err(|e| e.to_string())
        })
    }

    fn call_tick(&mut self, kind: &str, dt: f64) -> Result<(), String> {
        self.context.with(|ctx| {
            ctx.globals()
                .get::<_, Function>("__tick")
                .and_then(|f| f.call::<_, ()>((kind, dt)))
                .catch(&ctx)
                .map_err(|e| e.to_string())
        })
    }

    fn call_advance_clock(&mut self, ms: f64) -> Result<(), String> {
        self.context.with(|ctx| {
            ctx.globals()
                .get::<_, Function>("__advanceClock")
                .and_then(|f| f.call::<_, ()>((ms,)))
                .catch(&ctx)
                .map_err(|e| e.to_string())
        })
    }

    fn pump(&mut self) -> Result<()> {
        loop {
            match self.runtime.execute_pending_job() {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    let msg =
                        e.0.with(|ctx| CaughtError::from_error(&ctx, Error::Exception).to_string());
                    tracing::trace!("scene job error: {msg}");
                }
            }
        }
        if self.expired.load(Ordering::Relaxed) {
            bail!(
                "scene runtime exceeded the {} deadline",
                crate::lodgen::simplify::SUBPROC_TIMEOUT_ENV
            );
        }
        Ok(())
    }
}

fn install_host<'js>(
    ctx: &Ctx<'js>,
    main_crdt: Option<Vec<u8>>,
    read_file: ReadFileFn,
    capture: Rc<RefCell<CaptureOutcome>>,
) -> rquickjs::Result<()> {
    let parts = initial_state_parts(main_crdt.as_deref());
    // upstream hands Uint8Arrays here; the sdk6 adaption layer reads part.buffer
    let get_state = Function::new(
        ctx.clone(),
        move |c: Ctx<'js>| -> rquickjs::Result<Array<'js>> {
            let arr = Array::new(c.clone())?;
            for (i, part) in parts.iter().enumerate() {
                arr.set(i, TypedArray::<u8>::new_copy(c.clone(), part.as_slice())?)?;
            }
            Ok(arr)
        },
    )?;
    let sink = capture.clone();
    let prefix = main_crdt.clone();
    let send = Function::new(ctx.clone(), move |data: TypedArray<'js, u8>| {
        let mut cap = sink.borrow_mut();
        if let Some(head) = &prefix {
            cap.stream.extend_from_slice(head);
        }
        cap.stream
            .extend_from_slice(data.as_bytes().unwrap_or_default());
        cap.sent = true;
    })?;
    let read = Function::new(
        ctx.clone(),
        move |c: Ctx<'js>, name: String| -> rquickjs::Result<Object<'js>> {
            match read_file(&name) {
                Ok((bytes, hash)) => {
                    let out = Object::new(c.clone())?;
                    out.set("content", ArrayBuffer::new(c, bytes)?)?;
                    out.set("hash", hash)?;
                    Ok(out)
                }
                Err(e) => Err(Exception::throw_message(
                    &c,
                    &format!("readFile {name}: {e}"),
                )),
            }
        },
    )?;
    let log = Function::new(ctx.clone(), |msg: String| {
        tracing::trace!("scene log: {msg}");
    })?;
    let host = Object::new(ctx.clone())?;
    host.set("hasEntities", main_crdt.is_some())?;
    host.set("getStateParts", get_state)?;
    host.set("sendToRenderer", send)?;
    host.set("readFile", read)?;
    host.set("log", log)?;
    ctx.globals().set("__abgen", host)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{crdt, EngineLimits};
    use super::*;
    use std::collections::HashMap;

    pub(crate) const SCENE_HELPERS: &str = r#"
function putMessage(entity, component, timestamp, data) {
  const len = 24 + data.length;
  const buf = new ArrayBuffer(len);
  const v = new DataView(buf);
  v.setUint32(0, len, true);
  v.setUint32(4, 1, true);
  v.setUint32(8, entity, true);
  v.setUint32(12, component, true);
  v.setUint32(16, timestamp, true);
  v.setUint32(20, data.length, true);
  new Uint8Array(buf, 24).set(data);
  return new Uint8Array(buf);
}
function transformData(px, py, pz, sx, sy, sz) {
  const buf = new ArrayBuffer(44);
  const v = new DataView(buf);
  v.setFloat32(0, px, true);
  v.setFloat32(4, py, true);
  v.setFloat32(8, pz, true);
  v.setFloat32(24, 1, true);
  v.setFloat32(28, sx, true);
  v.setFloat32(32, sy, true);
  v.setFloat32(36, sz, true);
  return new Uint8Array(buf);
}
function gltfData(src) {
  const bytes = [0x0a, src.length];
  for (let i = 0; i < src.length; i++) bytes.push(src.charCodeAt(i));
  return new Uint8Array(bytes);
}
function joinParts(parts) {
  let total = 0;
  for (const p of parts) total += p.length;
  const joined = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    joined.set(p, off);
    off += p.length;
  }
  return joined;
}
"#;

    pub(crate) fn limits() -> EngineLimits {
        EngineLimits {
            memory_bytes: 256 << 20,
            stack_bytes: 1 << 20,
            deadline: None,
        }
    }

    pub(crate) fn job(code: String, main_crdt: Option<Vec<u8>>) -> SceneJob {
        SceneJob {
            code,
            main_crdt,
            read_file: Box::new(|name| match name {
                "blob.bin" => Ok((vec![7, 8, 9], "hblob".to_string())),
                _ => anyhow::bail!("no such file {name}"),
            }),
            limits: limits(),
        }
    }

    pub(crate) fn put(entity: u32, component: u32, ts: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((24 + data.len()) as u32).to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&entity.to_le_bytes());
        out.extend_from_slice(&component.to_le_bytes());
        out.extend_from_slice(&ts.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    pub(crate) fn gltf(src: &str) -> Vec<u8> {
        let mut out = vec![0x0a, src.len() as u8];
        out.extend_from_slice(src.as_bytes());
        out
    }

    #[test]
    fn sdk7_scene_runs_91_frames_and_emits_placements() {
        let code = format!(
            "{SCENE_HELPERS}
const engineApi = require('~system/EngineApi');
let frames = 0;
let immediateRan = false;
let startMs = 0;
const failures = [];
module.exports.onStart = async function () {{
  startMs = Date.now();
  const state = await engineApi.crdtGetState();
  if (state.hasEntities) failures.push('hasEntities');
  if (state.data.length !== 4) failures.push('stateParts=' + state.data.length);
  const server = await engineApi.isServer();
  if (!server.isServer) failures.push('isServer');
  const rf = await require('~system/Runtime').readFile({{ fileName: 'blob.bin' }});
  if (new Uint8Array(rf.content).length !== 3) failures.push('readFile');
  if (rf.hash !== 'hblob') failures.push('readFileHash=' + rf.hash);
  try {{
    require('foo');
    failures.push('require');
  }} catch (err) {{
    if (String(err).indexOf('Unknown module foo') === -1) failures.push('requireMsg=' + err);
  }}
  setImmediate(async () => {{
    immediateRan = true;
  }});
}};
module.exports.onUpdate = async function (dt) {{
  frames += 1;
  if (frames === 1) {{
    if (dt !== 0) failures.push('firstDt=' + dt);
    if (!immediateRan) failures.push('immediate');
  }}
  if (frames !== 91) return;
  const elapsed = Date.now() - startMs;
  if (elapsed < 2990 || elapsed > 3010) failures.push('clock=' + elapsed);
  if (failures.length) {{
    await engineApi.crdtSendToRenderer({{ data: putMessage(999, 1041, 1, gltfData('failed:' + failures.join('|'))) }});
    return;
  }}
  const joined = joinParts([
    putMessage(600, 1, 1, transformData(8, 0, 4, 2, 2, 2)),
    putMessage(600, 1041, 1, gltfData('models/child.glb'))
  ]);
  await engineApi.crdtSendToRenderer({{ data: joined }});
}};
"
        );
        let outcome = QuickJsEngine.run_capture(job(code, None)).unwrap();
        assert!(outcome.sent);
        let mut content = HashMap::new();
        content.insert("models/child.glb".to_string(), "hchild".to_string());
        let got = crdt::placements_from_crdt(&outcome.stream, &content);
        assert_eq!(got.placements.len(), 1);
        let p = &got.placements[0];
        assert_eq!(p.glb_file.as_deref(), Some("models/child.glb"));
        assert_eq!(p.glb_hash.as_deref(), Some("hchild"));
        assert_eq!(p.position, [8.0, 0.0, 4.0]);
        assert_eq!(p.scale, [2.0, 2.0, 2.0]);
    }

    #[test]
    fn main_crdt_is_prepended_on_every_send() {
        let main = put(700, crdt::GLTF_CONTAINER, 1, &gltf("main.glb"));
        let code = format!(
            "{SCENE_HELPERS}
const engineApi = require('~system/EngineApi');
let frames = 0;
const failures = [];
module.exports.onStart = async function () {{
  const state = await engineApi.crdtGetState();
  if (!state.hasEntities) failures.push('hasEntities');
  if (state.data.length !== 5) failures.push('stateParts=' + state.data.length);
}};
module.exports.onUpdate = async function (_dt) {{
  frames += 1;
  if (frames > 2) return;
  if (failures.length) {{
    await engineApi.crdtSendToRenderer({{ data: putMessage(999, 1041, 1, gltfData('failed:' + failures.join('|'))) }});
    return;
  }}
  await engineApi.crdtSendToRenderer({{ data: new Uint8Array(0) }});
}};
"
        );
        let outcome = QuickJsEngine
            .run_capture(job(code, Some(main.clone())))
            .unwrap();
        assert!(outcome.sent);
        assert_eq!(outcome.stream.len(), 2 * main.len());
        assert_eq!(&outcome.stream[..main.len()], &main[..]);
        let content = HashMap::new();
        let got = crdt::placements_from_crdt(&outcome.stream, &content);
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.placements[0].glb_file.as_deref(), Some("main.glb"));
    }

    #[test]
    fn silent_scene_reports_nothing_sent() {
        let outcome = QuickJsEngine
            .run_capture(job(
                "module.exports.onUpdate = async () => {};".into(),
                None,
            ))
            .unwrap();
        assert!(!outcome.sent);
        assert!(outcome.stream.is_empty());
    }

    #[test]
    fn eval_throw_still_completes() {
        let outcome = QuickJsEngine
            .run_capture(job("throw new Error('boom');".into(), None))
            .unwrap();
        assert!(!outcome.sent);
        assert!(outcome.stream.is_empty());
    }

    #[test]
    fn deadline_interrupt_is_a_hard_error() {
        for code in [
            "while (true) {}",
            "module.exports.onStart = async function () { for (;;) {} };",
        ] {
            let mut j = job(code.into(), None);
            j.limits.deadline = Some(std::time::Duration::from_millis(100));
            let err = QuickJsEngine.run_capture(j).unwrap_err();
            assert!(
                err.to_string()
                    .contains(crate::lodgen::simplify::SUBPROC_TIMEOUT_ENV),
                "{code}: {err}"
            );
        }
    }

    pub(crate) const SYNTHETIC_GAME: &str = include_str!("../testdata/synthetic-game.js");

    #[test]
    fn synthetic_game_fixture_places_one_glb() {
        let outcome = QuickJsEngine
            .run_capture(job(SYNTHETIC_GAME.into(), None))
            .unwrap();
        assert!(outcome.sent);
        let mut content = HashMap::new();
        content.insert("Model.GLB".to_string(), "hmodel".to_string());
        let got = crdt::placements_from_crdt(&outcome.stream, &content);
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.skipped_mesh_renderer, 0);
        assert_eq!(got.unresolved_src, 0);
        let p = &got.placements[0];
        assert_eq!(p.glb_hash.as_deref(), Some("hmodel"));
        assert_eq!(p.glb_file.as_deref(), Some("model.glb"));
        assert_eq!(p.position, [8.0, 1.0, 4.0]);
        assert_eq!(p.rotation, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(p.scale, [2.0, 2.0, 2.0]);
    }

    #[test]
    fn synthetic_game_placements_json_is_deterministic() {
        let mut content = HashMap::new();
        content.insert("model.glb".to_string(), "hmodel".to_string());
        let mut runs = Vec::new();
        for _ in 0..2 {
            let outcome = QuickJsEngine
                .run_capture(job(SYNTHETIC_GAME.into(), None))
                .unwrap();
            let got = crdt::placements_from_crdt(&outcome.stream, &content);
            runs.push(serde_json::to_string_pretty(&got.placements).unwrap());
        }
        assert_eq!(runs[0], runs[1]);
        assert!(runs[0].contains("hmodel"));
    }

    #[test]
    fn immediates_queued_in_start_send_before_the_first_update() {
        let code = format!(
            "{SCENE_HELPERS}
const engineApi = require('~system/EngineApi');
let sentUpdate = false;
module.exports.onStart = async function () {{
  setImmediate(async () => {{
    await engineApi.crdtSendToRenderer({{ data: putMessage(601, 1041, 1, gltfData('immediate.glb')) }});
  }});
}};
module.exports.onUpdate = async function (_dt) {{
  if (sentUpdate) return;
  sentUpdate = true;
  await engineApi.crdtSendToRenderer({{ data: putMessage(602, 1041, 1, gltfData('update.glb')) }});
}};
"
        );
        let outcome = QuickJsEngine.run_capture(job(code, None)).unwrap();
        let mut want = put(601, crdt::GLTF_CONTAINER, 1, &gltf("immediate.glb"));
        want.extend_from_slice(&put(602, crdt::GLTF_CONTAINER, 1, &gltf("update.glb")));
        assert_eq!(outcome.stream, want);
    }

    #[test]
    fn update_throws_keep_the_start_placements() {
        let code = format!(
            "{SCENE_HELPERS}
const engineApi = require('~system/EngineApi');
module.exports.onStart = async function () {{
  await engineApi.crdtSendToRenderer({{ data: putMessage(512, 1041, 1, gltfData('start.glb')) }});
}};
module.exports.onUpdate = async function (_dt) {{
  throw new Error('boom');
}};
"
        );
        let outcome = QuickJsEngine.run_capture(job(code, None)).unwrap();
        assert!(outcome.sent);
        let got = crdt::placements_from_crdt(&outcome.stream, &HashMap::new());
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.placements[0].glb_file.as_deref(), Some("start.glb"));
    }

    #[test]
    fn get_state_echo_reproduces_the_synthetic_initial_state() {
        let code = "
const engineApi = require('~system/EngineApi');
module.exports.onStart = async function () {
  const state = await engineApi.crdtGetState();
  for (const part of state.data) {
    await engineApi.crdtSendToRenderer({ data: part });
  }
};
module.exports.onUpdate = async function (_dt) {};
"
        .to_string();
        let outcome = QuickJsEngine.run_capture(job(code, None)).unwrap();
        assert_eq!(outcome.stream, crdt::synthetic_initial_state(None));
        let got = crdt::placements_from_crdt(&outcome.stream, &HashMap::new());
        assert!(got.placements.is_empty());
    }

    #[test]
    fn allocation_bomb_errors_without_killing_the_process() {
        let code = format!(
            "{SCENE_HELPERS}
const engineApi = require('~system/EngineApi');
module.exports.onStart = async function () {{
  await engineApi.crdtSendToRenderer({{ data: putMessage(512, 1041, 1, gltfData('before.glb')) }});
  const hog = [];
  for (;;) hog.push(new Uint8Array(1 << 20).fill(1));
}};
module.exports.onUpdate = async function (_dt) {{}};
"
        );
        let mut j = job(code, None);
        j.limits.memory_bytes = 32 << 20;
        let outcome = QuickJsEngine.run_capture(j).unwrap();
        assert!(outcome.sent);
        let got = crdt::placements_from_crdt(&outcome.stream, &HashMap::new());
        assert_eq!(got.placements.len(), 1);
        assert_eq!(got.placements[0].glb_file.as_deref(), Some("before.glb"));
    }

    #[test]
    fn virtual_clock_advances_three_seconds_over_the_run() {
        let code = format!(
            "{SCENE_HELPERS}
const engineApi = require('~system/EngineApi');
let startMs = 0;
let frames = 0;
module.exports.onStart = async function () {{
  startMs = Date.now();
}};
module.exports.onUpdate = async function (_dt) {{
  frames += 1;
  if (frames !== 91) return;
  await engineApi.crdtSendToRenderer({{ data: putMessage(512, 1041, 1, gltfData('delta-' + (Date.now() - startMs) + '.glb')) }});
}};
"
        );
        let outcome = QuickJsEngine.run_capture(job(code, None)).unwrap();
        let got = crdt::placements_from_crdt(&outcome.stream, &HashMap::new());
        assert_eq!(got.placements.len(), 1);
        let file = got.placements[0].glb_file.clone().unwrap();
        let ms: i64 = file
            .trim_start_matches("delta-")
            .trim_end_matches(".glb")
            .parse()
            .unwrap();
        assert!((2990..=3010).contains(&ms), "{ms}");
    }
}
