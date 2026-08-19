use super::*;

/// Reconstructed alpha ramp for mode 4 given 6-bit endpoints. 8 entries for
/// the 3-bit index selector, 4 for the 2-bit one (upper half unused).
#[inline]
fn mode4_alpha_vals(la: i32, ha: i32, index_selector: usize) -> [i32; 8] {
    let mut vals = [0i32; 8];
    if index_selector == 0 {
        vals[0] = (la << 2) | (la >> 4);
        vals[7] = (ha << 2) | (ha >> 4);
        for i in 1..7 {
            vals[i] =
                (vals[0] * (64 - G_WEIGHTS3[i] as i32) + vals[7] * G_WEIGHTS3[i] as i32 + 32) >> 6;
        }
    } else {
        vals[0] = (la << 2) | (la >> 4);
        vals[3] = (ha << 2) | (ha >> 4);
        let (w1, w2) = (21, 43);
        vals[1] = (vals[0] * (64 - w1) + vals[3] * w1 + 32) >> 6;
        vals[2] = (vals[0] * (64 - w2) + vals[3] * w2 + 32) >> 6;
    }
    vals
}

/// For each pixel pick the ramp entry with the smallest absolute alpha error
/// (ties to the lowest index), returning the weighted squared error total.
/// `alphas` and `vals` are always in 0..=255, `num_vals` is 4 or 8.
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
fn alpha_selectors_scalar(
    alphas: &[u8; 16],
    vals: &[i32; 8],
    num_vals: usize,
    weight: u64,
    selectors: &mut [i32; 16],
) -> u64 {
    let mut err = 0u64;
    for i in 0..16 {
        let a = alphas[i] as i32;
        let mut s = 0i32;
        let mut be = iabs32(a - vals[0]);
        for (j, &v) in vals.iter().enumerate().take(num_vals).skip(1) {
            let e = iabs32(a - v);
            if e < be {
                be = e;
                s = j as i32;
            }
        }
        err += (be * be) as u64 * weight;
        selectors[i] = s;
    }
    err
}

/// NEON version of `alpha_selectors_scalar`; bit-identical by construction:
/// every lane runs the same strictly-less argmin over the same u8 distances,
/// and the error sum is the exact integer `weight * sum(be^2)`.
#[cfg(target_arch = "aarch64")]
fn alpha_selectors_neon(
    alphas: &[u8; 16],
    vals: &[i32; 8],
    num_vals: usize,
    weight: u64,
    selectors: &mut [i32; 16],
) -> u64 {
    use std::arch::aarch64::*;
    // SAFETY: NEON is baseline on aarch64; all loads/stores use in-bounds
    unsafe {
        let av = vld1q_u8(alphas.as_ptr());
        let mut best_d = vabdq_u8(av, vdupq_n_u8(vals[0] as u8));
        let mut best_s = vdupq_n_u8(0);
        for (j, &v) in vals.iter().enumerate().take(num_vals).skip(1) {
            let d = vabdq_u8(av, vdupq_n_u8(v as u8));
            let lt = vcltq_u8(d, best_d);
            best_d = vbslq_u8(lt, d, best_d);
            best_s = vbslq_u8(lt, vdupq_n_u8(j as u8), best_s);
        }
        let lo = vget_low_u8(best_d);
        let hi = vget_high_u8(best_d);
        let sum = vaddlvq_u16(vmull_u8(lo, lo)) + vaddlvq_u16(vmull_u8(hi, hi));
        let s16_lo = vmovl_u8(vget_low_u8(best_s));
        let s16_hi = vmovl_u8(vget_high_u8(best_s));
        let dst = selectors.as_mut_ptr() as *mut u32;
        vst1q_u32(dst, vmovl_u16(vget_low_u16(s16_lo)));
        vst1q_u32(dst.add(4), vmovl_u16(vget_high_u16(s16_lo)));
        vst1q_u32(dst.add(8), vmovl_u16(vget_low_u16(s16_hi)));
        vst1q_u32(dst.add(12), vmovl_u16(vget_high_u16(s16_hi)));
        sum as u64 * weight
    }
}

#[inline]
fn alpha_selectors(
    alphas: &[u8; 16],
    vals: &[i32; 8],
    num_vals: usize,
    weight: u64,
    selectors: &mut [i32; 16],
) -> u64 {
    #[cfg(target_arch = "aarch64")]
    {
        alpha_selectors_neon(alphas, vals, num_vals, weight, selectors)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        alpha_selectors_scalar(alphas, vals, num_vals, weight, selectors)
    }
}

