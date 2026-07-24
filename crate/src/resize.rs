use crate::bc7_pure::{linear_to_srgb_u8, srgb_to_linear_u8};

const C: usize = 4;

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

const PREMUL_ALPHA_EPS: f64 = 1e-8;

pub fn premul_downscale_rgba(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
