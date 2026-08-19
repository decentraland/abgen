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
