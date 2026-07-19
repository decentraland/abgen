use super::driver::{self, EngineSession, SCENE_THREAD_STACK};
use super::{initial_state_parts, CaptureOutcome, ReadFileFn, SceneEngine, SceneJob};
use anyhow::{anyhow, bail, Result};
use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct V8Engine;

impl SceneEngine for V8Engine {
    fn run_capture(&self, job: SceneJob) -> Result<CaptureOutcome> {
        let worker = std::thread::Builder::new()
            .name("abgen-scenerun-v8".into())
            .stack_size(SCENE_THREAD_STACK)
            .spawn(move || run_on_thread(job))
            .map_err(|e| anyhow!("spawn scene runtime thread: {e}"))?;
        worker
            .join()
            .map_err(|_| anyhow!("scene runtime thread panicked"))?
    }
}

static V8_INIT: std::sync::Once = std::sync::Once::new();

fn ensure_v8_initialized() {
    V8_INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });
}

#[derive(Default)]
struct Watchdog {
    expired: AtomicBool,
    oom: AtomicBool,
    stopped: AtomicBool,
    isolate: Mutex<Option<v8::IsolateHandle>>,
}

impl Watchdog {
    fn terminate(&self, flag: &AtomicBool) {
        flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.isolate.lock().unwrap().as_ref() {
            h.terminate_execution();
        }
    }
}

fn watchdog_loop(wd: Arc<Watchdog>, deadline: Instant) {
    while !wd.stopped.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            wd.terminate(&wd.expired);
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

extern "C" fn near_heap_limit_cb(
    data: *mut c_void,
    current_heap_limit: usize,
    _initial_heap_limit: usize,
) -> usize {
    if !data.is_null() {
        let wd = unsafe { &*(data as *const Watchdog) };
        wd.terminate(&wd.oom);
    }
    current_heap_limit + 32 * 1024 * 1024
}

struct Host {
    parts: Vec<Vec<u8>>,
    prefix: Option<Vec<u8>>,
    read_file: ReadFileFn,
    capture: CaptureOutcome,
}

fn host_with<R>(isolate: &mut v8::Isolate, f: impl FnOnce(&RefCell<Host>) -> R) -> R {
    let ptr = isolate.get_data(0) as *const RefCell<Host>;
    f(unsafe { &*ptr })
}

fn run_on_thread(job: SceneJob) -> Result<CaptureOutcome> {
    ensure_v8_initialized();
    let SceneJob {
        code,
        main_crdt,
        read_file,
        limits,
    } = job;
    let watchdog = Arc::new(Watchdog::default());
    let isolate =
        &mut v8::Isolate::new(v8::CreateParams::default().heap_limits(0, limits.memory_bytes));
    *watchdog.isolate.lock().unwrap() = Some(isolate.thread_safe_handle());
    isolate.add_near_heap_limit_callback(near_heap_limit_cb, Arc::as_ptr(&watchdog) as *mut c_void);
    let host: Box<RefCell<Host>> = Box::new(RefCell::new(Host {
        parts: initial_state_parts(main_crdt.as_deref()),
        prefix: main_crdt,
        read_file,
        capture: CaptureOutcome::default(),
    }));
    isolate.set_data(0, host.as_ref() as *const RefCell<Host> as *mut c_void);
    let wd_thread = limits.deadline.map(|budget| {
        let wd = Arc::clone(&watchdog);
        let deadline = Instant::now() + budget;
        std::thread::spawn(move || watchdog_loop(wd, deadline))
    });

    let has_entities = host.borrow().prefix.is_some();
    let run = {
        v8::scope!(let handle_scope, isolate);
        let context = v8::Context::new(handle_scope, Default::default());
        let ctx_scope = &mut v8::ContextScope::new(handle_scope, context);
        install_host(ctx_scope, has_entities);
        let mut session = V8Session {
            scope: &mut **ctx_scope,
            watchdog: &watchdog,
        };
        driver::drive(&mut session, &code)
    };

    watchdog.stopped.store(true, Ordering::SeqCst);
    *watchdog.isolate.lock().unwrap() = None;
    if let Some(t) = wd_thread {
        let _ = t.join();
    }
    run?;
    let outcome = std::mem::take(&mut host.borrow_mut().capture);
    Ok(outcome)
}

struct V8Session<'a, 's, 'i> {
    scope: &'a mut v8::PinScope<'s, 'i>,
    watchdog: &'a Watchdog,
}

