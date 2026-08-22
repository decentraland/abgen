use crate::bc7_pure::{linear_to_srgb_u8, srgb_to_linear_u8};

const C: usize = 4;

const SRGB_LIN_F64: [f64; 256] = {
    let mut t = [0f64; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = srgb_to_linear_u8(i as u8) as f64;
        i += 1;
    }
    t
};

const ALPHA_DIV255_F64: [f64; 256] = {
    let mut t = [0f64; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = i as f64 / 255.0;
        i += 1;
    }
    t
};

#[inline]
fn cubic_bc(x: f64, b: f64, c: f64) -> f64 {
    let x = x.abs();
    if x < 1.0 {
        ((12.0 - 9.0 * b - 6.0 * c) * x * x * x
            + (-18.0 + 12.0 * b + 6.0 * c) * x * x
            + (6.0 - 2.0 * b))
            / 6.0
    } else if x < 2.0 {
        ((-b - 6.0 * c) * x * x * x
            + (6.0 * b + 30.0 * c) * x * x
            + (-12.0 * b - 48.0 * c) * x
            + (8.0 * b + 24.0 * c))
            / 6.0
    } else {
        0.0
    }
}

struct AxisPlan {
    taps: Vec<Vec<(usize, f64)>>,
}

fn bspline_plan(n: usize, m: usize) -> AxisPlan {
    let ratio = n as f64 / m as f64;
    let support = 2.0;
    let mut taps = Vec::with_capacity(m);
    for d in 0..m {
        let center = (d as f64 + 0.5) * ratio - 0.5;
        let lo = (center - support).floor() as i64;
        let hi = (center + support).ceil() as i64;
        let mut row = Vec::with_capacity((hi - lo + 1) as usize);
        let mut p = lo;
        while p <= hi {
            let w = cubic_bc(center - p as f64, 1.0, 0.0);
            if w != 0.0 {
                let pc = p.clamp(0, n as i64 - 1) as usize;
                row.push((pc, w));
            }
            p += 1;
        }
        taps.push(row);
    }
    AxisPlan { taps }
}

fn area_plan(n: usize, m: usize) -> AxisPlan {
    let ratio = n as f64 / m as f64;
    let mut taps = Vec::with_capacity(m);
    for d in 0..m {
        let lo = d as f64 * ratio;
        let hi = ((d + 1) as f64 * ratio).min(n as f64);
        let first = lo.floor() as usize;
        let last = (hi.ceil() as usize).min(n) - 1;
        let mut row = Vec::with_capacity(last - first + 1);
        for p in first..=last {
            let w = ((p + 1) as f64).min(hi) - (p as f64).max(lo);
            if w > 0.0 {
                row.push((p, w));
            }
        }
        taps.push(row);
    }
    AxisPlan { taps }
}

fn plan_for_axis(n: usize, m: usize) -> AxisPlan {
    if m < n {
        area_plan(n, m)
    } else {
        bspline_plan(n, m)
    }
}

/// Accumulate one output pixel (4 f64 channels) from `taps`, matching the
/// scalar `acc[ch] += w * src[o + ch]; out = acc / wsum` order exactly.
/// The NEON path uses separate mul + add (never fma) so each lane performs
/// the same IEEE operation sequence as the scalar fallback: bit-identical.
#[cfg(target_arch = "aarch64")]
#[inline]
fn accum_taps_4(
    src: &[f64],
    taps: &[(usize, f64)],
    off_scale: usize,
    off_base: usize,
    out: &mut [f64],
    out_off: usize,
) {
    use std::arch::aarch64::*;
    debug_assert!(out_off + C <= out.len());
    unsafe {
        let mut acc0 = vdupq_n_f64(0.0);
        let mut acc1 = vdupq_n_f64(0.0);
        let mut wsum = 0f64;
        for &(s, w) in taps {
            let o = off_base + s * off_scale;
            debug_assert!(o + C <= src.len());
            let p = src.as_ptr().add(o);
            let wv = vdupq_n_f64(w);
            acc0 = vaddq_f64(acc0, vmulq_f64(wv, vld1q_f64(p)));
            acc1 = vaddq_f64(acc1, vmulq_f64(wv, vld1q_f64(p.add(2))));
            wsum += w;
        }
        let wv = vdupq_n_f64(wsum);
        let op = out.as_mut_ptr().add(out_off);
        vst1q_f64(op, vdivq_f64(acc0, wv));
        vst1q_f64(op.add(2), vdivq_f64(acc1, wv));
    }
}

