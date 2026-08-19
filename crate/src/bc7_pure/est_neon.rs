use super::*;
use std::arch::aarch64::*;

/// 16 pixels, 4 per NEON vector: every chunk loop is a constant-trip loop so
/// LLVM unrolls it and keeps the gathered lanes in registers instead of
/// spilling the `[float32x4_t; 4]` scratch to the stack for a runtime index.
const MAX_CHUNKS: usize = 4;

#[inline]
unsafe fn load_tab(src: &[f32; 16]) -> uint8x16x4_t {
    let p = src.as_ptr() as *const u8;
    uint8x16x4_t(
        vld1q_u8(p),
        vld1q_u8(p.add(16)),
        vld1q_u8(p.add(32)),
        vld1q_u8(p.add(48)),
    )
}

/// Byte-shuffle indices that gather 4 f32s (given 4 pixel indices in 0..16)
/// out of a 64-byte table with one `vqtbl4q_u8`.
#[inline]
unsafe fn byte_idx(idxs: *const i32) -> uint8x16_t {
    let idxv = vreinterpretq_u32_s32(vld1q_s32(idxs));
    let base = vmulq_u32(idxv, vdupq_n_u32(0x0404_0404));
    vaddq_u8(
        vreinterpretq_u8_u32(base),
        vreinterpretq_u8_u32(vdupq_n_u32(0x0302_0100)),
    )
}

#[inline]
unsafe fn tbl_gather(tab: uint8x16x4_t, bidx: uint8x16_t) -> float32x4_t {
    vreinterpretq_f32_u8(vqtbl4q_u8(tab, bidx))
}

/// Left-to-right accumulate of the first `cnt` lanes of `v` into `acc`.
///
/// Same add order as the scalar per-pixel loop. The `cnt == 4` case (every
/// chunk but a partial last one) is extracted lane-by-lane in registers: the
/// stack store/reload it replaces was a store-to-load-forwarding stall sitting
/// directly on the serial fadd dependency chain that is this kernel's critical
/// path.
#[inline]
unsafe fn sum4(v: float32x4_t, cnt: usize, acc: &mut f32) {
    if cnt == 4 {
        let mut a = *acc;
        a += vgetq_lane_f32::<0>(v);
        a += vgetq_lane_f32::<1>(v);
        a += vgetq_lane_f32::<2>(v);
        a += vgetq_lane_f32::<3>(v);
        *acc = a;
        return;
    }
    let mut a = *acc;
    if cnt > 0 {
        a += vgetq_lane_f32::<0>(v);
    }
    if cnt > 1 {
        a += vgetq_lane_f32::<1>(v);
    }
    if cnt > 2 {
        a += vgetq_lane_f32::<2>(v);
    }
    *acc = a;
}