impl EngineSession for V8Session<'_, '_, '_> {
    fn eval(&mut self, code: &str) -> Result<(), String> {
        let scope = &mut *self.scope;
        v8::tc_scope!(let tc, scope);
        let Some(code) = v8::String::new(tc, code) else {
            return Err("scene source too large".into());
        };
        let Some(script) = v8::Script::compile(tc, code, None) else {
            return Err(caught(tc));
        };
        if script.run(tc).is_none() {
            return Err(caught(tc));
        }
        Ok(())
    }

    fn call_tick(&mut self, kind: &str, dt: f64) -> Result<(), String> {
        let scope = &mut *self.scope;
        let args = [jstr(scope, kind).into(), v8::Number::new(scope, dt).into()];
        call_global(scope, "__tick", &args)
    }

    fn call_advance_clock(&mut self, ms: f64) -> Result<(), String> {
        let scope = &mut *self.scope;
        let args = [v8::Number::new(scope, ms).into()];
        call_global(scope, "__advanceClock", &args)
    }

    fn pump(&mut self) -> Result<()> {
        self.scope.perform_microtask_checkpoint();
        if self.watchdog.expired.load(Ordering::SeqCst) {
            bail!(
                "scene runtime exceeded the {} deadline",
                crate::lodgen::simplify::SUBPROC_TIMEOUT_ENV
            );
        }
        if self.watchdog.oom.load(Ordering::SeqCst) {
            bail!("scene runtime exceeded the heap cap");
        }
        Ok(())
    }
}

fn call_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    name: &str,
    args: &[v8::Local<'s, v8::Value>],
) -> Result<(), String> {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let key = jstr(scope, name).into();
    let f = match global.get(scope, key) {
        Some(v) if v.is_function() => v8::Local::<v8::Function>::try_from(v).unwrap(),
        _ => return Err(format!("{name} is not installed")),
    };
    v8::tc_scope!(let tc, scope);
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    if f.call(tc, recv, args).is_none() {
        return Err(caught(tc));
    }
    Ok(())
}

fn caught(tc: &mut v8::PinnedRef<v8::TryCatch<v8::HandleScope>>) -> String {
    tc.exception()
        .and_then(|e| e.to_string(tc))
        .map(|s| s.to_rust_string_lossy(tc))
        .unwrap_or_else(|| "unknown JS error".into())
}

fn jstr<'s>(scope: &mut v8::PinScope<'s, '_>, s: &str) -> v8::Local<'s, v8::String> {
    v8::String::new(scope, s).unwrap()
}

fn set_prop(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    key: &str,
    value: v8::Local<v8::Value>,
) {
    let k = jstr(scope, key).into();
    obj.set(scope, k, value);
}

fn set_fn(
    scope: &mut v8::PinScope,
    obj: v8::Local<v8::Object>,
    name: &str,
    cb: impl v8::MapFnTo<v8::FunctionCallback>,
) {
    let f = v8::Function::new(scope, cb).unwrap();
    let k = jstr(scope, name).into();
    obj.set(scope, k, f.into());
}

fn read_uint8array(val: v8::Local<v8::Value>) -> Option<Vec<u8>> {
    let view = v8::Local::<v8::ArrayBufferView>::try_from(val).ok()?;
    let mut out = vec![0u8; view.byte_length()];
    view.copy_contents(&mut out);
    Some(out)
}

fn make_uint8array<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: &[u8],
) -> v8::Local<'s, v8::Uint8Array> {
    let ab = make_arraybuffer(scope, bytes.to_vec());
    v8::Uint8Array::new(scope, ab, 0, ab.byte_length()).unwrap()
}

fn make_arraybuffer<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    bytes: Vec<u8>,
) -> v8::Local<'s, v8::ArrayBuffer> {
    let store = v8::ArrayBuffer::new_backing_store_from_vec(bytes).make_shared();
    v8::ArrayBuffer::with_backing_store(scope, &store)
}

fn op_get_state_parts(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let parts = host_with(scope, |h| h.borrow().parts.clone());
    let arr = v8::Array::new(scope, parts.len() as i32);
    for (i, p) in parts.iter().enumerate() {
        let ua = make_uint8array(scope, p);
        arr.set_index(scope, i as u32, ua.into());
    }
    rv.set(arr.into());
}

fn op_send_to_renderer(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue,
) {
    let bytes = read_uint8array(args.get(0)).unwrap_or_default();
    host_with(scope, |h| {
        let h = &mut *h.borrow_mut();
        if let Some(head) = &h.prefix {
            h.capture.stream.extend_from_slice(head);
        }
        h.capture.stream.extend_from_slice(&bytes);
        h.capture.sent = true;
    });
}

fn op_read_file(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let name = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    let read = host_with(scope, |h| (h.borrow().read_file)(&name));
    match read {
        Ok((bytes, hash)) => {
            let out = v8::Object::new(scope);
            let ab = make_arraybuffer(scope, bytes);
            set_prop(scope, out, "content", ab.into());
            let hash = jstr(scope, &hash).into();
            set_prop(scope, out, "hash", hash);
            rv.set(out.into());
        }
        Err(e) => {
            let msg = jstr(scope, &format!("readFile {name}: {e}"));
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
        }
    }
}

fn op_log(scope: &mut v8::PinScope, args: v8::FunctionCallbackArguments, _rv: v8::ReturnValue) {
    let msg = args
        .get(0)
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default();
    tracing::trace!("scene log: {msg}");
}

