// Worker pool: one mode-1 scan finds the entity hash and per-glb dep lists,
// then per-glb mode-2 jobs fan out over N workers (mode-3 LOD bake last, on
// its own, so its memory spike never overlaps the encode jobs). A trapped
// worker is terminated and respawned from the cached Module — its instance
// memory is poisoned — so one malformed file fails alone.

const MODEL_RE = /\.(glb|gltf)$/;
const ISS_RE = /_initialscenestate\.json$/;

const sha256 = async (data) =>
  [...new Uint8Array(await crypto.subtle.digest('SHA-256', data))]
    .map((b) => b.toString(16).padStart(2, '0')).join('');

// WebGPU bridge: one GPU worker (bit-exact qualified at init) services all
// convert workers over a SharedArrayBuffer. Needs crossOriginIsolated (COOP/
// COEP — server.py sends them) + WebGPU + Atomics.waitAsync; anything missing
// or failing degrades to the CPU-SIMD path with a note.
async function initGpu(opts, cb) {
  if (opts.gpu === false) return null;
  const off = (why) => {
    cb({ type: 'event', data: { ev: 'note', msg: `WebGPU encode: off (${why}) — CPU-SIMD path active` } });
    return null;
  };
  if (typeof SharedArrayBuffer === 'undefined'
      || (typeof crossOriginIsolated !== 'undefined' && !crossOriginIsolated)) {
    return off('needs crossOriginIsolated; serve with COOP/COEP headers');
  }
  if (typeof navigator === 'undefined' || !navigator.gpu) return off('no WebGPU in this browser');
  if (typeof Atomics.waitAsync !== 'function') return off('no Atomics.waitAsync');
  const sab = new SharedArrayBuffer(16 + (64 << 20) + 4096);
  const spawnGpu = opts.spawnGpu
    || (() => new Worker(new URL('./gpu-worker.js', import.meta.url), { type: 'module' }));
  const w = spawnGpu();
  const res = await new Promise((resolve) => {
    const t = setTimeout(() => resolve({ ok: false, err: 'gpu worker init timeout' }), 30000);
    w.onmessage = (e) => { clearTimeout(t); resolve(e.data); };
    w.onerror = (e) => { clearTimeout(t); resolve({ ok: false, err: String((e && (e.message || e.type)) || e) }); };
    w.postMessage({ cmd: 'init', sab });
  });
  if (!res.ok) {
    try { w.terminate(); } catch (_) {}
    return off(res.err);
  }
  cb({ type: 'event', data: { ev: 'note', msg: `WebGPU encode: ON — ${res.adapter} (bit-exact qualified)` } });
  return { sab, w };
}