pub(super) fn est_idx_neon(
    mode: usize,
    p: &CCParams,
    idxs: &[i32; 16],
    num_pixels: usize,
    lf: &LaneF32,
) -> u64 {
    if num_pixels == 0 {
        return 0;
    }
    unsafe {
        let rt = load_tab(&lf.r);
        let gt_ = load_tab(&lf.g);
        let bt = load_tab(&lf.b);
        let lane = vld1q_s32([0i32, 1, 2, 3].as_ptr());

        let nchunks = num_pixels.div_ceil(4);
        let mut rv = [vdupq_n_f32(0.0); 4];
        let mut gv = [vdupq_n_f32(0.0); 4];
        let mut bv = [vdupq_n_f32(0.0); 4];

        let v255 = vdupq_n_f32(255.0);
        let v0 = vdupq_n_f32(0.0);
        let (mut minr, mut ming, mut minb) = (v255, v255, v255);
        let (mut maxr, mut maxg, mut maxb) = (v0, v0, v0);
        for c in 0..MAX_CHUNKS {
            if c >= nchunks {
                break;
            }
            let cnt = (num_pixels - c * 4).min(4);
            let bidx = byte_idx(idxs.as_ptr().add(c * 4));
            rv[c] = tbl_gather(rt, bidx);
            gv[c] = tbl_gather(gt_, bidx);
            bv[c] = tbl_gather(bt, bidx);
            if cnt == 4 {
                minr = vminq_f32(minr, rv[c]);
                ming = vminq_f32(ming, gv[c]);
                minb = vminq_f32(minb, bv[c]);
                maxr = vmaxq_f32(maxr, rv[c]);
                maxg = vmaxq_f32(maxg, gv[c]);
                maxb = vmaxq_f32(maxb, bv[c]);
            } else {
                let valid = vcgtq_s32(vdupq_n_s32(cnt as i32), lane);
                minr = vminq_f32(minr, vbslq_f32(valid, rv[c], v255));
                ming = vminq_f32(ming, vbslq_f32(valid, gv[c], v255));
                minb = vminq_f32(minb, vbslq_f32(valid, bv[c], v255));
                maxr = vmaxq_f32(maxr, vbslq_f32(valid, rv[c], v0));
                maxg = vmaxq_f32(maxg, vbslq_f32(valid, gv[c], v0));
                maxb = vmaxq_f32(maxb, vbslq_f32(valid, bv[c], v0));
            }
        }
        let lr = vminvq_f32(minr);
        let lg = vminvq_f32(ming);
        let lb = vminvq_f32(minb);
        let hr = vmaxvq_f32(maxr);
        let hg = vmaxvq_f32(maxg);
        let hb = vmaxvq_f32(maxb);

        let n = 1u32 << G_COLOR_INDEX_BITCOUNT[mode];
        let sr = lr;
        let sg = lg;
        let sb = lb;
        let dir = hr - lr;
        let dig = hg - lg;
        let dib = hb - lb;
        let far = dir;
        let fag = dig;
        let fab = dib;
        let low = far * sr + fag * sg + fab * sb;
        let high = far * hr + fag * hg + fab * hb;
        let scale = (n as f32 - 1.0) / (high - low);
        let inv_n = 1.0 / (n as f32 - 1.0);

        let (wr, wg, wb) = if p.weights[0] != 1 || p.weights[1] != 1 || p.weights[2] != 1 {
            (
                p.weights[0] as f32,
                p.weights[1] as f32,
                p.weights[2] as f32,
            )
        } else {
            (1.0, 1.0, 1.0)
        };

        let farv = vdupq_n_f32(far);
        let fagv = vdupq_n_f32(fag);
        let fabv = vdupq_n_f32(fab);
        let lowv = vdupq_n_f32(low);
        let scalev = vdupq_n_f32(scale);
        let halfv = vdupq_n_f32(0.5);
        let invnv = vdupq_n_f32(inv_n);
        let onev = vdupq_n_f32(1.0);
        let srv = vdupq_n_f32(sr);
        let sgv = vdupq_n_f32(sg);
        let sbv = vdupq_n_f32(sb);
        let dirv = vdupq_n_f32(dir);
        let digv = vdupq_n_f32(dig);
        let dibv = vdupq_n_f32(dib);
        let wrv = vdupq_n_f32(wr);
        let wgv = vdupq_n_f32(wg);
        let wbv = vdupq_n_f32(wb);

        let mut total_errf = 0f32;
        for c in 0..MAX_CHUNKS {
            if c >= nchunks {
                break;
            }
            let d = vaddq_f32(
                vaddq_f32(vmulq_f32(farv, rv[c]), vmulq_f32(fagv, gv[c])),
                vmulq_f32(fabv, bv[c]),
            );
            let t1 = vaddq_f32(vmulq_f32(vsubq_f32(d, lowv), scalev), halfv);
            let s0 = vmulq_f32(vrndmq_f32(t1), invnv);
            let lt = vcltq_f32(s0, v0);
            let s1 = vbslq_f32(lt, v0, s0);
            let gt = vcgtq_f32(s0, onev);
            let s = vbslq_f32(gt, onev, s1);
            let itr = vaddq_f32(srv, vmulq_f32(dirv, s));
            let itg = vaddq_f32(sgv, vmulq_f32(digv, s));
            let itb = vaddq_f32(sbv, vmulq_f32(dibv, s));
            let dr = vsubq_f32(itr, rv[c]);
            let dg = vsubq_f32(itg, gv[c]);
            let db = vsubq_f32(itb, bv[c]);
            let term = vaddq_f32(
                vaddq_f32(
                    vmulq_f32(vmulq_f32(wrv, dr), dr),
                    vmulq_f32(vmulq_f32(wgv, dg), dg),
                ),
                vmulq_f32(vmulq_f32(wbv, db), db),
            );
            let cnt = (num_pixels - c * 4).min(4);
            sum4(term, cnt, &mut total_errf);
        }
        total_errf as i64 as u64
    }
}