#[inline]
fn alpha_lane(pixels: &[ColorI; 16]) -> [u8; 16] {
    let mut a = [0u8; 16];
    for i in 0..16 {
        a[i] = pixels[i].c[3] as u8;
    }
    a
}

pub(super) fn handle_alpha_block_mode4(
    pixels: &[ColorI; 16],
    cp: &Params,
    params: &mut CCParams,
    lo_a: i32,
    hi_a: i32,
    opt4: &mut OptResults,
    mode4_err: &mut u64,
) {
    params.has_alpha = false;
    params.comp_bits = 5;
    params.has_pbits = false;
    params.endpoints_share_pbit = false;
    params.perceptual = cp.perceptual;

    let alphas = alpha_lane(pixels);
    let weight = params.weights[3] as u64;

    for index_selector in 0..2usize {
        if cp.mode4_index_mask & (1 << index_selector) == 0 {
            continue;
        }
        if index_selector != 0 {
            params.psel_weights = &G_WEIGHTS3;
            params.psel_weightsx = &G_WEIGHTS3X;
            params.num_selector_weights = 8;
        } else {
            params.psel_weights = &G_WEIGHTS2;
            params.psel_weightsx = &G_WEIGHTS2X;
            params.num_selector_weights = 4;
        }
        let num_vals = if index_selector == 0 { 8 } else { 4 };
        let mut results = CCResults::new();
        let trial_err_color = color_cell_compression(4, params, &mut results, cp, 16, pixels, true);

        let mut la = ((lo_a + 2) >> 2).min(63);
        let mut ha = ((hi_a + 2) >> 2).min(63);
        if la == ha && lo_a != hi_a {
            if ha != 63 {
                ha += 1;
            } else if la != 0 {
                la -= 1;
            }
        }

        let mut best_alpha_err = u64::MAX;
        let mut best_la = 0i32;
        let mut best_ha = 0i32;
        let mut best_alpha_selectors = [0i32; 16];

        for pass in 0..2 {
            let vals = mode4_alpha_vals(la, ha, index_selector);
            let mut trial_alpha_selectors = [0i32; 16];
            let trial_alpha_err =
                alpha_selectors(&alphas, &vals, num_vals, weight, &mut trial_alpha_selectors);
            if trial_alpha_err < best_alpha_err {
                best_alpha_err = trial_alpha_err;
                best_la = la;
                best_ha = ha;
                best_alpha_selectors = trial_alpha_selectors;
            }
            if pass == 0 {
                let mut xl = 0f32;
                let mut xh = 0f32;
                let sw = if index_selector != 0 {
                    &G_WEIGHTS2X[..]
                } else {
                    &G_WEIGHTS3X[..]
                };
                compute_lsq_endpoints_a(16, &trial_alpha_selectors, sw, &mut xl, &mut xh, pixels);
                if xl > xh {
                    std::mem::swap(&mut xl, &mut xh);
                }
                la = itrunc((xl * (63.0 / 255.0) + 0.5).floor()).clamp(0, 63);
                ha = itrunc((xh * (63.0 / 255.0) + 0.5).floor()).clamp(0, 63);
            }
        }

        if cp.uber_level > 0 {
            let d = (cp.uber_level as i32).min(3);
            for ld in -d..=d {
                for hd in -d..=d {
                    la = (best_la + ld).clamp(0, 63);
                    ha = (best_ha + hd).clamp(0, 63);
                    let vals = mode4_alpha_vals(la, ha, index_selector);
                    let mut trial_alpha_selectors = [0i32; 16];
                    let trial_alpha_err = alpha_selectors(
                        &alphas,
                        &vals,
                        num_vals,
                        weight,
                        &mut trial_alpha_selectors,
                    );
                    if trial_alpha_err < best_alpha_err {
                        best_alpha_err = trial_alpha_err;
                        best_la = la;
                        best_ha = ha;
                        best_alpha_selectors = trial_alpha_selectors;
                    }
                }
            }
        }

        let trial_err = trial_err_color + best_alpha_err;
        if trial_err < *mode4_err {
            *mode4_err = trial_err;
            opt4.mode = 4;
            opt4.index_selector = index_selector as u32;
            opt4.rotation = 0;
            opt4.partition = 0;
            opt4.low[0] = results.low;
            opt4.high[0] = results.high;
            opt4.low[0].c[3] = best_la;
            opt4.high[0].c[3] = best_ha;
            opt4.selectors = results.selectors;
            opt4.alpha_selectors = best_alpha_selectors;
        }
    }
}

