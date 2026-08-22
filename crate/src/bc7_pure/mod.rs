#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

mod alpha;
mod bits;
mod ccc;
mod color;
#[cfg(target_arch = "aarch64")]
mod est_neon;
mod est_simd;
#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
mod est_wasm128;
mod estimate;
mod evaluate;
mod mip;
mod opaque;
mod opt_tables;
mod pack;
mod partition;
mod tables;
#[cfg(target_arch = "wasm32")]
mod wasm_hook;

pub use mip::*;
#[cfg(target_arch = "wasm32")]
pub use wasm_hook::{set_encode_hook, EncodeHook};

use alpha::*;
use bits::*;
use ccc::*;
use color::*;
use est_simd::*;
use estimate::*;
use evaluate::*;
use opaque::*;
use opt_tables::*;
use pack::*;
use partition::*;
use std::sync::OnceLock;
use tables::*;

#[derive(Clone)]
pub struct Params {
    pub max_partitions_mode: [u32; 8],
    pub weights: [u32; 4],
    pub uber_level: u32,
    pub refinement_passes: u32,
    pub mode4_rotation_mask: u32,
    pub mode4_index_mask: u32,
    pub mode5_rotation_mask: u32,
    pub uber1_mask: u32,
    pub perceptual: bool,
    pub pbit_search: bool,
    pub mode6_only: bool,

    pub op_max_mode13: u32,
    pub op_max_mode0: u32,
    pub op_max_mode2: u32,
    pub use_mode: [bool; 7],

    pub al_max_mode7: u32,
    pub mode67_weight_mul: [u32; 4],
    pub use_mode4: bool,
    pub use_mode5: bool,
    pub use_mode6: bool,
    pub use_mode7: bool,
    pub use_mode4_rotation: bool,
    pub use_mode5_rotation: bool,
}

impl Params {
    pub const fn slow(perceptual: bool) -> Self {
        let weights = if perceptual {
            [128, 64, 16, 256]
        } else {
            [1, 1, 1, 1]
        };
        Params {
            max_partitions_mode: [16, 64, 64, 64, 0, 0, 0, 64],
            weights,
            uber_level: 0,
            refinement_passes: 1,
            mode4_rotation_mask: 0xF,
            mode4_index_mask: 3,
            mode5_rotation_mask: 0xF,
            uber1_mask: 7,
            perceptual,
            pbit_search: true,
            mode6_only: false,
            op_max_mode13: 1,
            op_max_mode0: 1,
            op_max_mode2: 1,
            use_mode: [true; 7],
            al_max_mode7: 2,
            mode67_weight_mul: [1, 1, 1, 1],
            use_mode4: true,
            use_mode5: true,
            use_mode6: true,
            use_mode7: true,
            use_mode4_rotation: true,
            use_mode5_rotation: true,
        }
    }

