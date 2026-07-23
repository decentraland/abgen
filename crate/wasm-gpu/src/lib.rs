//! WebGPU encode module for the wasm demo: the same bit-exact WGSL BC7 lane
//! the native wgpu backend runs, hosted in its own worker because browser
//! GPU readback is async-only. `gpu_init` acquires the adapter and
//! self-qualifies it against the in-module CPU oracle over the native
//! qualification matrix (sizes × srgb × perceptual × profile) — an adapter
//! that is not bit-identical to the CPU path is refused, exactly like the
//! native per-device contract. `gpu_encode` then services one request at a
//! time from the convert workers' SharedArrayBuffer bridge.
//!
//! Request layout (LE, mirrors abgen-wasm's gpu_host_encode): u32 width,
//! u32 height, i32 mips, u32 flags (bit0 flip, bit1 srgb, bit2 perceptual,
//! bit3 profile-basic), then rgba bytes. Response: the raw BC7 mip chain.

use abgen::bc7_pure;
use abgen::gpu::{build_engine, encode_bc7_mip_chain_on, init_gpu, Bc7Profile, Engine, Gpu};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::prelude::*;

thread_local! {
    static STATE: RefCell<Option<Rc<(Gpu, Engine)>>> = const { RefCell::new(None) };
}

fn cpu_profile(p: Bc7Profile) -> bc7_pure::Bc7Profile {
    match p {
        Bc7Profile::Slow => bc7_pure::Bc7Profile::Slow,
        Bc7Profile::Basic => bc7_pure::Bc7Profile::Basic,
    }
}

/// Deterministic RGBA test texture (self-contained LCG; the qualification
/// only needs inputs whose CPU encode is computed in this same module).
fn gen_texture(seed: u64, w: u32, h: u32) -> Vec<u8> {
    let mut s = seed.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(1);
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..(w * h) {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let v = s >> 32;
        out.extend_from_slice(&[
            (v & 0xff) as u8,
            ((v >> 8) & 0xff) as u8,
            ((v >> 16) & 0xff) as u8,
            ((v >> 24) & 0xff) as u8,
        ]);
    }
    out
}

async fn qualify(g: &Gpu, eng: &Engine) -> Result<(), String> {
    for &(w, h) in &[(64u32, 64u32), (128, 32), (37, 53)] {
        let tex = gen_texture(1, w, h);
        for srgb in [false, true] {
            for perceptual in [false, true] {
                for profile in [Bc7Profile::Slow, Bc7Profile::Basic] {
                    let (want, want_mips) = bc7_pure::encode_bc7_mip_chain_with_profile(
                        &tex,
                        w,
                        h,
                        None,
                        true,
                        srgb,
                        perceptual,
                        cpu_profile(profile),
                    );
                    let (got, got_mips) = encode_bc7_mip_chain_on(
                        g, eng, &tex, w, h, None, true, srgb, perceptual, profile,
                    )
                    .await
                    .map_err(|e| format!("qualification encode failed: {e:#}"))?;
                    if got != want || got_mips != want_mips {
                        return Err(format!(
                            "not bit-exact vs CPU oracle at {w}x{h} srgb={srgb} \
                             perceptual={perceptual} profile={profile:?}"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

/// Acquire + qualify the adapter. Resolves to the adapter summary string on
/// success; rejects (and leaves the module unarmed) on any failure.
#[wasm_bindgen]
pub async fn gpu_init() -> Result<String, JsValue> {
    let g = init_gpu().await.map_err(|e| JsValue::from_str(&e))?;
    let eng = build_engine(&g);
    qualify(&g, &eng)
        .await
        .map_err(|e| JsValue::from_str(&format!("{}: {e}", g.adapter_summary())))?;
    let summary = g.adapter_summary();
    STATE.with(|s| *s.borrow_mut() = Some(Rc::new((g, eng))));
    Ok(summary)
}

#[wasm_bindgen]
pub async fn gpu_encode(req: &[u8]) -> Result<js_sys::Uint8Array, JsValue> {
    if req.len() < 16 {
        return Err(JsValue::from_str("request too short"));
    }
    let u32_at = |i: usize| u32::from_le_bytes(req[i..i + 4].try_into().unwrap());
    let (w, h) = (u32_at(0), u32_at(4));
    let mips = u32_at(8) as i32;
    let flags = u32_at(12);
    let rgba = &req[16..];
    if rgba.len() != (w as usize) * (h as usize) * 4 {
        return Err(JsValue::from_str("rgba length mismatch"));
    }
    let profile = if flags & 8 != 0 {
        Bc7Profile::Basic
    } else {
        Bc7Profile::Slow
    };
    // Rc out of the thread-local: encode awaits GPU readback and must not
    // hold the RefCell borrow across the await.
    let state = STATE.with(|s| s.borrow().clone());
    let Some(state) = state else {
        return Err(JsValue::from_str("gpu_init has not succeeded"));
    };
    let (g, eng) = (&state.0, &state.1);
    let (out, _mips) = encode_bc7_mip_chain_on(
        g,
        eng,
        rgba,
        w,
        h,
        Some(mips),
        flags & 1 != 0,
        flags & 2 != 0,
        flags & 4 != 0,
        profile,
    )
    .await
    .map_err(|e| JsValue::from_str(&format!("{e:#}")))?;
    Ok(js_sys::Uint8Array::from(out.as_slice()))
}