/// 4-entry alpha ramp for modes 5: endpoints are full 8-bit.
#[inline]
fn mode5_alpha_vals(lo_a: i32, hi_a: i32) -> [i32; 8] {
    let mut vals = [0i32; 8];
    vals[0] = lo_a;
    vals[3] = hi_a;
    let (w1, w2) = (21, 43);
    vals[1] = (vals[0] * (64 - w1) + vals[3] * w1 + 32) >> 6;
    vals[2] = (vals[0] * (64 - w2) + vals[3] * w2 + 32) >> 6;
    vals
}

pub(super) fn handle_alpha_block_mode5(
    pixels: &[ColorI; 16],
    cp: &Params,
    params: &mut CCParams,
    mut lo_a: i32,
    mut hi_a: i32,
    opt5: &mut OptResults,
    mode5_err: &mut u64,
) {
    params.psel_weights = &G_WEIGHTS2;
    params.psel_weightsx = &G_WEIGHTS2X;
    params.num_selector_weights = 4;
    params.comp_bits = 7;
    params.has_alpha = false;
    params.has_pbits = false;
    params.endpoints_share_pbit = false;
    params.perceptual = cp.perceptual;

    let mut results5 = CCResults::new();
    *mode5_err = color_cell_compression(5, params, &mut results5, cp, 16, pixels, true);
    opt5.low[0] = results5.low;
    opt5.high[0] = results5.high;
    opt5.selectors = results5.selectors;

    if lo_a == hi_a {
        opt5.low[0].c[3] = lo_a;
        opt5.high[0].c[3] = hi_a;
        opt5.alpha_selectors = [0; 16];
    } else {
        let alphas = alpha_lane(pixels);
        let weight = params.weights[3] as u64;
        let mut mode5_alpha_err = u64::MAX;
        for pass in 0..2 {
            let vals = mode5_alpha_vals(lo_a, hi_a);
            let mut trial_alpha_selectors = [0i32; 16];
            let trial_alpha_err =
                alpha_selectors(&alphas, &vals, 4, weight, &mut trial_alpha_selectors);
            if trial_alpha_err < mode5_alpha_err {
                mode5_alpha_err = trial_alpha_err;
                opt5.low[0].c[3] = lo_a;
                opt5.high[0].c[3] = hi_a;
                opt5.alpha_selectors = trial_alpha_selectors;
            }
            if pass == 0 {
                let mut xl = 0f32;
                let mut xh = 0f32;
                compute_lsq_endpoints_a(
                    16,
                    &trial_alpha_selectors,
                    &G_WEIGHTS2X,
                    &mut xl,
                    &mut xh,
                    pixels,
                );
                let mut new_lo = itrunc((xl + 0.5).floor()).clamp(0, 255);
                let mut new_hi = itrunc((xh + 0.5).floor()).clamp(0, 255);
                if new_lo > new_hi {
                    std::mem::swap(&mut new_lo, &mut new_hi);
                }
                if new_lo == lo_a && new_hi == hi_a {
                    break;
                }
                lo_a = new_lo;
                hi_a = new_hi;
            }
        }
        if cp.uber_level > 0 {
            let d = (cp.uber_level as i32).min(3);
            for ld in -d..=d {
                for hd in -d..=d {
                    lo_a = (opt5.low[0].c[3] + ld).clamp(0, 255);
                    hi_a = (opt5.high[0].c[3] + hd).clamp(0, 255);
                    let vals = mode5_alpha_vals(lo_a, hi_a);
                    let mut trial_alpha_selectors = [0i32; 16];
                    let trial_alpha_err =
                        alpha_selectors(&alphas, &vals, 4, weight, &mut trial_alpha_selectors);
                    if trial_alpha_err < mode5_alpha_err {
                        mode5_alpha_err = trial_alpha_err;
                        opt5.low[0].c[3] = lo_a;
                        opt5.high[0].c[3] = hi_a;
                        opt5.alpha_selectors = trial_alpha_selectors;
                    }
                }
            }
        }
        *mode5_err += mode5_alpha_err;
    }
    opt5.mode = 5;
    opt5.index_selector = 0;
    opt5.rotation = 0;
    opt5.partition = 0;
}

