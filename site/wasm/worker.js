// Conversion worker: hosts the abgen wasm module off the main thread.
// Protocol with the pool: see pool.js — 'init' delivers the compiled Module,
// 'job' carries one mode-1/2/3 payload; every reply is tagged with the job id.
// Protocol with wasm: C ABI — poc_alloc/poc_free/poc_init/poc_convert plus
// imported host_emit(kind,ptr,len).

let exports = null;
let module = null;
let jobId = 0;
const td = new TextDecoder();

function hostEmit(kind, ptr, len) {
  const view = new Uint8Array(exports.memory.buffer, ptr, len);
  const bytes = view.slice();
  if (kind === 0) {
    postMessage({ type: 'event', job: jobId, data: JSON.parse(td.decode(bytes)) });
  } else if (kind === 1) {
    const dv = new DataView(bytes.buffer);
    const nl = dv.getUint32(0, true);
    const name = td.decode(bytes.subarray(4, 4 + nl));
    const dl = dv.getUint32(4 + nl, true);
    const data = bytes.slice(8 + nl, 8 + nl + dl);
    postMessage({ type: 'output', job: jobId, name, size: dl, data: data.buffer }, [data.buffer]);
  } else if (kind === 2) {
    postMessage({ type: 'fatal', job: jobId, msg: td.decode(bytes) });
  } else if (kind === 3) {
    postMessage({ type: 'manifest', job: jobId, json: JSON.parse(td.decode(bytes)) });
  }
}

// The C codecs (libjpeg/crnlib/draco) are linked against wasi-libc, whose
// stdio/clock/env calls surface as wasi_snapshot_preview1 imports. None of
// them matter for conversion output — stub the lot.
const wasiStubs = {
  proc_exit: (code) => { throw new Error(`wasi proc_exit(${code})`); },
  fd_write: (fd, iovs, iovsLen, nwritten) => {
    const dv = new DataView(exports.memory.buffer);
    let total = 0, text = '';
    for (let i = 0; i < iovsLen; i++) {
      const ptr = dv.getUint32(iovs + i * 8, true);
      const len = dv.getUint32(iovs + i * 8 + 4, true);
      text += td.decode(new Uint8Array(exports.memory.buffer, ptr, len));
      total += len;
    }
    if (text.trim()) console.log('[wasi]', text.trim());
    dv.setUint32(nwritten, total, true);
    return 0;
  },
  fd_close: () => 0,
  fd_seek: () => 8,
  fd_fdstat_get: () => 8,
  fd_prestat_get: () => 8,
  fd_prestat_dir_name: () => 8,
  environ_sizes_get: (countPtr, sizePtr) => {
    const dv = new DataView(exports.memory.buffer);
    dv.setUint32(countPtr, 0, true);
    dv.setUint32(sizePtr, 0, true);
    return 0;
  },
  environ_get: () => 0,
  clock_time_get: (id, precision, outPtr) => {
    new DataView(exports.memory.buffer)
      .setBigUint64(outPtr, BigInt(Math.round(performance.now() * 1e6)), true);
    return 0;
  },
  random_get: (ptr, len) => {
    crypto.getRandomValues(new Uint8Array(exports.memory.buffer, ptr, len));
    return 0;
  },
};
const wasi = new Proxy(wasiStubs, {
  get: (t, k) => k in t ? t[k] : ((...a) => { console.warn('wasi stub miss:', k); return 52; }),
});

async function ensureWasm() {
  if (exports) return;
  const imports = {
    env: { host_emit: hostEmit },
    wasi_snapshot_preview1: wasi,
  };
  const r = module
    ? await WebAssembly.instantiate(module, imports)
    : await WebAssembly.instantiateStreaming(fetch('abgen_poc.wasm'), imports);
  exports = (r.instance || r).exports;
  exports.poc_init();
  postMessage({ type: 'ready' });
}

function buildInput(m) {
  const te = new TextEncoder();
  const parts = [];
  const u32 = (n) => {
    const b = new Uint8Array(4);
    new DataView(b.buffer).setUint32(0, n >>> 0, true);
    return b;
  };
  const chunk = (s) => {
    const b = te.encode(s || '');
    parts.push(u32(b.length), b);
  };
  parts.push(u32(m.files.length));
  for (const f of m.files) {
    chunk(f.name);
    parts.push(u32(f.data.byteLength), new Uint8Array(f.data));
  }
  chunk(m.platform);
  chunk(m.entityType);
  parts.push(new Uint8Array([m.magenta ? 1 : 0, m.lod ? 1 : 0, m.mode | 0, m.crop ? 1 : 0]));
  parts.push(u32(m.triCap | 0));
  chunk(m.entityHash);
  chunk(m.onlyGlb);
  const table = m.contentTable || [];
  parts.push(u32(table.length));
  for (const [name, hash] of table) { chunk(name); chunk(hash); }
  let total = 0;
  for (const p of parts) total += p.byteLength;
  const blob = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    blob.set(p, off);
    off += p.byteLength;
  }
  return blob;
}

onmessage = async (e) => {
  const m = e.data;
  if (m.cmd === 'init') {
    module = m.module;
    try {
      await ensureWasm();
    } catch (err) {
      postMessage({ type: 'fatal', msg: String((err && err.message) || err) });
    }
    return;
  }
  if (m.cmd !== 'job') return;
  jobId = m.job;
  try {
    await ensureWasm();
    const blob = buildInput(m);
    const ptr = exports.poc_alloc(blob.byteLength);
    new Uint8Array(exports.memory.buffer, ptr, blob.byteLength).set(blob);
    let code;
    try {
      code = exports.poc_convert(ptr, blob.byteLength);
    } finally {
      try { exports.poc_free(ptr, blob.byteLength); } catch (_) {}
    }
    postMessage({ type: 'done', job: jobId, code });
  } catch (err) {
    // any throw out of poc_convert is a trap: the instance is poisoned, the
    // pool must terminate this worker and respawn from the cached Module.
    postMessage({ type: 'trap', job: jobId, msg: String((err && err.message) || err) });
    exports = null;
  }
};