pub(super) fn est_mode7_idx_neon(
    p: &CCParams,
    idxs: &[i32; 16],
    num_pixels: usize,
    lf: &LaneF32,
) -> u64 {
    if num_pixels == 0 {
        return 0;
    }
    unsafe {
        let rt = load_tab(&lf.r);
        let gt_ = load_tab(&lf.g);
        let bt = load_tab(&lf.b);
        let at = load_tab(&lf.a);
        let lane = vld1q_s32([0i32, 1, 2, 3].as_ptr());

        let nchunks = num_pixels.div_ceil(4);
        let mut rv = [vdupq_n_f32(0.0); 4];
        let mut gv = [vdupq_n_f32(0.0); 4];
        let mut bv = [vdupq_n_f32(0.0); 4];
        let mut av = [vdupq_n_f32(0.0); 4];

        let v255 = vdupq_n_f32(255.0);
        let v0 = vdupq_n_f32(0.0);
        let (mut minr, mut ming, mut minb, mut mina) = (v255, v255, v255, v255);
        let (mut maxr, mut maxg, mut maxb, mut maxa) = (v0, v0, v0, v0);
        for c in 0..MAX_CHUNKS {
            if c >= nchunks {
                break;
            }
            let cnt = (num_pixels - c * 4).min(4);
            let bidx = byte_idx(idxs.as_ptr().add(c * 4));
            rv[c] = tbl_gather(rt, bidx);
            gv[c] = tbl_gather(gt_, bidx);
            bv[c] = tbl_gather(bt, bidx);
            av[c] = tbl_gather(at, bidx);
            if cnt == 4 {
                minr = vminq_f32(minr, rv[c]);
                ming = vminq_f32(ming, gv[c]);
                minb = vminq_f32(minb, bv[c]);
                mina = vminq_f32(mina, av[c]);
                maxr = vmaxq_f32(maxr, rv[c]);
                maxg = vmaxq_f32(maxg, gv[c]);
                maxb = vmaxq_f32(maxb, bv[c]);
                maxa = vmaxq_f32(maxa, av[c]);
                continue;
            }
            let valid = vcgtq_s32(vdupq_n_s32(cnt as i32), lane);
            minr = vminq_f32(minr, vbslq_f32(valid, rv[c], v255));
            ming = vminq_f32(ming, vbslq_f32(valid, gv[c], v255));
            minb = vminq_f32(minb, vbslq_f32(valid, bv[c], v255));
            mina = vminq_f32(mina, vbslq_f32(valid, av[c], v255));
            maxr = vmaxq_f32(maxr, vbslq_f32(valid, rv[c], v0));
            maxg = vmaxq_f32(maxg, vbslq_f32(valid, gv[c], v0));
            maxb = vmaxq_f32(maxb, vbslq_f32(valid, bv[c], v0));
            maxa = vmaxq_f32(maxa, vbslq_f32(valid, av[c], v0));
        }
        let lr = vminvq_f32(minr);
        let lg = vminvq_f32(ming);
        let lb = vminvq_f32(minb);
        let la = vminvq_f32(mina);
        let hr = vmaxvq_f32(maxr);
        let hg = vmaxvq_f32(maxg);
        let hb = vmaxvq_f32(maxb);
        let ha = vmaxvq_f32(maxa);

        let n = 4f32;
        let (sr, sg, sb, sa) = (lr, lg, lb, la);
        let dir = hr - lr;
        let dig = hg - lg;
        let dib = hb - lb;
        let dia = ha - la;
        let (far, fag, fab, faa) = (dir, dig, dib, dia);
        let low = far * sr + fag * sg + fab * sb + faa * sa;
        let high = far * hr + fag * hg + fab * hb + faa * ha;
        let scale = (n - 1.0) / (high - low);
        let inv_n = 1.0 / (n - 1.0);

        let (wr, wg, wb, wa) = if !p.perceptual
            && (p.weights[0] != 1 || p.weights[1] != 1 || p.weights[2] != 1 || p.weights[3] != 1)
        {
            (
                p.weights[0] as f32,
                p.weights[1] as f32,
                p.weights[2] as f32,
                p.weights[3] as f32,
            )
        } else {
            (1.0, 1.0, 1.0, 1.0)
        };

        let farv = vdupq_n_f32(far);
        let fagv = vdupq_n_f32(fag);
        let fabv = vdupq_n_f32(fab);
        let faav = vdupq_n_f32(faa);
        let lowv = vdupq_n_f32(low);
        let scalev = vdupq_n_f32(scale);
        let halfv = vdupq_n_f32(0.5);
        let invnv = vdupq_n_f32(inv_n);
        let onev = vdupq_n_f32(1.0);
        let srv = vdupq_n_f32(sr);
        let sgv = vdupq_n_f32(sg);
        let sbv = vdupq_n_f32(sb);
        let sav = vdupq_n_f32(sa);
        let dirv = vdupq_n_f32(dir);
        let digv = vdupq_n_f32(dig);
        let dibv = vdupq_n_f32(dib);
        let diav = vdupq_n_f32(dia);
        let wrv = vdupq_n_f32(wr);
        let wgv = vdupq_n_f32(wg);
        let wbv = vdupq_n_f32(wb);
        let wav = vdupq_n_f32(wa);

        let mut total_errf = 0f32;
        for c in 0..MAX_CHUNKS {
            if c >= nchunks {
                break;
            }
            let d = vaddq_f32(
                vaddq_f32(
                    vaddq_f32(vmulq_f32(farv, rv[c]), vmulq_f32(fagv, gv[c])),
                    vmulq_f32(fabv, bv[c]),
                ),
                vmulq_f32(faav, av[c]),
            );
            let t1 = vaddq_f32(vmulq_f32(vsubq_f32(d, lowv), scalev), halfv);
            let s0 = vmulq_f32(vrndmq_f32(t1), invnv);
            let lt = vcltq_f32(s0, v0);
            let s1 = vbslq_f32(lt, v0, s0);
            let gt = vcgtq_f32(s0, onev);
            let s = vbslq_f32(gt, onev, s1);
            let dr = vsubq_f32(vaddq_f32(srv, vmulq_f32(dirv, s)), rv[c]);
            let dg = vsubq_f32(vaddq_f32(sgv, vmulq_f32(digv, s)), gv[c]);
            let db = vsubq_f32(vaddq_f32(sbv, vmulq_f32(dibv, s)), bv[c]);
            let da = vsubq_f32(vaddq_f32(sav, vmulq_f32(diav, s)), av[c]);
            let term = vaddq_f32(
                vaddq_f32(
                    vaddq_f32(
                        vmulq_f32(vmulq_f32(wrv, dr), dr),
                        vmulq_f32(vmulq_f32(wgv, dg), dg),
                    ),
                    vmulq_f32(vmulq_f32(wbv, db), db),
                ),
                vmulq_f32(vmulq_f32(wav, da), da),
            );
            let cnt = (num_pixels - c * 4).min(4);
            sum4(term, cnt, &mut total_errf);
        }
        total_errf as i64 as u64
    }
}