pub(super) fn handle_alpha_block(
    pixels: &[ColorI; 16],
    cp: &Params,
    base: &CCParams,
    lo_a: i32,
    hi_a: i32,
    plan: &PartitionPlan,
) -> [u8; 16] {
    let mut base = base.clone();
    base.perceptual = cp.perceptual;
    let base = &base;
    let mut opt_results = OptResults::new();
    let mut best_err = u64::MAX;

    if cp.use_mode4 {
        let num_rotations = if cp.perceptual || !cp.use_mode4_rotation {
            1
        } else {
            4
        };
        for rotation in 0..num_rotations {
            if cp.mode4_rotation_mask & (1 << rotation) == 0 {
                continue;
            }
            let mut params4 = base.clone();
            if rotation != 0 {
                params4.weights.swap(rotation - 1, 3);
            }
            let mut rot_pixels = *pixels;
            let mut tlo = lo_a;
            let mut thi = hi_a;
            if rotation != 0 {
                tlo = 255;
                thi = 0;
                for i in 0..16 {
                    rot_pixels[i].c.swap(3, rotation - 1);
                    tlo = tlo.min(rot_pixels[i].c[3]);
                    thi = thi.max(rot_pixels[i].c[3]);
                }
            }
            let mut trial4 = OptResults::new();
            let mut trial_err = best_err;
            handle_alpha_block_mode4(
                &rot_pixels,
                cp,
                &mut params4,
                tlo,
                thi,
                &mut trial4,
                &mut trial_err,
            );
            if trial_err < best_err {
                best_err = trial_err;
                opt_results.mode = 4;
                opt_results.index_selector = trial4.index_selector;
                opt_results.rotation = rotation as u32;
                opt_results.partition = 0;
                opt_results.low[0] = trial4.low[0];
                opt_results.high[0] = trial4.high[0];
                opt_results.selectors = trial4.selectors;
                opt_results.alpha_selectors = trial4.alpha_selectors;
            }
        }
    }

    if cp.use_mode6 {
        let mut params6 = base.clone();
        for c in 0..4 {
            params6.weights[c] *= cp.mode67_weight_mul[c];
        }
        params6.psel_weights = &G_WEIGHTS4;
        params6.psel_weightsx = &G_WEIGHTS4X;
        params6.num_selector_weights = 16;
        params6.comp_bits = 7;
        params6.has_pbits = true;
        params6.endpoints_share_pbit = false;
        params6.has_alpha = true;
        let mut results6 = CCResults::new();
        let mode6_err = color_cell_compression(6, &params6, &mut results6, cp, 16, pixels, true);
        if mode6_err < best_err {
            best_err = mode6_err;
            opt_results.mode = 6;
            opt_results.index_selector = 0;
            opt_results.rotation = 0;
            opt_results.partition = 0;
            opt_results.low[0] = results6.low;
            opt_results.high[0] = results6.high;
            opt_results.pbits[0] = results6.pbits;
            opt_results.selectors = results6.selectors;
        }
    }

    if cp.use_mode5 {
        let num_rotations = if cp.perceptual || !cp.use_mode5_rotation {
            1
        } else {
            4
        };
        for rotation in 0..num_rotations {
            if cp.mode5_rotation_mask & (1 << rotation) == 0 {
                continue;
            }
            let mut params5 = base.clone();
            if rotation != 0 {
                params5.weights.swap(rotation - 1, 3);
            }
            let mut rot_pixels = *pixels;
            let mut tlo = lo_a;
            let mut thi = hi_a;
            if rotation != 0 {
                tlo = 255;
                thi = 0;
                for i in 0..16 {
                    rot_pixels[i].c.swap(3, rotation - 1);
                    tlo = tlo.min(rot_pixels[i].c[3]);
                    thi = thi.max(rot_pixels[i].c[3]);
                }
            }
            let mut trial5 = OptResults::new();
            let mut trial_err = 0u64;
            handle_alpha_block_mode5(
                &rot_pixels,
                cp,
                &mut params5,
                tlo,
                thi,
                &mut trial5,
                &mut trial_err,
            );
            if trial_err < best_err {
                best_err = trial_err;
                opt_results = trial5;
                opt_results.rotation = rotation as u32;
            }
        }
    }

    if cp.use_mode7 {
        let solutions = &plan.list7;
        let num_solutions = solutions.len();
        let mut params7 = base.clone();
        for c in 0..4 {
            params7.weights[c] *= cp.mode67_weight_mul[c];
        }
        params7.psel_weights = &G_WEIGHTS2;
        params7.psel_weightsx = &G_WEIGHTS2X;
        params7.num_selector_weights = 4;
        params7.comp_bits = 5;
        params7.has_pbits = true;
        params7.endpoints_share_pbit = false;
        params7.has_alpha = true;

        let run_partition =
            |trial_partition: u32, best_err: &mut u64, opt: &mut OptResults, refine_force: bool| {
                let part = &G_PARTITION2[(trial_partition as usize) * 16..];
                let mut subset_colors = [[ColorI::default(); 16]; 2];
                let mut subset_total = [0usize; 2];
                let mut subset_pixel_index = [[0usize; 16]; 2];
                let mut subset_selectors = [[0i32; 16]; 2];
                let mut subset_low = [ColorI::default(); 2];
                let mut subset_high = [ColorI::default(); 2];
                let mut subset_pbits = [[0u32; 2]; 2];
                for idx in 0..16 {
                    let pp = part[idx] as usize;
                    subset_colors[pp][subset_total[pp]] = pixels[idx];
                    subset_pixel_index[pp][subset_total[pp]] = idx;
                    subset_total[pp] += 1;
                }
                let mut trial_err = 0u64;
                let mut ok = true;
                for subset in 0..2 {
                    let mut results = CCResults::new();
                    let refine = (num_solutions <= 2) || refine_force;
                    let err = color_cell_compression(
                        7,
                        &params7,
                        &mut results,
                        cp,
                        subset_total[subset],
                        &subset_colors[subset],
                        refine,
                    );
                    subset_selectors[subset] = results.selectors;
                    subset_low[subset] = results.low;
                    subset_high[subset] = results.high;
                    subset_pbits[subset] = results.pbits;
                    trial_err += err;
                    if trial_err > *best_err {
                        ok = false;
                        break;
                    }
                }
                if ok && trial_err < *best_err {
                    *best_err = trial_err;
                    opt.mode = 7;
                    opt.index_selector = 0;
                    opt.rotation = 0;
                    opt.partition = trial_partition;
                    for subset in 0..2 {
                        for i in 0..subset_total[subset] {
                            opt.selectors[subset_pixel_index[subset][i]] =
                                subset_selectors[subset][i];
                        }
                        opt.low[subset] = subset_low[subset];
                        opt.high[subset] = subset_high[subset];
                        opt.pbits[subset] = subset_pbits[subset];
                    }
                    return true;
                }
                false
            };

        for solution_index in 0..num_solutions {
            run_partition(
                solutions[solution_index].index,
                &mut best_err,
                &mut opt_results,
                false,
            );
        }
        if num_solutions > 2 && opt_results.mode == 7 {
            let tp = opt_results.partition;
            run_partition(tp, &mut best_err, &mut opt_results, true);
        }
    }

    encode_bc7_block_bits(&opt_results)
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
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

    #[test]
    fn neon_alpha_selectors_match_scalar() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        let mut cases: Vec<([u8; 16], [i32; 8], usize, u64)> = Vec::new();
        cases.push(([0u8; 16], [0; 8], 8, 1));
        cases.push(([255u8; 16], [255; 8], 8, 1));
        let ramp = mode4_alpha_vals(0, 63, 0);
        cases.push(([0u8; 16], ramp, 8, 3));
        let mut edge = [0u8; 16];
        for (i, e) in edge.iter_mut().enumerate() {
            *e = if i % 2 == 0 { 0 } else { 255 };
        }
        cases.push((edge, mode4_alpha_vals(31, 32, 1), 4, 7));
        for _ in 0..2000 {
            let mut alphas = [0u8; 16];
            for a in alphas.iter_mut() {
                *a = rng.next_u32() as u8;
            }
            let la = (rng.next_u32() % 64) as i32;
            let ha = (rng.next_u32() % 64) as i32;
            let sel = (rng.next_u32() % 2) as usize;
            let vals = mode4_alpha_vals(la, ha, sel);
            let num_vals = if sel == 0 { 8 } else { 4 };
            cases.push((alphas, vals, num_vals, (rng.next_u32() % 128 + 1) as u64));
            let vals5 =
                mode5_alpha_vals((rng.next_u32() % 256) as i32, (rng.next_u32() % 256) as i32);
            cases.push((alphas, vals5, 4, (rng.next_u32() % 128 + 1) as u64));
        }
        for (alphas, vals, num_vals, weight) in cases {
            let mut sel_s = [0i32; 16];
            let mut sel_n = [0i32; 16];
            let err_s = alpha_selectors_scalar(&alphas, &vals, num_vals, weight, &mut sel_s);
            let err_n = alpha_selectors_neon(&alphas, &vals, num_vals, weight, &mut sel_n);
            assert_eq!(err_s, err_n, "err mismatch for {alphas:?} {vals:?}");
            assert_eq!(sel_s, sel_n, "selector mismatch for {alphas:?} {vals:?}");
        }
    }
}