#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn accum_taps_4(
    src: &[f64],
    taps: &[(usize, f64)],
    off_scale: usize,
    off_base: usize,
    out: &mut [f64],
    out_off: usize,
) {
    let mut acc = [0f64; C];
    let mut wsum = 0f64;
    for &(s, w) in taps {
        let o = off_base + s * off_scale;
        for ch in 0..C {
            acc[ch] += w * src[o + ch];
        }
        wsum += w;
    }
    for ch in 0..C {
        out[out_off + ch] = acc[ch] / wsum;
    }
}

fn resample_axes(work: &[f64], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f64> {
    let hplan = if dw == sw {
        None
    } else {
        Some(plan_for_axis(sw, dw))
    };
    let mut inter = vec![0f64; sh * dw * C];
    match &hplan {
        None => inter.copy_from_slice(work),
        Some(plan) => {
            let src_rs = sw * C;
            let dst_rs = dw * C;
            for y in 0..sh {
                let srow = y * src_rs;
                let drow = y * dst_rs;
                for (x, taps) in plan.taps.iter().enumerate() {
                    accum_taps_4(work, taps, C, srow, &mut inter, drow + x * C);
                }
            }
        }
    }

    let vplan = if dh == sh {
        None
    } else {
        Some(plan_for_axis(sh, dh))
    };
    let Some(plan) = &vplan else {
        return inter;
    };
    let mut out = vec![0f64; dh * dw * C];
    let inter_rs = dw * C;
    for (y, taps) in plan.taps.iter().enumerate() {
        let orow = &mut out[y * inter_rs..(y + 1) * inter_rs];
        let mut wsum = 0f64;
        for &(sy, w) in taps {
            let srow = &inter[sy * inter_rs..(sy + 1) * inter_rs];
            for (a, &s) in orow.iter_mut().zip(srow) {
                *a += w * s;
            }
            wsum += w;
        }
        for a in orow.iter_mut() {
            *a /= wsum;
        }
    }
    out
}

/// Convert one row of `sw` RGBA pixels from `u8` to `f64`, exactly like the
/// whole-image conversion `box_downscale_rgba` used to do up front — same
/// per-channel math, just scoped to a single row so it can be fused into
/// that row's horizontal resample pass.
fn row_to_f64(srow: &[u8], srgb: bool) -> Vec<f64> {
    let mut row = vec![0f64; srow.len()];
    if srgb {
        for (px, wp) in srow.chunks_exact(C).zip(row.chunks_exact_mut(C)) {
            wp[0] = SRGB_LIN_F64[px[0] as usize];
            wp[1] = SRGB_LIN_F64[px[1] as usize];
            wp[2] = SRGB_LIN_F64[px[2] as usize];
            wp[3] = px[3] as f64;
        }
    } else {
        for (d, &v) in row.iter_mut().zip(srow.iter()) {
            *d = v as f64;
        }
    }
    row
}

pub fn box_downscale_rgba(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    srgb: bool,
) -> Vec<u8> {
    use rayon::prelude::*;

    debug_assert_eq!(src.len(), sw * sh * C);
    if (sw, sh) == (dw, dh) {
        return src.to_vec();
    }

    // Horizontal pass, fused with the u8->f64 conversion: each output row
    // depends only on the matching source row, so both the conversion and
    // the horizontal resample for that row run together, across rows in
    // parallel, instead of materializing a whole-image f64 copy up front.
    let hplan = if dw == sw {
        None
    } else {
        Some(plan_for_axis(sw, dw))
    };
    let src_rs = sw * C;
    let dst_rs = dw * C;
    let mut inter = vec![0f64; sh * dw * C];
    inter
        .par_chunks_mut(dst_rs)
        .enumerate()
        .for_each(|(y, orow)| {
            let srow = &src[y * src_rs..(y + 1) * src_rs];
            let row_f64 = row_to_f64(srow, srgb);
            match &hplan {
                None => orow.copy_from_slice(&row_f64),
                Some(plan) => {
                    for (x, taps) in plan.taps.iter().enumerate() {
                        accum_taps_4(&row_f64, taps, C, 0, orow, x * C);
                    }
                }
            }
        });

    // Vertical pass: each output row is an independent weighted sum of
    // source rows, so it also runs row-parallel; the per-row accumulation
    // order (taps in plan order, channels 0..C) is unchanged.
    let vplan = if dh == sh {
        None
    } else {
        Some(plan_for_axis(sh, dh))
    };
    let fin = match &vplan {
        None => inter,
        Some(plan) => {
            let inter_rs = dw * C;
            let mut out = vec![0f64; dh * dw * C];
            out.par_chunks_mut(inter_rs)
                .zip(plan.taps.par_iter())
                .for_each(|(orow, taps)| {
                    let mut wsum = 0f64;
                    for &(sy, w) in taps {
                        let srow = &inter[sy * inter_rs..(sy + 1) * inter_rs];
                        for (a, &s) in orow.iter_mut().zip(srow) {
                            *a += w * s;
                        }
                        wsum += w;
                    }
                    for a in orow.iter_mut() {
                        *a /= wsum;
                    }
                });
            out
        }
    };

    let mut out = vec![0u8; dh * dw * C];
    if srgb {
        out.par_chunks_mut(C)
            .zip(fin.par_chunks(C))
            .for_each(|(px, fp)| {
                px[0] = linear_to_srgb_u8(fp[0] as f32);
                px[1] = linear_to_srgb_u8(fp[1] as f32);
                px[2] = linear_to_srgb_u8(fp[2] as f32);
                px[3] = fp[3].round().clamp(0.0, 255.0) as u8;
            });
    } else {
        out.par_iter_mut().zip(fin.par_iter()).for_each(|(d, &v)| {
            *d = v.round().clamp(0.0, 255.0) as u8;
        });
    }
    out
}

const PREMUL_ALPHA_EPS: f64 = 1e-8;

pub fn premul_downscale_rgba(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    debug_assert_eq!(src.len(), sw * sh * C);
    if (sw, sh) == (dw, dh) {
        return src.to_vec();
    }

    let mut work = vec![0f64; sh * sw * C];
    for (px, wp) in src.chunks_exact(C).zip(work.chunks_exact_mut(C)) {
        let a = ALPHA_DIV255_F64[px[3] as usize];
        wp[0] = SRGB_LIN_F64[px[0] as usize] * a;
        wp[1] = SRGB_LIN_F64[px[1] as usize] * a;
        wp[2] = SRGB_LIN_F64[px[2] as usize] * a;
        wp[3] = a;
    }
    let fin = resample_axes(&work, sw, sh, dw, dh);

    let mut out = vec![0u8; dh * dw * C];
    for (px, fp) in out.chunks_exact_mut(C).zip(fin.chunks_exact(C)) {
        let a = fp[3];
        for ch in 0..3 {
            let lin = if a > PREMUL_ALPHA_EPS {
                (fp[ch] / a).clamp(0.0, 1.0)
            } else {
                0.0
            };
            px[ch] = linear_to_srgb_u8(lin as f32);
        }
        px[3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
    }
    out
}

#[cfg(test)]
fn box_downscale_rgba_reference(
    src: &[u8],
    sw: usize,
    sh: usize,
    dw: usize,
    dh: usize,
    srgb: bool,
) -> Vec<u8> {
    if (sw, sh) == (dw, dh) {
        return src.to_vec();
    }
    let mut work = vec![0f64; sh * sw * C];
    if srgb {
        for (px, wp) in src.chunks_exact(C).zip(work.chunks_exact_mut(C)) {
            wp[0] = SRGB_LIN_F64[px[0] as usize];
            wp[1] = SRGB_LIN_F64[px[1] as usize];
            wp[2] = SRGB_LIN_F64[px[2] as usize];
            wp[3] = px[3] as f64;
        }
    } else {
        for (d, &v) in work.iter_mut().zip(src.iter()) {
            *d = v as f64;
        }
    }
    let fin = resample_axes(&work, sw, sh, dw, dh);
    let mut out = vec![0u8; dh * dw * C];
    if srgb {
        for (px, fp) in out.chunks_exact_mut(C).zip(fin.chunks_exact(C)) {
            px[0] = linear_to_srgb_u8(fp[0] as f32);
            px[1] = linear_to_srgb_u8(fp[1] as f32);
            px[2] = linear_to_srgb_u8(fp[2] as f32);
            px[3] = fp[3].round().clamp(0.0, 255.0) as u8;
        }
    } else {
        for (d, &v) in out.iter_mut().zip(fin.iter()) {
            *d = v.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift64(s: &mut u64) -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    }

    /// A/B against the pre-parallelization implementation (kept here as
    /// `box_downscale_rgba_reference`) over a battery of non-trivial sizes,
    /// including odd/1-wide/1-tall shapes: the fused, row-parallel version
    /// must be byte-for-byte identical.
    #[test]
    fn box_downscale_matches_reference_impl() {
        let mut s = 0x1234_5678_9abc_def1u64;
        let cases: [(usize, usize, usize, usize); 8] = [
            (37, 53, 17, 23),
            (64, 64, 32, 32),
            (128, 32, 40, 40),
            (200, 1, 50, 1),
            (1, 200, 1, 50),
            (300, 200, 300, 200),
            (5, 5, 5, 5),
            (9, 7, 3, 11),
        ];
        for &(sw, sh, dw, dh) in &cases {
            let n = sw * sh * C;
            let src: Vec<u8> = (0..n).map(|_| (xorshift64(&mut s) & 0xff) as u8).collect();
            for srgb in [false, true] {
                let got = box_downscale_rgba(&src, sw, sh, dw, dh, srgb);
                let want = box_downscale_rgba_reference(&src, sw, sh, dw, dh, srgb);
                assert_eq!(
                    got, want,
                    "{sw}x{sh}->{dw}x{dh} srgb={srgb}: fused/parallel path diverged"
                );
            }
        }
    }

    #[test]
    fn linear_domain_average() {
        let src = [
            255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255,
        ];
        let out = box_downscale_rgba(&src, 2, 2, 1, 1, true);
        assert_eq!(&out[..3], &[188, 188, 188]);
        assert_eq!(out[3], 128);
    }

    #[test]
    fn byte_domain_average_when_not_srgb() {
        let src = [
            255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255,
        ];
        let out = box_downscale_rgba(&src, 2, 2, 1, 1, false);
        assert_eq!(out, [128, 128, 128, 128]);
    }

    #[test]
    fn area_weights_exact() {
        let mut src = vec![0u8; 3 * C];
        for (y, v) in [30u8, 90, 150].into_iter().enumerate() {
            for ch in 0..3 {
                src[y * C + ch] = v;
            }
            src[y * C + 3] = 255;
        }
        let out = box_downscale_rgba(&src, 1, 3, 1, 2, false);
        assert_eq!(out[0], 50);
        assert_eq!(out[C], 130);
        assert_eq!(out[3], 255);
        assert_eq!(out[C + 3], 255);
    }

    fn step_col(edge: usize) -> Vec<u8> {
        let mut src = vec![0u8; 2 * 300 * C];
        for y in 0..300 {
            let v = if y < edge { 0 } else { 255 };
            for x in 0..2 {
                let o = (y * 2 + x) * C;
                src[o] = v;
                src[o + 1] = v;
                src[o + 2] = v;
                src[o + 3] = 255;
            }
        }
        let out = box_downscale_rgba(&src, 2, 300, 2, 256, true);
        (0..256).map(|y| out[(y * 2) * C]).collect()
    }

    #[test]
    fn step_edge_linear_no_overshoot() {
        let col = step_col(151);
        assert_eq!(col[127], 0);
        assert_eq!(col[128], 107);
        assert_eq!(col[129], 255);
        assert!(col.windows(2).all(|w| w[1] >= w[0]));

        let aligned = step_col(150);
        assert_eq!(aligned[127], 0);
        assert_eq!(aligned[128], 255);
    }

    #[test]
    fn upscale_bspline_byte_domain_unchanged() {
        let n_src = 200;
        let mut src = vec![0u8; 2 * n_src * C];
        for x in 0..2 {
            let o = (100 * 2 + x) * C;
            src[o] = 255;
            src[o + 1] = 255;
            src[o + 2] = 255;
        }
        for y in 0..n_src {
            for x in 0..2 {
                src[(y * 2 + x) * C + 3] = 255;
            }
        }
        let out = box_downscale_rgba(&src, 2, n_src, 2, 256, false);
        let col: Vec<u8> = (0..256).map(|y| out[(y * 2) * C]).collect();
        assert_eq!(col[126], 2);
        assert_eq!(col[127], 58);
        assert_eq!(col[128], 167);
        assert_eq!(col[129], 94);
        assert_eq!(col[130], 7);
    }

    #[test]
    fn identity_passthrough() {
        let src = vec![7u8; 4 * 4 * C];
        let out = box_downscale_rgba(&src, 4, 4, 4, 4, true);
        assert_eq!(out, src);
    }

    #[test]
    fn premul_edge_no_rgb_drag() {
        let (sw, sh) = (8usize, 8usize);
        let mut src = vec![0u8; sw * sh * C];
        for y in 0..sh {
            for x in 0..sw {
                let o = (y * sw + x) * C;
                if x < 3 {
                    src[o] = 255;
                    src[o + 3] = 255;
                } else {
                    src[o + 1] = 255;
                }
            }
        }
        let premul = premul_downscale_rgba(&src, sw, sh, 4, 4);
        for px in premul.chunks_exact(C) {
            if px[3] > 0 {
                assert_eq!(px[0], 255, "visible red thinned: {px:?}");
                assert_eq!(px[1], 0, "hidden green dragged in: {px:?}");
            } else {
                assert_eq!(&px[0..3], &[0, 0, 0], "uncovered texel not zeroed: {px:?}");
            }
        }
        let straight = box_downscale_rgba(&src, sw, sh, 4, 4, true);
        let dragged = straight.chunks_exact(C).any(|px| px[3] > 0 && px[1] > 0);
        assert!(dragged, "straight filter no longer drags; fixture stale");
    }

    #[test]
    fn premul_matches_straight_when_fully_opaque() {
        let (sw, sh) = (7usize, 5usize);
        let mut src = vec![0u8; sw * sh * C];
        for (i, px) in src.chunks_exact_mut(C).enumerate() {
            px[0] = (i * 37 % 256) as u8;
            px[1] = (i * 101 % 256) as u8;
            px[2] = (i * 197 % 256) as u8;
            px[3] = 255;
        }
        assert_eq!(
            premul_downscale_rgba(&src, sw, sh, 3, 2),
            box_downscale_rgba(&src, sw, sh, 3, 2, true)
        );
    }
}

#[cfg(test)]
mod parity_tests {
    //! Byte-for-byte parity of the optimized pipeline against a verbatim copy
    //! of the original scalar implementation.
    use super::{box_downscale_rgba, premul_downscale_rgba, C};
    use crate::bc7_pure::srgb_to_linear_u8;

    mod reference {
        use super::super::{plan_for_axis, C, PREMUL_ALPHA_EPS};
        use crate::bc7_pure::{linear_to_srgb_u8, srgb_to_linear_u8};

        fn resample_axes(work: &[f64], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<f64> {
            let hplan = if dw == sw {
                None
            } else {
                Some(plan_for_axis(sw, dw))
            };
            let mut inter = vec![0f64; sh * dw * C];
            match &hplan {
                None => inter.copy_from_slice(work),
                Some(plan) => {
                    let src_rs = sw * C;
                    let dst_rs = dw * C;
                    for y in 0..sh {
                        let srow = y * src_rs;
                        let drow = y * dst_rs;
                        for (x, taps) in plan.taps.iter().enumerate() {
                            let mut acc = [0f64; C];
                            let mut wsum = 0f64;
                            for &(sx, w) in taps {
                                let o = srow + sx * C;
                                for ch in 0..C {
                                    acc[ch] += w * work[o + ch];
                                }
                                wsum += w;
                            }
                            let o = drow + x * C;
                            for ch in 0..C {
                                inter[o + ch] = acc[ch] / wsum;
                            }
                        }
                    }
                }
            }

            let vplan = if dh == sh {
                None
            } else {
                Some(plan_for_axis(sh, dh))
            };
            let Some(plan) = &vplan else {
                return inter;
            };
            let mut out = vec![0f64; dh * dw * C];
            let inter_rs = dw * C;
            for (y, taps) in plan.taps.iter().enumerate() {
                let drow = y * inter_rs;
                for x in 0..dw {
                    let mut acc = [0f64; C];
                    let mut wsum = 0f64;
                    for &(sy, w) in taps {
                        let o = sy * inter_rs + x * C;
                        for ch in 0..C {
                            acc[ch] += w * inter[o + ch];
                        }
                        wsum += w;
                    }
                    let o = drow + x * C;
                    for ch in 0..C {
                        out[o + ch] = acc[ch] / wsum;
                    }
                }
            }
            out
        }

        pub fn box_downscale_rgba(
            src: &[u8],
            sw: usize,
            sh: usize,
            dw: usize,
            dh: usize,
            srgb: bool,
        ) -> Vec<u8> {
            debug_assert_eq!(src.len(), sw * sh * C);
            if (sw, sh) == (dw, dh) {
                return src.to_vec();
            }

            let mut work = vec![0f64; sh * sw * C];
            for (i, &v) in src.iter().enumerate() {
                work[i] = if srgb && i % C != 3 {
                    srgb_to_linear_u8(v) as f64
                } else {
                    v as f64
                };
            }
            let fin = resample_axes(&work, sw, sh, dw, dh);

            let mut out = vec![0u8; dh * dw * C];
            for (i, &v) in fin.iter().enumerate() {
                out[i] = if srgb && i % C != 3 {
                    linear_to_srgb_u8(v as f32)
                } else {
                    v.round().clamp(0.0, 255.0) as u8
                };
            }
            out
        }

        pub fn premul_downscale_rgba(
            src: &[u8],
            sw: usize,
            sh: usize,
            dw: usize,
            dh: usize,
        ) -> Vec<u8> {
            debug_assert_eq!(src.len(), sw * sh * C);
            if (sw, sh) == (dw, dh) {
                return src.to_vec();
            }

            let mut work = vec![0f64; sh * sw * C];
            for (px, wp) in src.chunks_exact(C).zip(work.chunks_exact_mut(C)) {
                let a = px[3] as f64 / 255.0;
                for ch in 0..3 {
                    wp[ch] = srgb_to_linear_u8(px[ch]) as f64 * a;
                }
                wp[3] = a;
            }
            let fin = resample_axes(&work, sw, sh, dw, dh);

            let mut out = vec![0u8; dh * dw * C];
            for (px, fp) in out.chunks_exact_mut(C).zip(fin.chunks_exact(C)) {
                let a = fp[3];
                for ch in 0..3 {
                    let lin = if a > PREMUL_ALPHA_EPS {
                        (fp[ch] / a).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    px[ch] = linear_to_srgb_u8(lin as f32);
                }
                px[3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            out
        }
    }

    fn xorshift(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    fn random_rgba(w: usize, h: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..w * h * C)
            .map(|_| (xorshift(&mut s) & 0xFF) as u8)
            .collect()
    }

    fn corpus() -> Vec<(Vec<u8>, usize, usize)> {
        let mut cases = Vec::new();
        for &(w, h, seed) in &[
            (16usize, 16usize, 0x1234_5678u64),
            (17, 13, 0xDEAD_BEEF),
            (64, 32, 0xA5A5_5A5A),
            (33, 47, 0x0BAD_F00D),
        ] {
            cases.push((random_rgba(w, h, seed), w, h));
        }
        cases.push((vec![0u8; 16 * 16 * C], 16, 16));
        cases.push((vec![255u8; 16 * 16 * C], 16, 16));
        cases.push((vec![128u8; 12 * 20 * C], 12, 20));
        let (w, h) = (24usize, 24usize);
        let mut alpha_edge = random_rgba(w, h, 0xFEED_FACE);
        for (i, px) in alpha_edge.chunks_exact_mut(C).enumerate() {
            let x = i % w;
            px[3] = if x < w / 2 { 255 } else { 0 };
        }
        cases.push((alpha_edge, w, h));
        let mut low_alpha = random_rgba(w, h, 0xC0FF_EE00);
        for px in low_alpha.chunks_exact_mut(C) {
            px[3] %= 3;
        }
        cases.push((low_alpha, w, h));
        cases
    }

    #[test]
    fn optimized_matches_reference_bytes() {
        for (src, w, h) in corpus() {
            let dims: Vec<(usize, usize)> = vec![
                (w / 2, h / 2),
                ((w * 2).max(1), h * 2),
                (w / 2, h * 2),
                (w, h / 2),
                (w / 2, h),
                (w.max(3) - 2, h + 3),
            ];
            for (dw, dh) in dims {
                if dw == 0 || dh == 0 {
                    continue;
                }
                for srgb in [false, true] {
                    assert_eq!(
                        box_downscale_rgba(&src, w, h, dw, dh, srgb),
                        reference::box_downscale_rgba(&src, w, h, dw, dh, srgb),
                        "box parity failed: {w}x{h} -> {dw}x{dh} srgb={srgb}"
                    );
                }
                assert_eq!(
                    premul_downscale_rgba(&src, w, h, dw, dh),
                    reference::premul_downscale_rgba(&src, w, h, dw, dh),
                    "premul parity failed: {w}x{h} -> {dw}x{dh}"
                );
            }
        }
    }

    #[test]
    fn alpha_lut_matches_scalar_division() {
        for v in 0u16..=255 {
            assert_eq!(
                super::ALPHA_DIV255_F64[v as usize].to_bits(),
                (v as f64 / 255.0).to_bits()
            );
        }
    }

    #[test]
    fn lut_matches_scalar_conversion() {
        for v in 0u16..=255 {
            let v = v as u8;
            assert_eq!(
                super::SRGB_LIN_F64[v as usize].to_bits(),
                (srgb_to_linear_u8(v) as f64).to_bits()
            );
        }
    }
}