    pub fn basic(perceptual: bool) -> Self {
        let mut p = Self::slow(perceptual);
        p.uber_level = 1;
        p.pbit_search = false;
        p.al_max_mode7 = 1;
        if perceptual {
            p.use_mode[0] = false;
            p.use_mode[2] = false;
            p.use_mode[3] = false;
            p.use_mode[4] = false;
            p.use_mode[5] = false;
        } else {
            p.max_partitions_mode[1] = 32;
            p.max_partitions_mode[2] = 32;
            p.max_partitions_mode[3] = 32;
            p.max_partitions_mode[7] = 32;
            p.use_mode[2] = false;
        }
        p
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bc7Profile {
    Slow,
    Basic,
}

fn apply_mode_tree_hint(pixels: &[ColorI; 16], cp: &Params) -> Option<Params> {
    let mut rgba = [[0i32; 4]; 16];
    for i in 0..16 {
        rgba[i] = pixels[i].c;
    }
    let feat = crate::bc7_mode_tree::block_features(&rgba);
    let (mode, conf) = crate::bc7_mode_tree::predict(&feat);
    let thr = 9000u16;
    let var_rgb = feat[0];
    let max_dr = feat[1];
    let mut p = cp.clone();
    match mode {
        5 if conf >= thr => {
            p.use_mode4 = false;
            p.use_mode6 = false;
            p.use_mode7 = false;
        }
        6 if conf >= thr && var_rgb >= 200 && max_dr >= 16 => {
            p.use_mode4 = false;
            p.use_mode5 = false;
            p.use_mode7 = false;
        }
        _ => return None,
    }
    Some(p)
}

#[derive(Clone, Copy, PartialEq)]
enum BlockClass {
    Solid([i32; 4]),
    Alpha(i32, i32),
    Opaque,
}

fn classify_block(pixels: &[ColorI; 16]) -> BlockClass {
    let (mut lo_r, mut hi_r) = (255i32, 0i32);
    let (mut lo_g, mut hi_g) = (255i32, 0i32);
    let (mut lo_b, mut hi_b) = (255i32, 0i32);
    let (mut lo_a, mut hi_a) = (255i32, 0i32);
    for i in 0..16 {
        let r = pixels[i].c[0];
        let g = pixels[i].c[1];
        let b = pixels[i].c[2];
        let a = pixels[i].c[3];
        lo_r = lo_r.min(r);
        hi_r = hi_r.max(r);
        lo_g = lo_g.min(g);
        hi_g = hi_g.max(g);
        lo_b = lo_b.min(b);
        hi_b = hi_b.max(b);
        lo_a = lo_a.min(a);
        hi_a = hi_a.max(a);
    }
    if lo_r == hi_r && lo_g == hi_g && lo_b == hi_b && lo_a == hi_a {
        BlockClass::Solid([lo_r, lo_g, lo_b, lo_a])
    } else if lo_a < 255 {
        BlockClass::Alpha(lo_a, hi_a)
    } else {
        BlockClass::Opaque
    }
}

/// Encodes `group` (at most `SIMD_W` blocks) into `out`, 16 bytes per block.
/// All per-group state lives in fixed arrays: the only heap traffic left is
/// the partition-plan solution lists, which are moved here, never cloned.
fn compress_group_into(group: &[[ColorI; 16]], cp: &Params, out: &mut [u8]) {
    let n = group.len();
    debug_assert!(n <= SIMD_W && out.len() == n * 16);
    if n == 0 {
        return;
    }
    let mut base = CCParams::clear();
    base.weights = cp.weights;

    let mut classes = [BlockClass::Opaque; SIMD_W];
    for (c, pixels) in classes.iter_mut().zip(group) {
        *c = classify_block(pixels);
    }

    let mut alpha_idx = [0usize; SIMD_W];
    let mut alpha_n = 0usize;
    let mut opaque_idx = [0usize; SIMD_W];
    let mut opaque_n = 0usize;
    for i in 0..n {
        if matches!(classes[i], BlockClass::Alpha(..)) {
            alpha_idx[alpha_n] = i;
            alpha_n += 1;
        } else if classes[i] == BlockClass::Opaque && !cp.mode6_only {
            opaque_idx[opaque_n] = i;
            opaque_n += 1;
        }
    }

    let mut plans: [PartitionPlan; SIMD_W] = Default::default();
    let mut lanes: [&[ColorI; 16]; SIMD_W] = [&group[0]; SIMD_W];
    if alpha_n > 0 && cp.use_mode7 {
        for k in 0..alpha_n {
            lanes[k] = &group[alpha_idx[k]];
        }
        let r = estimate_partition_list_group(7, &lanes[..alpha_n], cp, cp.al_max_mode7 as i32);
        for (k, list) in r.into_iter().enumerate() {
            plans[alpha_idx[k]].list7 = list;
        }
    }
    if opaque_n > 0 {
        for k in 0..opaque_n {
            lanes[k] = &group[opaque_idx[k]];
        }
        let sub_plans = build_partition_plans(&lanes[..opaque_n], cp, false);
        for (k, sub) in sub_plans.into_iter().enumerate() {
            plans[opaque_idx[k]] = sub;
        }
    }

    for (i, pixels) in group.iter().enumerate() {
        let blk = match classes[i] {
            BlockClass::Solid(c) => {
                handle_block_solid(c[0] as usize, c[1] as usize, c[2] as usize, c[3])
            }
            BlockClass::Alpha(lo, hi) => {
                let gated = apply_mode_tree_hint(pixels, cp);
                handle_alpha_block(
                    pixels,
                    gated.as_ref().unwrap_or(cp),
                    &base,
                    lo,
                    hi,
                    &plans[i],
                )
            }
            BlockClass::Opaque => {
                if cp.mode6_only {
                    handle_opaque_block_mode6(pixels, cp, &base)
                } else {
                    handle_opaque_block(pixels, cp, &base, &plans[i])
                }
            }
        };
        out[i * 16..i * 16 + 16].copy_from_slice(&blk);
    }
}

fn compress_group(group: &[[ColorI; 16]], cp: &Params) -> Vec<[u8; 16]> {
    let mut bytes = vec![0u8; group.len() * 16];
    compress_group_into(group, cp, &mut bytes);
    bytes
        .chunks_exact(16)
        .map(|c| {
            let mut b = [0u8; 16];
            b.copy_from_slice(c);
            b
        })
        .collect()
}

fn block_from_bytes(rgba16: &[u8]) -> [ColorI; 16] {
    let mut pixels = [ColorI::default(); 16];
    for i in 0..16 {
        pixels[i] = ColorI {
            c: [
                rgba16[i * 4] as i32,
                rgba16[i * 4 + 1] as i32,
                rgba16[i * 4 + 2] as i32,
                rgba16[i * 4 + 3] as i32,
            ],
        };
    }
    pixels
}

pub fn encode_blocks(rgba_block_major: &[u8], num_blocks: usize, params: &Params) -> Vec<u8> {
    assert_eq!(rgba_block_major.len(), num_blocks * 64);

    use rayon::prelude::*;
    let group_bytes = SIMD_W * 64;
    let mut out = vec![0u8; num_blocks * 16];
    rgba_block_major
        .par_chunks(group_bytes)
        .zip(out.par_chunks_mut(SIMD_W * 16))
        .for_each(|(chunk, dst)| {
            let n = chunk.len() / 64;
            let mut group = [[ColorI::default(); 16]; SIMD_W];
            for k in 0..n {
                group[k] = block_from_bytes(&chunk[k * 64..k * 64 + 64]);
            }
            compress_group_into(&group[..n], params, &mut dst[..n * 16]);
        });

    if let Some(path) = bc7_capture_path() {
        use std::io::Write;
        let mut rec = Vec::with_capacity(num_blocks * 80);
        for i in 0..num_blocks {
            rec.extend_from_slice(&out[i * 16..i * 16 + 16]);
            rec.extend_from_slice(&rgba_block_major[i * 64..i * 64 + 64]);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _g = BC7_CAPTURE_LOCK.lock().unwrap();
            let _ = f.write_all(&rec);
        }
    }
    out
}

static BC7_CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn bc7_capture_path() -> Option<std::path::PathBuf> {
    static P: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    P.get_or_init(|| std::env::var_os("ABGEN_BC7_CAPTURE").map(std::path::PathBuf::from))
        .clone()
}
