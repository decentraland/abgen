//! Host-encode hook for wasm embedders (wasm32 twin of `gpu_dispatch`).
//!
//! `poc_convert` is synchronous while browser WebGPU readback is async-only,
//! so the module itself cannot drive the GPU. An embedder that CAN — a worker
//! bridging to a WebGPU sibling over SharedArrayBuffer + Atomics.wait —
//! registers a hook here; every BC7 mip-chain encode is offered to it first
//! and falls through to the scalar/SIMD CPU path whenever the hook is unset,
//! declines (no GPU, oversized payload) or fails. Output bytes are the
//! embedder's contract: the WebGPU lane must be the same bit-exact WGSL
//! kernels the native wgpu backend qualifies.

use super::Bc7Profile;
use std::sync::atomic::{AtomicUsize, Ordering};

#[allow(clippy::type_complexity)]
pub type EncodeHook = fn(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    srgb: bool,
    perceptual: bool,
    profile: Bc7Profile,
) -> Option<(Vec<u8>, i32)>;

static HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_encode_hook(hook: EncodeHook) {
    HOOK.store(hook as usize, Ordering::Relaxed);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn try_encode(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    srgb: bool,
    perceptual: bool,
    profile: Bc7Profile,
) -> Option<(Vec<u8>, i32)> {
    let p = HOOK.load(Ordering::Relaxed);
    if p == 0 {
        return None;
    }
    let hook: EncodeHook = unsafe { std::mem::transmute::<usize, EncodeHook>(p) };
    hook(
        rgba, width, height, mip_count, flip, srgb, perceptual, profile,
    )
}