pub(super) fn qualified() -> bool {
    static Q: OnceLock<bool> = OnceLock::new();
    *Q.get_or_init(probe_matches_scalar)
}

fn xs(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn probe_matches_scalar() -> bool {
    if std::env::var_os("ABGEN_BC7_SCALAR").is_some() {
        return false;
    }
    let mut st = 0x9e3779b97f4a7c15u64;
    let weight_sets: [([u32; 4], bool); 4] = [
        ([1, 1, 1, 1], false),
        ([128, 64, 16, 256], true),
        ([128, 64, 16, 256], false),
        ([2, 3, 5, 7], false),
    ];
    let modes = [0usize, 1, 2, 3, 7];
    for case in 0..128usize {
        let mode = modes[case % modes.len()];
        let num_pixels = case % 16 + 1;
        let (weights, perceptual) = weight_sets[case % weight_sets.len()];
        let mut pixels = [ColorI::default(); 16];
        if case % 8 == 7 {
            let c = [
                (xs(&mut st) & 0xff) as i32,
                (xs(&mut st) & 0xff) as i32,
                (xs(&mut st) & 0xff) as i32,
                (xs(&mut st) & 0xff) as i32,
            ];
            pixels = [ColorI { c }; 16];
        } else {
            for px in pixels.iter_mut() {
                px.c = [
                    (xs(&mut st) & 0xff) as i32,
                    (xs(&mut st) & 0xff) as i32,
                    (xs(&mut st) & 0xff) as i32,
                    (xs(&mut st) & 0xff) as i32,
                ];
            }
        }
        let mut idxs = [0i32; 16];
        for (i, v) in idxs.iter_mut().enumerate() {
            *v = i as i32;
        }
        for i in (1..16usize).rev() {
            let j = (xs(&mut st) % (i as u64 + 1)) as usize;
            idxs.swap(i, j);
        }
        let mut p = CCParams::clear();
        p.weights = weights;
        p.perceptual = perceptual;
        let lf = LaneF32::new(&pixels);
        let ok = if mode == 7 {
            est_mode7_idx_neon(&p, &idxs, num_pixels, &lf)
                == ccc_est_mode7_idx_scalar(&p, &idxs, num_pixels, &pixels)
        } else {
            est_idx_neon(mode, &p, &idxs, num_pixels, &lf)
                == ccc_est_idx_scalar(mode, &p, &idxs, num_pixels, &pixels)
        };
        if !ok {
            return false;
        }
    }
    true
}

pub(super) struct PreRgbNeon {
    rt: uint8x16x4_t,
    gt: uint8x16x4_t,
    bt: uint8x16x4_t,
    wrv: float32x4_t,
    wgv: float32x4_t,
    wbv: float32x4_t,
    invnv: float32x4_t,
    nm1: f32,
}

unsafe fn pre_rgb_neon(mode: usize, p: &CCParams, lf: &LaneF32) -> PreRgbNeon {
    let n = 1u32 << G_COLOR_INDEX_BITCOUNT[mode];
    let nm1 = n as f32 - 1.0;
    let inv_n = 1.0 / nm1;
    let (wr, wg, wb) = if p.weights[0] != 1 || p.weights[1] != 1 || p.weights[2] != 1 {
        (
            p.weights[0] as f32,
            p.weights[1] as f32,
            p.weights[2] as f32,
        )
    } else {
        (1.0, 1.0, 1.0)
    };
    PreRgbNeon {
        rt: load_tab(&lf.r),
        gt: load_tab(&lf.g),
        bt: load_tab(&lf.b),
        wrv: vdupq_n_f32(wr),
        wgv: vdupq_n_f32(wg),
        wbv: vdupq_n_f32(wb),
        invnv: vdupq_n_f32(inv_n),
        nm1,
    }
}

pub(super) struct PreRgbaNeon {
    rt: uint8x16x4_t,
    gt: uint8x16x4_t,
    bt: uint8x16x4_t,
    at: uint8x16x4_t,
    wrv: float32x4_t,
    wgv: float32x4_t,
    wbv: float32x4_t,
    wav: float32x4_t,
    invnv: float32x4_t,
    nm1: f32,
}

unsafe fn pre_rgba_neon(p: &CCParams, lf: &LaneF32) -> PreRgbaNeon {
    let n = 4f32;
    let nm1 = n - 1.0;
    let inv_n = 1.0 / nm1;
    let (wr, wg, wb, wa) = if !p.perceptual
        && (p.weights[0] != 1 || p.weights[1] != 1 || p.weights[2] != 1 || p.weights[3] != 1)
    {
        (
            p.weights[0] as f32,
            p.weights[1] as f32,
            p.weights[2] as f32,
            p.weights[3] as f32,
        )
    } else {
        (1.0, 1.0, 1.0, 1.0)
    };
    PreRgbaNeon {
        rt: load_tab(&lf.r),
        gt: load_tab(&lf.g),
        bt: load_tab(&lf.b),
        at: load_tab(&lf.a),
        wrv: vdupq_n_f32(wr),
        wgv: vdupq_n_f32(wg),
        wbv: vdupq_n_f32(wb),
        wav: vdupq_n_f32(wa),
        invnv: vdupq_n_f32(inv_n),
        nm1,
    }
}

#[inline]
unsafe fn subset_err_rgb_pre(pre: &PreRgbNeon, idxs: &[i32; 16], num_pixels: usize) -> u64 {
    if num_pixels == 0 {
        return 0;
    }
    let lane = vld1q_s32([0i32, 1, 2, 3].as_ptr());
    let nchunks = num_pixels.div_ceil(4);
    let mut rv = [vdupq_n_f32(0.0); 4];
    let mut gv = [vdupq_n_f32(0.0); 4];
    let mut bv = [vdupq_n_f32(0.0); 4];

    let v255 = vdupq_n_f32(255.0);
    let v0 = vdupq_n_f32(0.0);
    let (mut minr, mut ming, mut minb) = (v255, v255, v255);
    let (mut maxr, mut maxg, mut maxb) = (v0, v0, v0);
    for c in 0..MAX_CHUNKS {
        if c >= nchunks {
            break;
        }
        let cnt = (num_pixels - c * 4).min(4);
        let bidx = byte_idx(idxs.as_ptr().add(c * 4));
        rv[c] = tbl_gather(pre.rt, bidx);
        gv[c] = tbl_gather(pre.gt, bidx);
        bv[c] = tbl_gather(pre.bt, bidx);
        if cnt == 4 {
            minr = vminq_f32(minr, rv[c]);
            ming = vminq_f32(ming, gv[c]);
            minb = vminq_f32(minb, bv[c]);
            maxr = vmaxq_f32(maxr, rv[c]);
            maxg = vmaxq_f32(maxg, gv[c]);
            maxb = vmaxq_f32(maxb, bv[c]);
        } else {
            let valid = vcgtq_s32(vdupq_n_s32(cnt as i32), lane);
            minr = vminq_f32(minr, vbslq_f32(valid, rv[c], v255));
            ming = vminq_f32(ming, vbslq_f32(valid, gv[c], v255));
            minb = vminq_f32(minb, vbslq_f32(valid, bv[c], v255));
            maxr = vmaxq_f32(maxr, vbslq_f32(valid, rv[c], v0));
            maxg = vmaxq_f32(maxg, vbslq_f32(valid, gv[c], v0));
            maxb = vmaxq_f32(maxb, vbslq_f32(valid, bv[c], v0));
        }
    }
    let lr = vminvq_f32(minr);
    let lg = vminvq_f32(ming);
    let lb = vminvq_f32(minb);
    let hr = vmaxvq_f32(maxr);
    let hg = vmaxvq_f32(maxg);
    let hb = vmaxvq_f32(maxb);

    let sr = lr;
    let sg = lg;
    let sb = lb;
    let dir = hr - lr;
    let dig = hg - lg;
    let dib = hb - lb;
    let far = dir;
    let fag = dig;
    let fab = dib;
    let low = far * sr + fag * sg + fab * sb;
    let high = far * hr + fag * hg + fab * hb;
    let scale = pre.nm1 / (high - low);

    let farv = vdupq_n_f32(far);
    let fagv = vdupq_n_f32(fag);
    let fabv = vdupq_n_f32(fab);
    let lowv = vdupq_n_f32(low);
    let scalev = vdupq_n_f32(scale);
    let halfv = vdupq_n_f32(0.5);
    let onev = vdupq_n_f32(1.0);
    let srv = vdupq_n_f32(sr);
    let sgv = vdupq_n_f32(sg);
    let sbv = vdupq_n_f32(sb);
    let dirv = vdupq_n_f32(dir);
    let digv = vdupq_n_f32(dig);
    let dibv = vdupq_n_f32(dib);

    let mut total_errf = 0f32;
    for c in 0..MAX_CHUNKS {
        if c >= nchunks {
            break;
        }
        let d = vaddq_f32(
            vaddq_f32(vmulq_f32(farv, rv[c]), vmulq_f32(fagv, gv[c])),
            vmulq_f32(fabv, bv[c]),
        );
        let t1 = vaddq_f32(vmulq_f32(vsubq_f32(d, lowv), scalev), halfv);
        let s0 = vmulq_f32(vrndmq_f32(t1), pre.invnv);
        let lt = vcltq_f32(s0, v0);
        let s1 = vbslq_f32(lt, v0, s0);
        let gt = vcgtq_f32(s0, onev);
        let s = vbslq_f32(gt, onev, s1);
        let itr = vaddq_f32(srv, vmulq_f32(dirv, s));
        let itg = vaddq_f32(sgv, vmulq_f32(digv, s));
        let itb = vaddq_f32(sbv, vmulq_f32(dibv, s));
        let dr = vsubq_f32(itr, rv[c]);
        let dg = vsubq_f32(itg, gv[c]);
        let db = vsubq_f32(itb, bv[c]);
        let term = vaddq_f32(
            vaddq_f32(
                vmulq_f32(vmulq_f32(pre.wrv, dr), dr),
                vmulq_f32(vmulq_f32(pre.wgv, dg), dg),
            ),
            vmulq_f32(vmulq_f32(pre.wbv, db), db),
        );
        let cnt = (num_pixels - c * 4).min(4);
        sum4(term, cnt, &mut total_errf);
    }
    total_errf as i64 as u64
}

#[inline]
unsafe fn subset_err_rgba_pre(pre: &PreRgbaNeon, idxs: &[i32; 16], num_pixels: usize) -> u64 {
    if num_pixels == 0 {
        return 0;
    }
    let lane = vld1q_s32([0i32, 1, 2, 3].as_ptr());
    let nchunks = num_pixels.div_ceil(4);
    let mut rv = [vdupq_n_f32(0.0); 4];
    let mut gv = [vdupq_n_f32(0.0); 4];
    let mut bv = [vdupq_n_f32(0.0); 4];
    let mut av = [vdupq_n_f32(0.0); 4];

    let v255 = vdupq_n_f32(255.0);
    let v0 = vdupq_n_f32(0.0);
    let (mut minr, mut ming, mut minb, mut mina) = (v255, v255, v255, v255);
    let (mut maxr, mut maxg, mut maxb, mut maxa) = (v0, v0, v0, v0);
    for c in 0..MAX_CHUNKS {
        if c >= nchunks {
            break;
        }
        let cnt = (num_pixels - c * 4).min(4);
        let bidx = byte_idx(idxs.as_ptr().add(c * 4));
        rv[c] = tbl_gather(pre.rt, bidx);
        gv[c] = tbl_gather(pre.gt, bidx);
        bv[c] = tbl_gather(pre.bt, bidx);
        av[c] = tbl_gather(pre.at, bidx);
        if cnt == 4 {
            minr = vminq_f32(minr, rv[c]);
            ming = vminq_f32(ming, gv[c]);
            minb = vminq_f32(minb, bv[c]);
            mina = vminq_f32(mina, av[c]);
            maxr = vmaxq_f32(maxr, rv[c]);
            maxg = vmaxq_f32(maxg, gv[c]);
            maxb = vmaxq_f32(maxb, bv[c]);
            maxa = vmaxq_f32(maxa, av[c]);
        } else {
            let valid = vcgtq_s32(vdupq_n_s32(cnt as i32), lane);
            minr = vminq_f32(minr, vbslq_f32(valid, rv[c], v255));
            ming = vminq_f32(ming, vbslq_f32(valid, gv[c], v255));
            minb = vminq_f32(minb, vbslq_f32(valid, bv[c], v255));
            mina = vminq_f32(mina, vbslq_f32(valid, av[c], v255));
            maxr = vmaxq_f32(maxr, vbslq_f32(valid, rv[c], v0));
            maxg = vmaxq_f32(maxg, vbslq_f32(valid, gv[c], v0));
            maxb = vmaxq_f32(maxb, vbslq_f32(valid, bv[c], v0));
            maxa = vmaxq_f32(maxa, vbslq_f32(valid, av[c], v0));
        }
    }
    let lr = vminvq_f32(minr);
    let lg = vminvq_f32(ming);
    let lb = vminvq_f32(minb);
    let la = vminvq_f32(mina);
    let hr = vmaxvq_f32(maxr);
    let hg = vmaxvq_f32(maxg);
    let hb = vmaxvq_f32(maxb);
    let ha = vmaxvq_f32(maxa);

    let (sr, sg, sb, sa) = (lr, lg, lb, la);
    let dir = hr - lr;
    let dig = hg - lg;
    let dib = hb - lb;
    let dia = ha - la;
    let (far, fag, fab, faa) = (dir, dig, dib, dia);
    let low = far * sr + fag * sg + fab * sb + faa * sa;
    let high = far * hr + fag * hg + fab * hb + faa * ha;
    let scale = pre.nm1 / (high - low);

    let farv = vdupq_n_f32(far);
    let fagv = vdupq_n_f32(fag);
    let fabv = vdupq_n_f32(fab);
    let faav = vdupq_n_f32(faa);
    let lowv = vdupq_n_f32(low);
    let scalev = vdupq_n_f32(scale);
    let halfv = vdupq_n_f32(0.5);
    let onev = vdupq_n_f32(1.0);
    let srv = vdupq_n_f32(sr);
    let sgv = vdupq_n_f32(sg);
    let sbv = vdupq_n_f32(sb);
    let sav = vdupq_n_f32(sa);
    let dirv = vdupq_n_f32(dir);
    let digv = vdupq_n_f32(dig);
    let dibv = vdupq_n_f32(dib);
    let diav = vdupq_n_f32(dia);

    let mut total_errf = 0f32;
    for c in 0..MAX_CHUNKS {
        if c >= nchunks {
            break;
        }
        let d = vaddq_f32(
            vaddq_f32(
                vaddq_f32(vmulq_f32(farv, rv[c]), vmulq_f32(fagv, gv[c])),
                vmulq_f32(fabv, bv[c]),
            ),
            vmulq_f32(faav, av[c]),
        );
        let t1 = vaddq_f32(vmulq_f32(vsubq_f32(d, lowv), scalev), halfv);
        let s0 = vmulq_f32(vrndmq_f32(t1), pre.invnv);
        let lt = vcltq_f32(s0, v0);
        let s1 = vbslq_f32(lt, v0, s0);
        let gt = vcgtq_f32(s0, onev);
        let s = vbslq_f32(gt, onev, s1);
        let dr = vsubq_f32(vaddq_f32(srv, vmulq_f32(dirv, s)), rv[c]);
        let dg = vsubq_f32(vaddq_f32(sgv, vmulq_f32(digv, s)), gv[c]);
        let db = vsubq_f32(vaddq_f32(sbv, vmulq_f32(dibv, s)), bv[c]);
        let da = vsubq_f32(vaddq_f32(sav, vmulq_f32(diav, s)), av[c]);
        let term = vaddq_f32(
            vaddq_f32(
                vaddq_f32(
                    vmulq_f32(vmulq_f32(pre.wrv, dr), dr),
                    vmulq_f32(vmulq_f32(pre.wgv, dg), dg),
                ),
                vmulq_f32(vmulq_f32(pre.wbv, db), db),
            ),
            vmulq_f32(vmulq_f32(pre.wav, da), da),
        );
        let cnt = (num_pixels - c * 4).min(4);
        sum4(term, cnt, &mut total_errf);
    }
    total_errf as i64 as u64
}

pub(super) fn est_partition_lane_neon(
    mode: usize,
    p: &CCParams,
    lf: &LaneF32,
    tab: &[SubsetIdx; 64],
    total_partitions: u32,
    total_subsets: usize,
) -> u32 {
    debug_assert!(mode != 7);
    unsafe {
        let pre = pre_rgb_neon(mode, p, lf);
        let mut best_err = u64::MAX;
        let mut best_partition = 0u32;
        for partition in 0..total_partitions {
            let si = &tab[partition as usize];
            let mut total_subset_err = 0u64;
            for subset in 0..total_subsets {
                let err = subset_err_rgb_pre(&pre, &si.idx[subset], si.total[subset]);
                total_subset_err += err;
                if total_subset_err >= best_err {
                    break;
                }
            }
            if total_subset_err < best_err {
                best_err = total_subset_err;
                best_partition = partition;
                if best_err == 0 {
                    break;
                }
            }
            if total_subsets == 2
                && partition as usize == BC7E_2SUBSET_CHECKERBOARD_PARTITION_INDEX
                && best_partition as usize != BC7E_2SUBSET_CHECKERBOARD_PARTITION_INDEX
            {
                break;
            }
        }
        best_partition
    }
}

pub(super) fn est_partition_list_lane_neon(
    mode: usize,
    p: &CCParams,
    lf: &LaneF32,
    tab: &[SubsetIdx; 64],
    part_lo: u32,
    part_hi: u32,
    total_subsets: usize,
    solutions: &mut [Solution],
    num_solutions: &mut i32,
    max_solutions: i32,
) -> i32 {
    enum Pre {
        Rgb(PreRgbNeon),
        Rgba(PreRgbaNeon),
    }
    unsafe {
        let pre = if mode == 7 {
            Pre::Rgba(pre_rgba_neon(p, lf))
        } else {
            Pre::Rgb(pre_rgb_neon(mode, p, lf))
        };
        let mut i_at = 0i32;
        for partition in part_lo..part_hi {
            let si = &tab[partition as usize];
            let full = *num_solutions == max_solutions;
            let thresh = if full {
                solutions[(max_solutions - 1) as usize].err
            } else {
                u64::MAX
            };
            let mut total_subset_err = 0u64;
            let mut pruned = false;
            for subset in 0..total_subsets {
                let err = match &pre {
                    Pre::Rgba(pre) => subset_err_rgba_pre(pre, &si.idx[subset], si.total[subset]),
                    Pre::Rgb(pre) => subset_err_rgb_pre(pre, &si.idx[subset], si.total[subset]),
                };
                total_subset_err += err;
                if total_subset_err >= thresh {
                    pruned = true;
                    break;
                }
            }
            if pruned {
                i_at = *num_solutions;
                continue;
            }
            let mut i = 0i32;
            while i < *num_solutions {
                if total_subset_err < solutions[i as usize].err {
                    break;
                }
                i += 1;
            }
            if i < *num_solutions {
                let mut solutions_to_move = (max_solutions - 1) - i;
                let num_elements_at_i = *num_solutions - i;
                if solutions_to_move > num_elements_at_i {
                    solutions_to_move = num_elements_at_i;
                }
                let mut j = solutions_to_move - 1;
                while j >= 0 {
                    solutions[(i + j + 1) as usize] = solutions[(i + j) as usize];
                    j -= 1;
                }
            }
            if *num_solutions < max_solutions {
                *num_solutions += 1;
            }
            if i < *num_solutions {
                solutions[i as usize].err = total_subset_err;
                solutions[i as usize].index = partition;
            }
            i_at = i;
        }
        i_at
    }
}

#[cfg(test)]
mod neon_parity_tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next_u32(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            (x >> 32) as u32
        }
    }

    const WEIGHT_SETS: [([u32; 4], bool); 4] = [
        ([1, 1, 1, 1], false),
        ([128, 64, 16, 256], true),
        ([128, 64, 16, 256], false),
        ([2, 3, 5, 7], false),
    ];

    fn params(rng: &mut Rng) -> CCParams {
        let (weights, perceptual) = WEIGHT_SETS[(rng.next_u32() % 4) as usize];
        let mut p = CCParams::clear();
        p.weights = weights;
        p.perceptual = perceptual;
        p
    }

    fn gen_pixels(rng: &mut Rng) -> [ColorI; 16] {
        let mut pixels = [ColorI::default(); 16];
        match rng.next_u32() % 4 {
            0 => {
                let c = [
                    (rng.next_u32() % 256) as i32,
                    (rng.next_u32() % 256) as i32,
                    (rng.next_u32() % 256) as i32,
                    (rng.next_u32() % 256) as i32,
                ];
                pixels = [ColorI { c }; 16];
            }
            1 => {
                for px in pixels.iter_mut() {
                    for v in px.c.iter_mut() {
                        *v = if rng.next_u32().is_multiple_of(2) {
                            0
                        } else {
                            255
                        };
                    }
                }
            }
            _ => {
                for px in pixels.iter_mut() {
                    for v in px.c.iter_mut() {
                        *v = (rng.next_u32() % 256) as i32;
                    }
                }
            }
        }
        pixels
    }

    fn gen_idxs(rng: &mut Rng) -> [i32; 16] {
        let mut idxs = [0i32; 16];
        for (i, v) in idxs.iter_mut().enumerate() {
            *v = i as i32;
        }
        for i in (1..16usize).rev() {
            let j = (rng.next_u32() as usize) % (i + 1);
            idxs.swap(i, j);
        }
        idxs
    }

    #[test]
    fn est_idx_neon_matches_scalar() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for case in 0..4000usize {
            let mode = (rng.next_u32() % 4) as usize;
            let num_pixels = case % 16 + 1;
            let p = params(&mut rng);
            let pixels = gen_pixels(&mut rng);
            let idxs = gen_idxs(&mut rng);
            let lf = LaneF32::new(&pixels);
            assert_eq!(
                est_idx_neon(mode, &p, &idxs, num_pixels, &lf),
                ccc_est_idx_scalar(mode, &p, &idxs, num_pixels, &pixels),
                "mode={mode} n={num_pixels} w={:?} pixels={:?} idxs={idxs:?}",
                p.weights,
                pixels.map(|px| px.c)
            );
        }
    }

    #[test]
    fn est_mode7_idx_neon_matches_scalar() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..4000usize {
            let num_pixels = case % 16 + 1;
            let p = params(&mut rng);
            let pixels = gen_pixels(&mut rng);
            let idxs = gen_idxs(&mut rng);
            let lf = LaneF32::new(&pixels);
            assert_eq!(
                est_mode7_idx_neon(&p, &idxs, num_pixels, &lf),
                ccc_est_mode7_idx_scalar(&p, &idxs, num_pixels, &pixels),
                "n={num_pixels} w={:?} perceptual={} pixels={:?} idxs={idxs:?}",
                p.weights,
                p.perceptual,
                pixels.map(|px| px.c)
            );
        }
    }

    #[test]
    fn subset_err_rgb_pre_matches_scalar() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for _ in 0..500usize {
            let mode = (rng.next_u32() % 4) as usize;
            let p = params(&mut rng);
            let pixels = gen_pixels(&mut rng);
            let lf = LaneF32::new(&pixels);
            let pre = unsafe { pre_rgb_neon(mode, &p, &lf) };
            for num_pixels in 1..=16usize {
                let idxs = gen_idxs(&mut rng);
                assert_eq!(
                    unsafe { subset_err_rgb_pre(&pre, &idxs, num_pixels) },
                    ccc_est_idx_scalar(mode, &p, &idxs, num_pixels, &pixels),
                    "mode={mode} n={num_pixels} w={:?} pixels={:?} idxs={idxs:?}",
                    p.weights,
                    pixels.map(|px| px.c)
                );
            }
        }
    }

    #[test]
    fn subset_err_rgba_pre_matches_scalar() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        for _ in 0..500usize {
            let p = params(&mut rng);
            let pixels = gen_pixels(&mut rng);
            let lf = LaneF32::new(&pixels);
            let pre = unsafe { pre_rgba_neon(&p, &lf) };
            for num_pixels in 1..=16usize {
                let idxs = gen_idxs(&mut rng);
                assert_eq!(
                    unsafe { subset_err_rgba_pre(&pre, &idxs, num_pixels) },
                    ccc_est_mode7_idx_scalar(&p, &idxs, num_pixels, &pixels),
                    "n={num_pixels} w={:?} perceptual={} pixels={:?} idxs={idxs:?}",
                    p.weights,
                    p.perceptual,
                    pixels.map(|px| px.c)
                );
            }
        }
    }
}
