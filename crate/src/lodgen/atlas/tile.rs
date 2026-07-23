use image::RgbaImage;
use std::collections::HashMap;

use super::{MIN_TILE_DIM, UV_EPS};
use crate::bc7_pure::{linear_to_srgb_u8, srgb_to_linear_u8};
use crate::lodgen::model::{AlphaClass, LodMaterial, LodPrimitive};

enum TileKind {
    Solid([u8; 4]),
    Image(Vec<u8>),
}

pub(super) struct Tile {
    kind: TileKind,
    pub(super) src_w: u32,
    pub(super) src_h: u32,
    pub(super) w: u32,
    pub(super) h: u32,
    pub(super) hash: String,
}

impl Tile {
    pub(super) fn from_pixels(pixels: Vec<u8>, w: u32, h: u32) -> Tile {
        let mut hashed = Vec::with_capacity(pixels.len() + 8);
        hashed.extend_from_slice(&w.to_le_bytes());
        hashed.extend_from_slice(&h.to_le_bytes());
        hashed.extend_from_slice(&pixels);
        let hash = crate::hashes::sha256_hex(&hashed);
        Tile {
            kind: TileKind::Image(pixels),
            src_w: w,
            src_h: h,
            w,
            h,
            hash,
        }
    }

    fn solid(color: [u8; 4]) -> Tile {
        let mut hashed = b"solid:".to_vec();
        hashed.extend_from_slice(&color);
        let hash = crate::hashes::sha256_hex(&hashed);
        Tile {
            kind: TileKind::Solid(color),
            src_w: MIN_TILE_DIM,
            src_h: MIN_TILE_DIM,
            w: MIN_TILE_DIM,
            h: MIN_TILE_DIM,
            hash,
        }
    }

    pub(super) fn is_solid(&self) -> bool {
        matches!(self.kind, TileKind::Solid(_))
    }