export async function runConvert(opts, cb) {
  const module = await opts.module;
  const gpu = await initGpu(opts, cb);
  const spawn = opts.spawn || (() => new Worker(new URL('./worker.js', import.meta.url)));
  const files = opts.files;
  const table = await Promise.all(files.map(async (f) => [f.name, await sha256(f.data)]));
  const byName = new Map(files.map((f) => [f.name, f]));
  const copyOf = (f) => ({ name: f.name, data: f.data.slice(0) });
  const base = { platform: opts.platform, entityType: opts.entityType, magenta: opts.magenta };

  let jobSeq = 0;
  let readySent = false;
  const slots = [];

  const pool = {
    onMsg(slot, m) {
      if (m.type === 'ready') {
        if (!readySent) { readySent = true; cb({ type: 'ready' }); }
        return;
      }
      if (m.type === 'trap') { this.onCrash(slot, m.msg); return; }
      const cur = slot.current;
      if (m.type === 'event') {
        if (cur && cur.sink) cur.sink(m.data);
        cb({ type: 'event', worker: slot.id, data: m.data });
      } else if (m.type === 'output') {
        cb({ type: 'output', worker: slot.id, name: m.name, size: m.size, data: m.data });
      } else if (m.type === 'fatal') {
        cb({ type: 'fatal', worker: slot.id, msg: m.msg });
      } else if (m.type === 'done') {
        if (cur) { slot.current = null; cur.resolve({ code: m.code }); }
      }
    },
    onCrash(slot, msg) {
      const cur = slot.current;
      if (!cur) return;
      slot.current = null;
      try { slot.w.terminate(); } catch (_) {}
      slot.attach();
      cb({
        type: 'event', worker: slot.id,
        data: { ev: 'file-error', file: cur.label, error: `wasm trap: ${msg}` },
      });
      cur.resolve({ code: -2, trapped: true });
    },
  };

  const mkSlot = (id) => {
    const slot = { id, w: null, current: null };
    slot.attach = () => {
      const w = spawn();
      w.onmessage = (e) => pool.onMsg(slot, e.data);
      w.onerror = (e) => pool.onCrash(slot, String((e && (e.message || e.type)) || e));
      w.postMessage({ cmd: 'init', module, sab: gpu && gpu.sab });
      slot.w = w;
    };
    slot.attach();
    slots.push(slot);
    return slot;
  };

  const run = (slot, label, sink, payload) => new Promise((resolve) => {
    slot.current = { label, sink, resolve };
    slot.w.postMessage({ cmd: 'job', job: ++jobSeq, ...payload }, payload.files.map((f) => f.data));
  });

  const finish = (code, workers, jobCount) => {
    for (const s of slots) { try { s.w.terminate(); } catch (_) {} }
    if (gpu) { try { gpu.w.terminate(); } catch (_) {} }
    cb({ type: 'done', code, workers, jobs: jobCount });
  };

  const slot0 = mkSlot(0);
  let entity = null;
  const scanned = [];
  const scanRes = await run(slot0, '(scan)', (ev) => {
    if (ev.ev === 'entity') entity = ev;
    else if (ev.ev === 'deps') scanned.push(ev);
  }, { ...base, mode: 1, lod: false, files: files.map(copyOf) });
  if (!entity || scanned.length === 0) {
    finish(scanRes.trapped ? -1 : scanRes.code, 1, 0);
    return;
  }

  const jobs = scanned.map((d) => ({
    glb: d.file,
    names: [d.file, ...d.deps.filter((n) => n !== d.file && byName.has(n))],
  }));
  const wantLod = !!opts.lod;
  const jobCount = jobs.length + (wantLod ? 1 : 0);
  const hc = (typeof navigator !== 'undefined' && navigator.hardwareConcurrency) || 4;
  // Default to 3/4 of the machine's cores: enough to saturate the encode
  // without starving the page, the GPU worker, and the rest of the system.
  const cap = Math.max(1, Math.ceil((hc * 3) / 4));
  const N = Math.max(1, Math.min(opts.size || cap, jobCount, cap));
  cb({ type: 'event', data: { ev: 'note', msg: `worker pool: ${N} workers, ${jobCount} job(s)` } });
  while (slots.length < N) mkSlot(slots.length);

  const ident = { entityType: entity.entityType, entityHash: entity.entityHash };
  const built = [];
  let failures = 0;
  let qi = 0;
  const drain = async (slot) => {
    while (qi < jobs.length) {
      const j = jobs[qi++];
      let ok = false;
      await run(slot, j.glb, (ev) => {
        if (ev.ev === 'file-done' && ev.file === j.glb) { ok = true; built.push(ev.bundle); }
      }, {
        ...base, ...ident, mode: 2, lod: false, onlyGlb: j.glb, contentTable: table,
        files: j.names.map((n) => copyOf(byName.get(n))),
      });
      if (!ok) failures++;
    }
  };
  await Promise.all(slots.map(drain));

  if (wantLod) {
    const lodFiles = files.filter((f) => {
      const n = f.name.toLowerCase();
      return MODEL_RE.test(n) || n.endsWith('scene.json') || ISS_RE.test(n);
    });
    await run(slots[0], 'LOD', null, {
      ...base, ...ident, mode: 3, lod: true, files: lodFiles.map(copyOf),
    });
  }

  built.sort();
  const manifest = {
    version: 'v-wasm-poc',
    files: [...built.filter((b, i) => i === 0 || b !== built[i - 1]), 'dcl'],
    exitCode: failures ? 12 : 0,
    contentServerUrl: 'wasm://in-browser',
  };
  cb({ type: 'manifest', json: manifest });
  finish(manifest.exitCode, N, jobCount);
}
