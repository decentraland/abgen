// WebGPU encode worker: hosts the wasm-bindgen abgen GPU module (the same
// bit-exact WGSL BC7 lane the native wgpu backend runs) and services encode
// requests from the convert workers over a SharedArrayBuffer.
//
// SAB protocol (Int32 header, payload at byte 16):
//   [0] lock    — held by the requesting convert worker, not touched here
//   [1] state   — 0 idle, 1 request ready, 2 done, 3 failed
//   [2] reqLen  — request bytes at payload offset
//   [3] outLen  — response bytes at payload offset (state 2)
// The convert worker blocks on Atomics.wait; this worker must keep its event
// loop alive for WebGPU readback, so it observes state via Atomics.waitAsync.

import init, { gpu_init, gpu_encode } from './gpu/abgen_wasm_gpu.js';

const HDR = 16;
let i32 = null;
let u8 = null;

async function until(idx, want) {
  for (;;) {
    const v = Atomics.load(i32, idx);
    if (v === want) return;
    const r = Atomics.waitAsync(i32, idx, v);
    if (r.async) await r.value;
  }
}

async function serve() {
  for (;;) {
    await until(1, 1);
    const len = i32[2];
    let ok = false;
    try {
      const out = await gpu_encode(u8.slice(HDR, HDR + len));
      if (out.length <= u8.length - HDR) {
        u8.set(out, HDR);
        i32[3] = out.length;
        ok = true;
      }
    } catch (e) {
      console.warn('webgpu encode failed, request falls back to CPU:', e);
    }
    Atomics.store(i32, 1, ok ? 2 : 3);
    Atomics.notify(i32, 1);
    await until(1, 0);
  }
}

onmessage = async (e) => {
  if (e.data.cmd !== 'init') return;
  try {
    await init();
    const adapter = await gpu_init(); // acquires + bit-exact-qualifies the adapter
    i32 = new Int32Array(e.data.sab);
    u8 = new Uint8Array(e.data.sab);
    postMessage({ ok: true, adapter });
    serve();
  } catch (err) {
    postMessage({ ok: false, err: String((err && err.message) || err) });
  }
};