fn install_host(scope: &mut v8::PinScope, has_entities: bool) {
    let context = scope.get_current_context();
    let global = context.global(scope);
    let host = v8::Object::new(scope);
    let has = v8::Boolean::new(scope, has_entities);
    set_prop(scope, host, "hasEntities", has.into());
    set_fn(scope, host, "getStateParts", op_get_state_parts);
    set_fn(scope, host, "sendToRenderer", op_send_to_renderer);
    set_fn(scope, host, "readFile", op_read_file);
    set_fn(scope, host, "log", op_log);
    set_prop(scope, global, "__abgen", host.into());
}

#[cfg(test)]
mod tests {
    use super::super::engine::tests::{gltf, job, put, SCENE_HELPERS};
    use super::super::{crdt, QuickJsEngine};
    use super::*;
    use std::collections::HashMap;

    fn parity_scene() -> String {
        format!(
            "{SCENE_HELPERS}
const engineApi = require('~system/EngineApi');
let frames = 0;
const notes = [];
module.exports.onStart = async function () {{
  notes.push('start@' + Date.now());
  const state = await engineApi.crdtGetState();
  notes.push('parts=' + state.data.length + ',has=' + state.hasEntities);
  const rf = await require('~system/Runtime').readFile({{ fileName: 'blob.bin' }});
  notes.push('rf=' + rf.hash + ':' + new Uint8Array(rf.content).join('.'));
  setImmediate(() => notes.push('imm'));
}};
module.exports.onUpdate = async function (dt) {{
  frames += 1;
  if (frames === 1) notes.push('dt0=' + dt);
  if (frames === 45) notes.push('mid@' + Date.now());
  if (frames !== 91) return;
  notes.push('end@' + Date.now());
  const joined = joinParts([
    putMessage(600, 1, 1, transformData(4, 0, 2, 1, 1, 1)),
    putMessage(601, 1, 2, transformData(1, 2, 3, 2, 2, 2)),
    putMessage(601, 1041, 1, gltfData('models/a.glb')),
    putMessage(700, 1018, 1, new Uint8Array(0)),
    putMessage(800, 1041, 1, gltfData(notes.join('|')))
  ]);
  await engineApi.crdtSendToRenderer({{ data: joined }});
}};
"
        )
    }

    #[test]
    fn v8_stream_is_byte_identical_to_quickjs() {
        let main = put(900, crdt::GLTF_CONTAINER, 1, &gltf("main.glb"));
        let cases = [(parity_scene(), None), (parity_scene(), Some(main))];
        for (code, main_crdt) in cases {
            let quick = QuickJsEngine
                .run_capture(job(code.clone(), main_crdt.clone()))
                .unwrap();
            let v8 = V8Engine.run_capture(job(code, main_crdt.clone())).unwrap();
            assert_eq!(quick.sent, v8.sent);
            assert_eq!(quick.stream, v8.stream, "main_crdt={}", main_crdt.is_some());
            let mut content = HashMap::new();
            content.insert("models/a.glb".to_string(), "ha".to_string());
            let q = crdt::placements_from_crdt(&quick.stream, &content);
            let v = crdt::placements_from_crdt(&v8.stream, &content);
            assert_eq!(q, v);
            assert_eq!(q.skipped_mesh_renderer, 1);
            assert_eq!(q.unresolved_src, 1 + usize::from(main_crdt.is_some()));
        }
    }

    #[test]
    fn v8_silent_scene_and_eval_throw_match_quickjs() {
        let outcome = V8Engine
            .run_capture(job(
                "module.exports.onUpdate = async () => {};".into(),
                None,
            ))
            .unwrap();
        assert!(!outcome.sent);
        assert!(outcome.stream.is_empty());
        let outcome = V8Engine
            .run_capture(job("throw new Error('boom');".into(), None))
            .unwrap();
        assert!(!outcome.sent);
        assert!(outcome.stream.is_empty());
    }

    #[test]
    fn v8_deadline_interrupt_is_a_hard_error() {
        let mut j = job("while (true) {}".into(), None);
        j.limits.deadline = Some(std::time::Duration::from_millis(100));
        let err = V8Engine.run_capture(j).unwrap_err();
        assert!(err
            .to_string()
            .contains(crate::lodgen::simplify::SUBPROC_TIMEOUT_ENV));
    }

    #[test]
    #[ignore = "network: resolves the wi1 golden coords on peer.decentraland.org"]
    fn cross_engine_parity_on_wi1_goldens() {
        let cases = [
            (
                "-150,150",
                include_str!("../testdata/golden_sdk7_-150_150.placements.json"),
            ),
            (
                "100,100",
                include_str!("../testdata/golden_sdk6_100_100.placements.json"),
            ),
        ];
        let client = crate::catalyst::CatalystClient::new("https://peer.decentraland.org/content");
        for (coords, golden) in cases {
            let ent = client.resolve_scene(coords).unwrap();
            let quick = super::super::run_scene_with(&QuickJsEngine, &client, &ent)
                .unwrap()
                .unwrap_or_default();
            let v8 = super::super::run_scene_with(&V8Engine, &client, &ent)
                .unwrap()
                .unwrap_or_default();
            assert_eq!(quick, v8, "{coords}");
            let got = serde_json::to_string_pretty(&v8.placements).unwrap();
            assert_eq!(got.trim(), golden.trim(), "{coords}");
        }
    }
}