    pub(super) fn render_cropped(
        &self,
        crop: Option<[u32; 4]>,
        w: u32,
        h: u32,
        premul: bool,
    ) -> Vec<u8> {
        match &self.kind {
            TileKind::Solid(c) => {
                let mut px = Vec::with_capacity(w as usize * h as usize * 4);
                for _ in 0..w as usize * h as usize {
                    px.extend_from_slice(c);
                }
                px
            }
            TileKind::Image(src) => {
                let (cx, cy, cw, ch) = match crop {
                    Some([x, y, cw, ch]) => (x, y, cw, ch),
                    None => (0, 0, self.src_w, self.src_h),
                };
                let window: Vec<u8> = if (cx, cy, cw, ch) == (0, 0, self.src_w, self.src_h) {
                    src.clone()
                } else {
                    let mut out = Vec::with_capacity(cw as usize * ch as usize * 4);
                    for row in cy..cy + ch {
                        let start = (row as usize * self.src_w as usize + cx as usize) * 4;
                        out.extend_from_slice(&src[start..start + cw as usize * 4]);
                    }
                    out
                };
                if (w, h) == (cw, ch) {
                    window
                } else {
                    downscale(
                        &window,
                        cw as usize,
                        ch as usize,
                        w as usize,
                        h as usize,
                        premul,
                    )
                }
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum EmisKey {
    Dark,
    Solid([u8; 4]),
    Image { hash: String, factor: [u64; 3] },
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) enum TileKey {
    Image {
        hash: String,
        tint: [u64; 4],
        reps: (u32, u32),
        emis: EmisKey,
    },
    Solid([u8; 4], EmisKey),
}

pub(super) enum UvMap {
    Rect { shift: [f64; 2], reps: [f64; 2] },
    Center,
}

pub(super) enum UvPlan {
    Rect { shift: [f64; 2], reps: (u32, u32) },
    Fallback,
}

#[derive(Default)]
pub(super) struct Bucket {
    pub(super) tiles: Vec<Tile>,
    pub(super) emis: Vec<Tile>,
    pub(super) weights: Vec<f64>,
    by_key: HashMap<TileKey, usize>,
    pub(super) prims: Vec<(usize, usize, UvMap)>,
    pub(super) refs: usize,
    pub(super) fallbacks: usize,
}

pub(super) fn prim_area(p: &LodPrimitive) -> f64 {
    let mut area = 0.0f64;
    for t in p.indices.chunks_exact(3) {
        let a = p.positions[t[0] as usize].map(|v| v as f64);
        let b = p.positions[t[1] as usize].map(|v| v as f64);
        let c = p.positions[t[2] as usize].map(|v| v as f64);
        let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let v = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let x = u[1] * v[2] - u[2] * v[1];
        let y = u[2] * v[0] - u[0] * v[2];
        let z = u[0] * v[1] - u[1] * v[0];
        area += (x * x + y * y + z * z).sqrt() * 0.5;
    }
    if area.is_finite() {
        area
    } else {
        0.0
    }
}

pub(super) fn tint_bits(c: [f64; 4]) -> [u64; 4] {
    c.map(|v| v.to_bits())
}

pub(super) fn srgb_encode(v: f64) -> f64 {
    if v <= 0.003_130_8 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

pub(super) fn srgb_decode(v: f64) -> f64 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

pub(super) fn solid_color(c: [f64; 4]) -> [u8; 4] {
    [
        (srgb_encode(c[0].clamp(0.0, 1.0)) * 255.0).round() as u8,
        (srgb_encode(c[1].clamp(0.0, 1.0)) * 255.0).round() as u8,
        (srgb_encode(c[2].clamp(0.0, 1.0)) * 255.0).round() as u8,
        (c[3].clamp(0.0, 1.0) * 255.0).round() as u8,
    ]
}

pub(super) fn tinted_pixels(img: &RgbaImage, tint: [f64; 4]) -> Vec<u8> {
    let t = tint.map(|v| v.clamp(0.0, 1.0));
    if t == [1.0; 4] {
        return img.as_raw().clone();
    }
    let mut out = Vec::with_capacity(img.as_raw().len());
    for px in img.as_raw().chunks_exact(4) {
        for ch in 0..3 {
            let lin = srgb_decode(px[ch] as f64 / 255.0) * t[ch];
            out.push((srgb_encode(lin.clamp(0.0, 1.0)) * 255.0).round() as u8);
        }
        out.push((px[3] as f64 * t[3]).round().clamp(0.0, 255.0) as u8);
    }
    out
}

pub(super) fn emissive_pixels(img: &RgbaImage, factor: [f64; 3]) -> Vec<u8> {
    let f = factor.map(|v| v.clamp(0.0, 1.0));
    let mut out = Vec::with_capacity(img.as_raw().len());
    for px in img.as_raw().chunks_exact(4) {
        for ch in 0..3 {
            let lin = srgb_decode(px[ch] as f64 / 255.0) * f[ch];
            out.push((srgb_encode(lin.clamp(0.0, 1.0)) * 255.0).round() as u8);
        }
        out.push(255);
    }
    out
}

pub(super) fn emissive_solid(factor: [f64; 3]) -> [u8; 4] {
    solid_color([factor[0], factor[1], factor[2], 1.0])
}

pub(super) fn glows(m: &LodMaterial) -> bool {
    m.emissive != [0.0; 3]
}

pub(super) fn emissive_tile(
    eimg: &RgbaImage,
    factor: [f64; 3],
    base_dims: (u32, u32),
    reps: (u32, u32),
    cap: u32,
) -> Tile {
    let mut px = emissive_pixels(eimg, factor);
    let (mut w, mut h) = (eimg.width(), eimg.height());
    if (w, h) != base_dims {
        px = crate::resize::box_downscale_rgba(
            &px,
            w as usize,
            h as usize,
            base_dims.0 as usize,
            base_dims.1 as usize,
            true,
        );
        (w, h) = base_dims;
    }
    let (px, w, h) = fused_repeat_bake(px, w, h, reps, cap, false);
    Tile::from_pixels(px, w, h)
}

pub(super) fn fused_repeat_bake(
    pixels: Vec<u8>,
    w: u32,
    h: u32,
    reps: (u32, u32),
    cap: u32,
    premul: bool,
) -> (Vec<u8>, u32, u32) {
    let (ru, rv) = reps;
    let full = (w as u64 * ru as u64).max(h as u64 * rv as u64);
    let (pixels, w, h) = if full <= cap as u64 {
        (pixels, w, h)
    } else {
        let scale = cap as f64 / full as f64;
        let pw = ((w as f64 * scale).floor() as u32).clamp(1, cap);
        let ph = ((h as f64 * scale).floor() as u32).clamp(1, cap);
        let out = downscale(
            &pixels,
            w as usize,
            h as usize,
            pw as usize,
            ph as usize,
            premul,
        );
        (out, pw, ph)
    };
    let tw = (w as u64 * ru as u64).min(cap as u64) as u32;
    let th = (h as u64 * rv as u64).min(cap as u64) as u32;
    if (tw, th) == (w, h) {
        return (pixels, w, h);
    }
    let mut out = vec![0u8; tw as usize * th as usize * 4];
    for y in 0..th as usize {
        let srow = (y % h as usize) * w as usize;
        for x in 0..tw as usize {
            let d = (y * tw as usize + x) * 4;
            let s = (srow + x % w as usize) * 4;
            out[d..d + 4].copy_from_slice(&pixels[s..s + 4]);
        }
    }
    (out, tw, th)
}

pub(super) fn premultiplied_filtering(class: AlphaClass) -> bool {
    class != AlphaClass::Opaque
}

pub(super) fn downscale(
    px: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    premul: bool,
) -> Vec<u8> {
    if premul {
        crate::resize::premul_downscale_rgba(px, sw, sh, dw, dh)
    } else {
        crate::resize::box_downscale_rgba(px, sw, sh, dw, dh, true)
    }
}

pub(super) fn average_color(class: AlphaClass, pixels: &[u8]) -> [u8; 4] {
    let n = (pixels.len() / 4).max(1) as f64;
    let mut weighted = [0f64; 3];
    let mut unweighted = [0f64; 3];
    let mut asum = 0f64;
    for px in pixels.chunks_exact(4) {
        let a = px[3] as f64 / 255.0;
        for ch in 0..3 {
            let lin = srgb_to_linear_u8(px[ch]) as f64;
            weighted[ch] += lin * a;
            unweighted[ch] += lin;
        }
        asum += a;
    }
    let alpha_weighted = premultiplied_filtering(class) && asum > 0.0;
    let mut out = [0u8; 4];
    for ch in 0..3 {
        let lin = if alpha_weighted {
            weighted[ch] / asum
        } else {
            unweighted[ch] / n
        };
        out[ch] = linear_to_srgb_u8(lin as f32);
    }
    out[3] = (asum / n * 255.0).round().clamp(0.0, 255.0) as u8;
    out
}

pub(super) fn solid_tile(color: [u8; 4]) -> Tile {
    Tile::solid(color)
}

pub(super) fn uv_plan(uvs: &[[f32; 2]]) -> UvPlan {
    let mut mn = [f64::INFINITY; 2];
    let mut mx = [f64::NEG_INFINITY; 2];
    for uv in uvs {
        for a in 0..2 {
            let v = uv[a] as f64;
            mn[a] = mn[a].min(v);
            mx[a] = mx[a].max(v);
        }
    }
    if !mn.iter().chain(mx.iter()).all(|v| v.is_finite()) {
        return UvPlan::Fallback;
    }
    let shift = [mn[0].floor(), mn[1].floor()];
    let smax = [mx[0] - shift[0], mx[1] - shift[1]];
    let reps = [
        ((smax[0] - UV_EPS).ceil() as i64).clamp(1, u32::MAX as i64),
        ((smax[1] - UV_EPS).ceil() as i64).clamp(1, u32::MAX as i64),
    ];
    UvPlan::Rect {
        shift,
        reps: (reps[0] as u32, reps[1] as u32),
    }
}

pub(super) fn intern_tile(
    bucket: &mut Bucket,
    key: TileKey,
    make: impl FnOnce() -> (Tile, Tile),
) -> usize {
    if let Some(&i) = bucket.by_key.get(&key) {
        return i;
    }
    let i = bucket.tiles.len();
    let (base, emis) = make();
    bucket.tiles.push(base);
    bucket.emis.push(emis);
    bucket.weights.push(0.0);
    bucket.by_key.insert(key, i);
    i
}
