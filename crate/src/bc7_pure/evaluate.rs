use super::*;

#[cfg(target_arch = "aarch64")]
static HAS_NEON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
#[cfg(target_arch = "aarch64")]
#[inline]
fn has_neon() -> bool {
    *HAS_NEON.get_or_init(|| std::env::var_os("ABGEN_BC7_SCALAR").is_none())
}
#[cfg(not(target_arch = "aarch64"))]
#[inline]
fn has_neon() -> bool {
    false
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn build_wc_table_sse(wc: &mut [[f32; 4]; 16], psel: &[u32], n: usize, nc: usize) {
    use std::arch::x86_64::*;
    let a = _mm_loadu_ps(wc[0].as_ptr());
    let b = _mm_loadu_ps(wc[n - 1].as_ptr());
    let v64 = _mm_set1_ps(64.0);
    let v32 = _mm_set1_ps(32.0);
    let inv64 = _mm_set1_ps(1.0 / 64.0);
    for i in 1..(n - 1) {
        let wv = _mm_set1_ps(psel[i] as f32);
        let iwv = _mm_sub_ps(v64, wv);
        let t = _mm_add_ps(_mm_add_ps(_mm_mul_ps(a, iwv), _mm_mul_ps(b, wv)), v32);
        let t = _mm_floor_ps(_mm_mul_ps(t, inv64));

        let t = if nc == 3 {
            _mm_insert_ps::<0b0000_1000>(t, t)
        } else {
            t
        };
        _mm_storeu_ps(wc[i].as_mut_ptr(), t);
    }
}

pub(super) fn evaluate_solution(
    low: &ColorI,
    high: &ColorI,
    pbits: &[u32; 2],
    p: &CCParams,
    res: &mut CCResults,
    num_pixels: usize,
    pixels: &[ColorI],
) -> u64 {
    let mut qmin = *low;
    let mut qmax = *high;
    if p.has_pbits {
        let (min_pbit, max_pbit) = if p.endpoints_share_pbit {
            (pbits[0], pbits[0])
        } else {
            (pbits[0], pbits[1])
        };
        for i in 0..4 {
            qmin.c[i] = (low.c[i] << 1) | min_pbit as i32;
            qmax.c[i] = (high.c[i] << 1) | max_pbit as i32;
        }
    }
    let amin = scale_color(&qmin, p);
    let amax = scale_color(&qmax, p);
    let n = p.num_selector_weights as usize;
    let nc = if p.has_alpha { 4 } else { 3 };

    let mut total_errf = 0f32;
    let wr = p.weights[0] as f32;
    let wg = p.weights[1] as f32;
    let wb = p.weights[2] as f32;
    let wa = p.weights[3] as f32;

    let mut wc = [[0f32; 4]; 16];
    for j in 0..4 {
        wc[0][j] = amin.c[j] as f32;
        wc[n - 1][j] = amax.c[j] as f32;
    }

    #[allow(unused_mut)]
    let mut wc_built = false;
    #[cfg(target_arch = "x86_64")]
    if has_avx2() {
        unsafe { build_wc_table_sse(&mut wc, p.psel_weights, n, nc) };
        wc_built = true;
    }
    #[cfg(target_arch = "aarch64")]
    if has_neon() {
        unsafe { neon::build_wc_table_neon(&mut wc, p.psel_weights, n, nc) };
        wc_built = true;
    }
    if !wc_built {
        for i in 1..(n - 1) {
            for j in 0..nc {
                wc[i][j] = ((wc[0][j] * (64.0 - p.psel_weights[i] as f32)
                    + wc[n - 1][j] * p.psel_weights[i] as f32
                    + 32.0)
                    * (1.0 / 64.0))
                    .floor();
            }
        }
    }

    if !p.perceptual {
        if !p.has_alpha {
            if n == 16 {
                let lr = amin.c[0] as f32;
                let lg = amin.c[1] as f32;
                let lb = amin.c[2] as f32;
                let dr = amax.c[0] as f32 - lr;
                let dg = amax.c[1] as f32 - lg;
                let db = amax.c[2] as f32 - lb;
                let f = n as f32 / (dr * dr + dg * dg + db * db);
                let lr = lr * -dr;
                let lg = lg * -dg;
                let lb = lb * -db;
                #[cfg(target_arch = "x86_64")]
                {
                    if has_avx2() {
                        unsafe {
                            total_errf = eval_solution_n16_rgb_avx2(
                                num_pixels,
                                pixels,
                                &wc,
                                wr,
                                wg,
                                wb,
                                dr,
                                dg,
                                db,
                                lr,
                                lg,
                                lb,
                                f,
                                n,
                                &mut res.selectors_temp,
                            );
                        }
                    } else {
                        total_errf = eval_solution_n16_rgb_scalar(
                            num_pixels,
                            pixels,
                            &wc,
                            wr,
                            wg,
                            wb,
                            dr,
                            dg,
                            db,
                            lr,
                            lg,
                            lb,
                            f,
                            n,
                            &mut res.selectors_temp,
                        );
                    }
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    #[allow(unused_mut)]
                    let mut simd_done = false;
                    #[cfg(target_arch = "aarch64")]
                    if has_neon() {
                        total_errf = unsafe {
                            neon::eval_solution_n16_rgb_neon(
                                num_pixels,
                                pixels,
                                &wc,
                                wr,
                                wg,
                                wb,
                                dr,
                                dg,
                                db,
                                lr,
                                lg,
                                lb,
                                f,
                                n,
                                &mut res.selectors_temp,
                            )
                        };
                        simd_done = true;
                    }
                    if !simd_done {
                        total_errf = eval_solution_n16_rgb_scalar(
                            num_pixels,
                            pixels,
                            &wc,
                            wr,
                            wg,
                            wb,
                            dr,
                            dg,
                            db,
                            lr,
                            lg,
                            lb,
                            f,
                            n,
                            &mut res.selectors_temp,
                        );
                    }
                }
            } else if (has_avx2() || has_neon()) && (n == 4 || n == 8) {
                #[cfg(target_arch = "x86_64")]
                {
                    total_errf = unsafe {
                        eval_discrete_avx2(
                            num_pixels,
                            pixels,
                            &wc,
                            wr,
                            wg,
                            wb,
                            wa,
                            false,
                            n,
                            &mut res.selectors_temp,
                        )
                    };
                }
                #[cfg(target_arch = "aarch64")]
                {
                    total_errf = unsafe {
                        neon::eval_discrete_neon(
                            num_pixels,
                            pixels,
                            &wc,
                            wr,
                            wg,
                            wb,
                            wa,
                            false,
                            n,
                            &mut res.selectors_temp,
                        )
                    };
                }
            } else {
                total_errf = if wr == 1.0 && wg == 1.0 && wb == 1.0 {
                    eval_discrete_rgb_scalar::<false>(
                        num_pixels,
                        pixels,
                        &wc,
                        wr,
                        wg,
                        wb,
                        n,
                        &mut res.selectors_temp,
                    )
                } else {
                    eval_discrete_rgb_scalar::<true>(
                        num_pixels,
                        pixels,
                        &wc,
                        wr,
                        wg,
                        wb,
                        n,
                        &mut res.selectors_temp,
                    )
                };
            }
        } else {
            if n == 16 {
                let lr = amin.c[0] as f32;
                let lg = amin.c[1] as f32;
                let lb = amin.c[2] as f32;
                let la = amin.c[3] as f32;
                let dr = amax.c[0] as f32 - lr;
                let dg = amax.c[1] as f32 - lg;
                let db = amax.c[2] as f32 - lb;
                let da = amax.c[3] as f32 - la;
                let f = n as f32 / (dr * dr + dg * dg + db * db + da * da);
                let lr = lr * -dr;
                let lg = lg * -dg;
                let lb = lb * -db;
                let la = la * -da;
                #[allow(unused_mut)]
                let mut simd_done = false;
                #[cfg(target_arch = "aarch64")]
                if has_neon() {
                    total_errf = unsafe {
                        neon::eval_solution_n16_rgba_neon(
                            num_pixels,
                            pixels,
                            &wc,
                            wr,
                            wg,
                            wb,
                            wa,
                            dr,
                            dg,
                            db,
                            da,
                            lr,
                            lg,
                            lb,
                            la,
                            f,
                            n,
                            &mut res.selectors_temp,
                        )
                    };
                    simd_done = true;
                }
                if !simd_done {
                    for i in 0..num_pixels {
                        let r = pixels[i].c[0] as f32;
                        let g = pixels[i].c[1] as f32;
                        let b = pixels[i].c[2] as f32;
                        let a = pixels[i].c[3] as f32;
                        let mut best_sel =
                            ((((r * dr + lr) + (g * dg + lg) + (b * db + lb) + (a * da + la)) * f)
                                + 0.5)
                                .floor();
                        best_sel = best_sel.clamp(1.0, (n - 1) as f32);
                        let best_sel0 = best_sel - 1.0;
                        let i0 = best_sel0 as i32 as usize;
                        let i1 = best_sel as i32 as usize;
                        let dr0 = wc[i0][0] - r;
                        let dg0 = wc[i0][1] - g;
                        let db0 = wc[i0][2] - b;
                        let da0 = wc[i0][3] - a;
                        let err0 =
                            wr * dr0 * dr0 + wg * dg0 * dg0 + wb * db0 * db0 + wa * da0 * da0;
                        let dr1 = wc[i1][0] - r;
                        let dg1 = wc[i1][1] - g;
                        let db1 = wc[i1][2] - b;
                        let da1 = wc[i1][3] - a;
                        let err1 =
                            wr * dr1 * dr1 + wg * dg1 * dg1 + wb * db1 * db1 + wa * da1 * da1;
                        let min_err = err0.min(err1);
                        total_errf += min_err;
                        res.selectors_temp[i] =
                            if min_err == err0 { best_sel0 } else { best_sel } as i32;
                    }
                }
            } else if (has_avx2() || has_neon()) && (n == 4 || n == 8) {
                #[cfg(target_arch = "x86_64")]
                {
                    total_errf = unsafe {
                        eval_discrete_avx2(
                            num_pixels,
                            pixels,
                            &wc,
                            wr,
                            wg,
                            wb,
                            wa,
                            true,
                            n,
                            &mut res.selectors_temp,
                        )
                    };
                }
                #[cfg(target_arch = "aarch64")]
                {
                    total_errf = unsafe {
                        neon::eval_discrete_neon(
                            num_pixels,
                            pixels,
                            &wc,
                            wr,
                            wg,
                            wb,
                            wa,
                            true,
                            n,
                            &mut res.selectors_temp,
                        )
                    };
                }
            } else {
                for i in 0..num_pixels {
                    let pr = pixels[i].c[0] as f32;
                    let pg = pixels[i].c[1] as f32;
                    let pb = pixels[i].c[2] as f32;
                    let pa = pixels[i].c[3] as f32;
                    let mut errs = [0f32; 4];
                    for k in 0..4usize {
                        let d0 = wc[k][0] - pr;
                        let d1 = wc[k][1] - pg;
                        let d2 = wc[k][2] - pb;
                        let d3 = wc[k][3] - pa;
                        errs[k] = wr * d0 * d0 + wg * d1 * d1 + wb * d2 * d2 + wa * d3 * d3;
                    }
                    let mut best_err = errs[0].min(errs[1]).min(errs[2]).min(errs[3]);
                    let mut best_sel = if best_err == errs[1] { 1 } else { 0 };
                    if best_err == errs[2] {
                        best_sel = 2;
                    }
                    if best_err == errs[3] {
                        best_sel = 3;
                    }
                    if n == 8 {
                        let mut e2 = [0f32; 4];
                        for k in 0..4usize {
                            let d0 = wc[4 + k][0] - pr;
                            let d1 = wc[4 + k][1] - pg;
                            let d2 = wc[4 + k][2] - pb;
                            let d3 = wc[4 + k][3] - pa;
                            e2[k] = wr * d0 * d0 + wg * d1 * d1 + wb * d2 * d2 + wa * d3 * d3;
                        }
                        best_err = best_err.min(e2[0].min(e2[1]).min(e2[2]).min(e2[3]));
                        if best_err == e2[0] {
                            best_sel = 4;
                        }
                        if best_err == e2[1] {
                            best_sel = 5;
                        }
                        if best_err == e2[2] {
                            best_sel = 6;
                        }
                        if best_err == e2[3] {
                            best_sel = 7;
                        }
                    }
                    total_errf += best_err;
                    res.selectors_temp[i] = best_sel;
                }
            }
        }
    } else {
        let wgp = wg * PR_WEIGHT;
        let wbp = wb * PB_WEIGHT;
        #[cfg(target_arch = "aarch64")]
        let neon_perc_done = if has_neon() && (n == 4 || n == 8 || n == 16) {
            total_errf = unsafe {
                neon::eval_perceptual_neon(
                    num_pixels,
                    pixels,
                    &wc,
                    wr,
                    wgp,
                    wbp,
                    wa,
                    p.has_alpha,
                    n,
                    &mut res.selectors_temp,
                )
            };
            true
        } else {
            false
        };
        #[cfg(not(target_arch = "aarch64"))]
        let neon_perc_done = false;
        if neon_perc_done {
        } else {
            let mut wy = [0f32; 16];
            let mut wcr = [0f32; 16];
            let mut wcb = [0f32; 16];

            for i in 0..16 {
                let r = wc[i][0];
                let g = wc[i][1];
                let b = wc[i][2];
                let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
                wy[i] = y;
                wcr[i] = r - y;
                wcb[i] = b - y;
            }
            #[cfg(target_arch = "x86_64")]
            let simd_done = if has_avx2() && (n == 4 || n == 8 || n == 16) {
                let mut wa4 = [0f32; 16];
                if p.has_alpha {
                    for i in 0..16 {
                        wa4[i] = wc[i][3];
                    }
                }
                total_errf = unsafe {
                    eval_perceptual_avx2(
                        num_pixels,
                        pixels,
                        &wy,
                        &wcr,
                        &wcb,
                        &wa4,
                        wr,
                        wgp,
                        wbp,
                        wa,
                        p.has_alpha,
                        n,
                        &mut res.selectors_temp,
                    )
                };
                true
            } else {
                false
            };
            #[cfg(not(target_arch = "x86_64"))]
            let simd_done = false;
            if simd_done {
            } else if p.has_alpha {
                for i in 0..num_pixels {
                    let r = pixels[i].c[0] as f32;
                    let g = pixels[i].c[1] as f32;
                    let b = pixels[i].c[2] as f32;
                    let a = pixels[i].c[3] as f32;
                    let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
                    let cr = r - y;
                    let cb = b - y;
                    let mut best_err = 1e10f32;
                    let mut best_sel = 0i32;
                    for j in 0..n {
                        let dl = y - wy[j];
                        let dcr = cr - wcr[j];
                        let dcb = cb - wcb[j];
                        let da = a - wc[j][3];
                        let err = wr * dl * dl + wgp * dcr * dcr + wbp * dcb * dcb + wa * da * da;
                        if err < best_err {
                            best_err = err;
                            best_sel = j as i32;
                        }
                    }
                    total_errf += best_err;
                    res.selectors_temp[i] = best_sel;
                }
            } else {
                for i in 0..num_pixels {
                    let r = pixels[i].c[0] as f32;
                    let g = pixels[i].c[1] as f32;
                    let b = pixels[i].c[2] as f32;
                    let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
                    let cr = r - y;
                    let cb = b - y;
                    let mut best_err = 1e10f32;
                    let mut best_sel = 0i32;
                    for j in 0..n {
                        let dl = y - wy[j];
                        let dcr = cr - wcr[j];
                        let dcb = cb - wcb[j];
                        let err = wr * dl * dl + wgp * dcr * dcr + wbp * dcb * dcb;
                        if err < best_err {
                            best_err = err;
                            best_sel = j as i32;
                        }
                    }
                    total_errf += best_err;
                    res.selectors_temp[i] = best_sel;
                }
            }
        }
    }

    let total_err = total_errf as i64 as u64;
    if total_err < res.best_overall_err {
        res.best_overall_err = total_err;
        res.low = *low;
        res.high = *high;
        res.pbits = *pbits;
        for i in 0..num_pixels {
            res.selectors[i] = res.selectors_temp[i];
        }
    }
    total_err
}

#[inline]
fn eval_discrete_rgb_scalar<const WEIGHTED: bool>(
    num_pixels: usize,
    pixels: &[ColorI],
    wc: &[[f32; 4]; 16],
    wr: f32,
    wg: f32,
    wb: f32,
    n: usize,
    selectors_temp: &mut [i32; 16],
) -> f32 {
    let mut total_errf = 0f32;
    for i in 0..num_pixels {
        let pr = pixels[i].c[0] as f32;
        let pg = pixels[i].c[1] as f32;
        let pb = pixels[i].c[2] as f32;

        let mut errs = [0f32; 4];
        for k in 0..4usize {
            let d0 = wc[k][0] - pr;
            let d1 = wc[k][1] - pg;
            let d2 = wc[k][2] - pb;
            errs[k] = if WEIGHTED {
                wr * d0 * d0 + wg * d1 * d1 + wb * d2 * d2
            } else {
                d0 * d0 + d1 * d1 + d2 * d2
            };
        }
        let mut best_err = errs[0].min(errs[1]).min(errs[2]).min(errs[3]);
        let mut best_sel = if best_err == errs[1] { 1 } else { 0 };
        if best_err == errs[2] {
            best_sel = 2;
        }
        if best_err == errs[3] {
            best_sel = 3;
        }
        if n == 8 {
            let mut e2 = [0f32; 4];
            for k in 0..4usize {
                let d0 = wc[4 + k][0] - pr;
                let d1 = wc[4 + k][1] - pg;
                let d2 = wc[4 + k][2] - pb;
                e2[k] = if WEIGHTED {
                    wr * d0 * d0 + wg * d1 * d1 + wb * d2 * d2
                } else {
                    d0 * d0 + d1 * d1 + d2 * d2
                };
            }
            best_err = best_err.min(e2[0].min(e2[1]).min(e2[2]).min(e2[3]));
            if best_err == e2[0] {
                best_sel = 4;
            }
            if best_err == e2[1] {
                best_sel = 5;
            }
            if best_err == e2[2] {
                best_sel = 6;
            }
            if best_err == e2[3] {
                best_sel = 7;
            }
        }
        total_errf += best_err;
        selectors_temp[i] = best_sel;
    }
    total_errf
}

#[inline]
fn eval_solution_n16_rgb_scalar(
    num_pixels: usize,
    pixels: &[ColorI],
    wc: &[[f32; 4]; 16],
    wr: f32,
    wg: f32,
    wb: f32,
    dr: f32,
    dg: f32,
    db: f32,
    lr: f32,
    lg: f32,
    lb: f32,
    f: f32,
    n: usize,
    selectors_temp: &mut [i32; 16],
) -> f32 {
    let mut total_errf = 0f32;
    for i in 0..num_pixels {
        let r = pixels[i].c[0] as f32;
        let g = pixels[i].c[1] as f32;
        let b = pixels[i].c[2] as f32;
        let mut best_sel = ((((r * dr + lr) + (g * dg + lg) + (b * db + lb)) * f) + 0.5).floor();
        best_sel = best_sel.clamp(1.0, (n - 1) as f32);
        let best_sel0 = best_sel - 1.0;
        let i0 = best_sel0 as i32 as usize;
        let i1 = best_sel as i32 as usize;
        let dr0 = wc[i0][0] - r;
        let dg0 = wc[i0][1] - g;
        let db0 = wc[i0][2] - b;
        let err0 = wr * dr0 * dr0 + wg * dg0 * dg0 + wb * db0 * db0;
        let dr1 = wc[i1][0] - r;
        let dg1 = wc[i1][1] - g;
        let db1 = wc[i1][2] - b;
        let err1 = wr * dr1 * dr1 + wg * dg1 * dg1 + wb * db1 * db1;
        let min_err = err0.min(err1);
        total_errf += min_err;
        selectors_temp[i] = if min_err == err0 { best_sel0 } else { best_sel } as i32;
    }
    total_errf
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn eval_solution_n16_rgb_avx2(
    num_pixels: usize,
    pixels: &[ColorI],
    wc: &[[f32; 4]; 16],
    wr: f32,
    wg: f32,
    wb: f32,
    dr: f32,
    dg: f32,
    db: f32,
    lr: f32,
    lg: f32,
    lb: f32,
    f: f32,
    n: usize,
    selectors_temp: &mut [i32; 16],
) -> f32 {
    use std::arch::x86_64::*;
    let w_v = _mm_setr_ps(wr, wg, wb, 0.0);
    let mut total_errf = 0f32;
    let n_minus_1 = (n - 1) as f32;
    for i in 0..num_pixels {
        let r = pixels[i].c[0] as f32;
        let g = pixels[i].c[1] as f32;
        let b = pixels[i].c[2] as f32;
        let mut best_sel = ((((r * dr + lr) + (g * dg + lg) + (b * db + lb)) * f) + 0.5).floor();
        best_sel = best_sel.clamp(1.0, n_minus_1);
        let best_sel0 = best_sel - 1.0;
        let i0 = best_sel0 as i32 as usize;
        let i1 = best_sel as i32 as usize;

        let pi = _mm_loadu_si128(pixels[i].c.as_ptr() as *const __m128i);
        let pi_v = _mm_cvtepi32_ps(pi);
        let wc0_v = _mm_loadu_ps(wc[i0].as_ptr());
        let wc1_v = _mm_loadu_ps(wc[i1].as_ptr());
        let d0_v = _mm_sub_ps(wc0_v, pi_v);
        let d1_v = _mm_sub_ps(wc1_v, pi_v);
        let t0 = _mm_mul_ps(_mm_mul_ps(w_v, d0_v), d0_v);
        let t1 = _mm_mul_ps(_mm_mul_ps(w_v, d1_v), d1_v);
        let r0 = _mm_cvtss_f32(t0);
        let g0 = _mm_cvtss_f32(_mm_shuffle_ps(t0, t0, 0b01_01_01_01));
        let b0 = _mm_cvtss_f32(_mm_shuffle_ps(t0, t0, 0b10_10_10_10));
        let err0 = r0 + g0 + b0;
        let r1 = _mm_cvtss_f32(t1);
        let g1 = _mm_cvtss_f32(_mm_shuffle_ps(t1, t1, 0b01_01_01_01));
        let b1 = _mm_cvtss_f32(_mm_shuffle_ps(t1, t1, 0b10_10_10_10));
        let err1 = r1 + g1 + b1;
        let min_err = err0.min(err1);
        total_errf += min_err;
        selectors_temp[i] = if min_err == err0 { best_sel0 } else { best_sel } as i32;
    }
    total_errf
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn eval_discrete_avx2(
    num_pixels: usize,
    pixels: &[ColorI],
    wc: &[[f32; 4]; 16],
    wr: f32,
    wg: f32,
    wb: f32,
    wa: f32,
    has_alpha: bool,
    n: usize,
    selectors_temp: &mut [i32; 16],
) -> f32 {
    use std::arch::x86_64::*;

    let mut wc0 = [0f32; 8];
    let mut wc1 = [0f32; 8];
    let mut wc2 = [0f32; 8];
    let mut wc3 = [0f32; 8];
    for k in 0..n {
        wc0[k] = wc[k][0];
        wc1[k] = wc[k][1];
        wc2[k] = wc[k][2];
        wc3[k] = wc[k][3];
    }
    let mut total_errf = 0f32;
    if n == 4 {
        let w0 = _mm_loadu_ps(wc0.as_ptr());
        let w1 = _mm_loadu_ps(wc1.as_ptr());
        let w2 = _mm_loadu_ps(wc2.as_ptr());
        let w3 = _mm_loadu_ps(wc3.as_ptr());
        let wrv = _mm_set1_ps(wr);
        let wgv = _mm_set1_ps(wg);
        let wbv = _mm_set1_ps(wb);
        let wav = _mm_set1_ps(wa);
        for i in 0..num_pixels {
            let d0 = _mm_sub_ps(w0, _mm_set1_ps(pixels[i].c[0] as f32));
            let d1 = _mm_sub_ps(w1, _mm_set1_ps(pixels[i].c[1] as f32));
            let d2 = _mm_sub_ps(w2, _mm_set1_ps(pixels[i].c[2] as f32));

            let mut err = _mm_add_ps(
                _mm_add_ps(
                    _mm_mul_ps(_mm_mul_ps(wrv, d0), d0),
                    _mm_mul_ps(_mm_mul_ps(wgv, d1), d1),
                ),
                _mm_mul_ps(_mm_mul_ps(wbv, d2), d2),
            );
            if has_alpha {
                let d3 = _mm_sub_ps(w3, _mm_set1_ps(pixels[i].c[3] as f32));
                err = _mm_add_ps(err, _mm_mul_ps(_mm_mul_ps(wav, d3), d3));
            }
            let m = _mm_min_ps(err, _mm_movehl_ps(err, err));
            let m = _mm_min_ss(m, _mm_shuffle_ps(m, m, 1));
            let best = _mm_cvtss_f32(m);
            let eq = _mm_movemask_ps(_mm_cmpeq_ps(err, _mm_set1_ps(best))) as u32;
            total_errf += best;
            selectors_temp[i] = (31 - eq.leading_zeros()) as i32;
        }
    } else {
        let w0 = _mm256_loadu_ps(wc0.as_ptr());
        let w1 = _mm256_loadu_ps(wc1.as_ptr());
        let w2 = _mm256_loadu_ps(wc2.as_ptr());
        let w3 = _mm256_loadu_ps(wc3.as_ptr());
        let wrv = _mm256_set1_ps(wr);
        let wgv = _mm256_set1_ps(wg);
        let wbv = _mm256_set1_ps(wb);
        let wav = _mm256_set1_ps(wa);
        for i in 0..num_pixels {
            let d0 = _mm256_sub_ps(w0, _mm256_set1_ps(pixels[i].c[0] as f32));
            let d1 = _mm256_sub_ps(w1, _mm256_set1_ps(pixels[i].c[1] as f32));
            let d2 = _mm256_sub_ps(w2, _mm256_set1_ps(pixels[i].c[2] as f32));
            let mut err = _mm256_add_ps(
                _mm256_add_ps(
                    _mm256_mul_ps(_mm256_mul_ps(wrv, d0), d0),
                    _mm256_mul_ps(_mm256_mul_ps(wgv, d1), d1),
                ),
                _mm256_mul_ps(_mm256_mul_ps(wbv, d2), d2),
            );
            if has_alpha {
                let d3 = _mm256_sub_ps(w3, _mm256_set1_ps(pixels[i].c[3] as f32));
                err = _mm256_add_ps(err, _mm256_mul_ps(_mm256_mul_ps(wav, d3), d3));
            }
            let best = hmin_ps256(err);
            let eq =
                _mm256_movemask_ps(_mm256_cmp_ps::<_CMP_EQ_OQ>(err, _mm256_set1_ps(best))) as u32;
            total_errf += best;
            selectors_temp[i] = (31 - eq.leading_zeros()) as i32;
        }
    }
    total_errf
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn eval_perceptual_avx2(
    num_pixels: usize,
    pixels: &[ColorI],
    wy: &[f32; 16],
    wcr: &[f32; 16],
    wcb: &[f32; 16],
    wa4: &[f32; 16],
    wr: f32,
    wgp: f32,
    wbp: f32,
    wa: f32,
    has_alpha: bool,
    n: usize,
    selectors_temp: &mut [i32; 16],
) -> f32 {
    use std::arch::x86_64::*;
    let mut total_errf = 0f32;
    if n == 4 {
        let wyv = _mm_loadu_ps(wy.as_ptr());
        let wcrv = _mm_loadu_ps(wcr.as_ptr());
        let wcbv = _mm_loadu_ps(wcb.as_ptr());
        let wav4 = _mm_loadu_ps(wa4.as_ptr());
        let wrv = _mm_set1_ps(wr);
        let wgpv = _mm_set1_ps(wgp);
        let wbpv = _mm_set1_ps(wbp);
        let wav = _mm_set1_ps(wa);
        for i in 0..num_pixels {
            let r = pixels[i].c[0] as f32;
            let g = pixels[i].c[1] as f32;
            let b = pixels[i].c[2] as f32;
            let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
            let cr = r - y;
            let cb = b - y;
            let dl = _mm_sub_ps(_mm_set1_ps(y), wyv);
            let dcr = _mm_sub_ps(_mm_set1_ps(cr), wcrv);
            let dcb = _mm_sub_ps(_mm_set1_ps(cb), wcbv);

            let mut err = _mm_add_ps(
                _mm_add_ps(
                    _mm_mul_ps(_mm_mul_ps(wrv, dl), dl),
                    _mm_mul_ps(_mm_mul_ps(wgpv, dcr), dcr),
                ),
                _mm_mul_ps(_mm_mul_ps(wbpv, dcb), dcb),
            );
            if has_alpha {
                let a = pixels[i].c[3] as f32;
                let da = _mm_sub_ps(_mm_set1_ps(a), wav4);
                err = _mm_add_ps(err, _mm_mul_ps(_mm_mul_ps(wav, da), da));
            }
            let m = _mm_min_ps(err, _mm_movehl_ps(err, err));
            let m = _mm_min_ss(m, _mm_shuffle_ps(m, m, 1));
            let best = _mm_cvtss_f32(m);
            let eq = _mm_movemask_ps(_mm_cmpeq_ps(err, _mm_set1_ps(best)));
            total_errf += best;
            selectors_temp[i] = eq.trailing_zeros() as i32;
        }
    } else {
        let rows = n / 8;
        let mut wyv = [_mm256_setzero_ps(); 2];
        let mut wcrv = [_mm256_setzero_ps(); 2];
        let mut wcbv = [_mm256_setzero_ps(); 2];
        let mut wav4 = [_mm256_setzero_ps(); 2];
        for rrow in 0..rows {
            wyv[rrow] = _mm256_loadu_ps(wy.as_ptr().add(rrow * 8));
            wcrv[rrow] = _mm256_loadu_ps(wcr.as_ptr().add(rrow * 8));
            wcbv[rrow] = _mm256_loadu_ps(wcb.as_ptr().add(rrow * 8));
            wav4[rrow] = _mm256_loadu_ps(wa4.as_ptr().add(rrow * 8));
        }
        let wrv = _mm256_set1_ps(wr);
        let wgpv = _mm256_set1_ps(wgp);
        let wbpv = _mm256_set1_ps(wbp);
        let wav = _mm256_set1_ps(wa);
        for i in 0..num_pixels {
            let r = pixels[i].c[0] as f32;
            let g = pixels[i].c[1] as f32;
            let b = pixels[i].c[2] as f32;
            let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
            let cr = r - y;
            let cb = b - y;
            let yv = _mm256_set1_ps(y);
            let crv = _mm256_set1_ps(cr);
            let cbv = _mm256_set1_ps(cb);
            let mut errs = [_mm256_setzero_ps(); 2];
            for rrow in 0..rows {
                let dl = _mm256_sub_ps(yv, wyv[rrow]);
                let dcr = _mm256_sub_ps(crv, wcrv[rrow]);
                let dcb = _mm256_sub_ps(cbv, wcbv[rrow]);
                let mut err = _mm256_add_ps(
                    _mm256_add_ps(
                        _mm256_mul_ps(_mm256_mul_ps(wrv, dl), dl),
                        _mm256_mul_ps(_mm256_mul_ps(wgpv, dcr), dcr),
                    ),
                    _mm256_mul_ps(_mm256_mul_ps(wbpv, dcb), dcb),
                );
                if has_alpha {
                    let a = pixels[i].c[3] as f32;
                    let da = _mm256_sub_ps(_mm256_set1_ps(a), wav4[rrow]);
                    err = _mm256_add_ps(err, _mm256_mul_ps(_mm256_mul_ps(wav, da), da));
                }
                errs[rrow] = err;
            }
            let combined = if rows == 2 {
                _mm256_min_ps(errs[0], errs[1])
            } else {
                errs[0]
            };
            let best = hmin_ps256(combined);
            let bv = _mm256_set1_ps(best);
            let mut mask = _mm256_movemask_ps(_mm256_cmp_ps::<_CMP_EQ_OQ>(errs[0], bv)) as u32;
            if rows == 2 {
                mask |= (_mm256_movemask_ps(_mm256_cmp_ps::<_CMP_EQ_OQ>(errs[1], bv)) as u32) << 8;
            }
            total_errf += best;
            selectors_temp[i] = mask.trailing_zeros() as i32;
        }
    }
    total_errf
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::ColorI;
    use std::arch::aarch64::*;

    #[inline]
    pub(super) unsafe fn build_wc_table_neon(
        wc: &mut [[f32; 4]; 16],
        psel: &[u32],
        n: usize,
        nc: usize,
    ) {
        let a = vld1q_f32(wc[0].as_ptr());
        let b = vld1q_f32(wc[n - 1].as_ptr());
        let v64 = vdupq_n_f32(64.0);
        let v32 = vdupq_n_f32(32.0);
        let inv64 = vdupq_n_f32(1.0 / 64.0);
        for i in 1..(n - 1) {
            let wv = vdupq_n_f32(psel[i] as f32);
            let iwv = vsubq_f32(v64, wv);
            let t = vaddq_f32(vaddq_f32(vmulq_f32(a, iwv), vmulq_f32(b, wv)), v32);
            let mut t = vrndmq_f32(vmulq_f32(t, inv64));
            if nc == 3 {
                t = vsetq_lane_f32::<3>(0.0, t);
            }
            vst1q_f32(wc[i].as_mut_ptr(), t);
        }
    }

    /// Perceptual (YCrCb) selector search, NEON port of `eval_perceptual_avx2`.
    ///
    /// ROWS = n/4 float32x4 candidate rows (n=4 -> 1, n=8 -> 2, n=16 -> 4).
    /// Op order mirrors the scalar loop exactly: no fused multiply-add anywhere,
    /// `total_errf` accumulates in ascending pixel order, and the selector is the
    /// LOWEST index whose error equals the row minimum (scalar uses strict `<`).
    #[inline(always)]
    unsafe fn eval_perceptual_impl<const ROWS: usize, const HAS_ALPHA: bool>(
        num_pixels: usize,
        pixels: &[ColorI],
        wc: &[[f32; 4]; 16],
        wr: f32,
        wgp: f32,
        wbp: f32,
        wa: f32,
        selectors_temp: &mut [i32; 16],
    ) -> f32 {
        let cy_r = vdupq_n_f32(0.2126);
        let cy_g = vdupq_n_f32(0.7152);
        let cy_b = vdupq_n_f32(0.0722);
        let mut wyv = [vdupq_n_f32(0.0); ROWS];
        let mut wcrv = [vdupq_n_f32(0.0); ROWS];
        let mut wcbv = [vdupq_n_f32(0.0); ROWS];
        let mut wav4 = [vdupq_n_f32(0.0); ROWS];
        let mut bitv = [vdupq_n_u32(0); ROWS];
        for row in 0..ROWS {
            let t = vld4q_f32(wc[row * 4].as_ptr());
            let y = vaddq_f32(
                vaddq_f32(vmulq_f32(t.0, cy_r), vmulq_f32(t.1, cy_g)),
                vmulq_f32(t.2, cy_b),
            );
            wyv[row] = y;
            wcrv[row] = vsubq_f32(t.0, y);
            wcbv[row] = vsubq_f32(t.2, y);
            wav4[row] = t.3;
            let base = 4 * row;
            let bits: [u32; 4] = [1 << base, 2 << base, 4 << base, 8 << base];
            bitv[row] = vld1q_u32(bits.as_ptr());
        }
        let wrv = vdupq_n_f32(wr);
        let wgpv = vdupq_n_f32(wgp);
        let wbpv = vdupq_n_f32(wbp);
        let wav = vdupq_n_f32(wa);

        let mut total_errf = 0f32;
        for i in 0..num_pixels {
            let r = pixels[i].c[0] as f32;
            let g = pixels[i].c[1] as f32;
            let b = pixels[i].c[2] as f32;
            let y = r * 0.2126 + g * 0.7152 + b * 0.0722;
            let cr = r - y;
            let cb = b - y;
            let yv = vdupq_n_f32(y);
            let crv = vdupq_n_f32(cr);
            let cbv = vdupq_n_f32(cb);
            let av = if HAS_ALPHA {
                vdupq_n_f32(pixels[i].c[3] as f32)
            } else {
                vdupq_n_f32(0.0)
            };

            let mut errs = [vdupq_n_f32(0.0); ROWS];
            for row in 0..ROWS {
                let dl = vsubq_f32(yv, wyv[row]);
                let dcr = vsubq_f32(crv, wcrv[row]);
                let dcb = vsubq_f32(cbv, wcbv[row]);
                let mut e = vaddq_f32(
                    vaddq_f32(
                        vmulq_f32(vmulq_f32(wrv, dl), dl),
                        vmulq_f32(vmulq_f32(wgpv, dcr), dcr),
                    ),
                    vmulq_f32(vmulq_f32(wbpv, dcb), dcb),
                );
                if HAS_ALPHA {
                    let da = vsubq_f32(av, wav4[row]);
                    e = vaddq_f32(e, vmulq_f32(vmulq_f32(wav, da), da));
                }
                errs[row] = e;
            }

            let mut comb = errs[0];
            for row in 1..ROWS {
                comb = vminq_f32(comb, errs[row]);
            }
            let best = vminvq_f32(comb);
            let bv = vdupq_n_f32(best);
            let mut mask = 0u32;
            for row in 0..ROWS {
                mask |= vaddvq_u32(vandq_u32(vceqq_f32(errs[row], bv), bitv[row]));
            }
            total_errf += best;
            selectors_temp[i] = mask.trailing_zeros() as i32;
        }
        total_errf
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(super) unsafe fn eval_perceptual_neon(
        num_pixels: usize,
        pixels: &[ColorI],
        wc: &[[f32; 4]; 16],
        wr: f32,
        wgp: f32,
        wbp: f32,
        wa: f32,
        has_alpha: bool,
        n: usize,
        selectors_temp: &mut [i32; 16],
    ) -> f32 {
        match (n, has_alpha) {
            (4, false) => eval_perceptual_impl::<1, false>(
                num_pixels,
                pixels,
                wc,
                wr,
                wgp,
                wbp,
                wa,
                selectors_temp,
            ),
            (4, true) => eval_perceptual_impl::<1, true>(
                num_pixels,
                pixels,
                wc,
                wr,
                wgp,
                wbp,
                wa,
                selectors_temp,
            ),
            (8, false) => eval_perceptual_impl::<2, false>(
                num_pixels,
                pixels,
                wc,
                wr,
                wgp,
                wbp,
                wa,
                selectors_temp,
            ),
            (8, true) => eval_perceptual_impl::<2, true>(
                num_pixels,
                pixels,
                wc,
                wr,
                wgp,
                wbp,
                wa,
                selectors_temp,
            ),
            (_, false) => eval_perceptual_impl::<4, false>(
                num_pixels,
                pixels,
                wc,
                wr,
                wgp,
                wbp,
                wa,
                selectors_temp,
            ),
            (_, true) => eval_perceptual_impl::<4, true>(
                num_pixels,
                pixels,
                wc,
                wr,
                wgp,
                wbp,
                wa,
                selectors_temp,
            ),
        }
    }

    #[inline]
    pub(super) unsafe fn eval_solution_n16_rgb_neon(
        num_pixels: usize,
        pixels: &[ColorI],
        wc: &[[f32; 4]; 16],
        wr: f32,
        wg: f32,
        wb: f32,
        dr: f32,
        dg: f32,
        db: f32,
        lr: f32,
        lg: f32,
        lb: f32,
        f: f32,
        n: usize,
        selectors_temp: &mut [i32; 16],
    ) -> f32 {
        let warr = [wr, wg, wb, 0.0f32];
        let w_v = vld1q_f32(warr.as_ptr());
        let mut total_errf = 0f32;
        let n_minus_1 = (n - 1) as f32;
        for i in 0..num_pixels {
            let r = pixels[i].c[0] as f32;
            let g = pixels[i].c[1] as f32;
            let b = pixels[i].c[2] as f32;
            let mut best_sel =
                ((((r * dr + lr) + (g * dg + lg) + (b * db + lb)) * f) + 0.5).floor();
            best_sel = best_sel.clamp(1.0, n_minus_1);
            let best_sel0 = best_sel - 1.0;
            let i0 = best_sel0 as i32 as usize;
            let i1 = best_sel as i32 as usize;

            let pi_v = vcvtq_f32_s32(vld1q_s32(pixels[i].c.as_ptr()));
            let d0_v = vsubq_f32(vld1q_f32(wc[i0].as_ptr()), pi_v);
            let d1_v = vsubq_f32(vld1q_f32(wc[i1].as_ptr()), pi_v);
            let t0 = vmulq_f32(vmulq_f32(w_v, d0_v), d0_v);
            let t1 = vmulq_f32(vmulq_f32(w_v, d1_v), d1_v);
            let err0 =
                (vgetq_lane_f32::<0>(t0) + vgetq_lane_f32::<1>(t0)) + vgetq_lane_f32::<2>(t0);
            let err1 =
                (vgetq_lane_f32::<0>(t1) + vgetq_lane_f32::<1>(t1)) + vgetq_lane_f32::<2>(t1);
            let min_err = err0.min(err1);
            total_errf += min_err;
            selectors_temp[i] = if min_err == err0 { best_sel0 } else { best_sel } as i32;
        }
        total_errf
    }

    #[inline]
    pub(super) unsafe fn eval_solution_n16_rgba_neon(
        num_pixels: usize,
        pixels: &[ColorI],
        wc: &[[f32; 4]; 16],
        wr: f32,
        wg: f32,
        wb: f32,
        wa: f32,
        dr: f32,
        dg: f32,
        db: f32,
        da: f32,
        lr: f32,
        lg: f32,
        lb: f32,
        la: f32,
        f: f32,
        n: usize,
        selectors_temp: &mut [i32; 16],
    ) -> f32 {
        let warr = [wr, wg, wb, wa];
        let w_v = vld1q_f32(warr.as_ptr());
        let mut total_errf = 0f32;
        let n_minus_1 = (n - 1) as f32;
        for i in 0..num_pixels {
            let r = pixels[i].c[0] as f32;
            let g = pixels[i].c[1] as f32;
            let b = pixels[i].c[2] as f32;
            let a = pixels[i].c[3] as f32;
            let mut best_sel =
                ((((r * dr + lr) + (g * dg + lg) + (b * db + lb) + (a * da + la)) * f) + 0.5)
                    .floor();
            best_sel = best_sel.clamp(1.0, n_minus_1);
            let best_sel0 = best_sel - 1.0;
            let i0 = best_sel0 as i32 as usize;
            let i1 = best_sel as i32 as usize;

            let pi_v = vcvtq_f32_s32(vld1q_s32(pixels[i].c.as_ptr()));
            let d0_v = vsubq_f32(vld1q_f32(wc[i0].as_ptr()), pi_v);
            let d1_v = vsubq_f32(vld1q_f32(wc[i1].as_ptr()), pi_v);
            let t0 = vmulq_f32(vmulq_f32(w_v, d0_v), d0_v);
            let t1 = vmulq_f32(vmulq_f32(w_v, d1_v), d1_v);
            let err0 = ((vgetq_lane_f32::<0>(t0) + vgetq_lane_f32::<1>(t0))
                + vgetq_lane_f32::<2>(t0))
                + vgetq_lane_f32::<3>(t0);
            let err1 = ((vgetq_lane_f32::<0>(t1) + vgetq_lane_f32::<1>(t1))
                + vgetq_lane_f32::<2>(t1))
                + vgetq_lane_f32::<3>(t1);
            let min_err = err0.min(err1);
            total_errf += min_err;
            selectors_temp[i] = if min_err == err0 { best_sel0 } else { best_sel } as i32;
        }
        total_errf
    }

    #[inline]
    pub(super) unsafe fn eval_discrete_neon(
        num_pixels: usize,
        pixels: &[ColorI],
        wc: &[[f32; 4]; 16],
        wr: f32,
        wg: f32,
        wb: f32,
        wa: f32,
        has_alpha: bool,
        n: usize,
        selectors_temp: &mut [i32; 16],
    ) -> f32 {
        let mut wc0 = [0f32; 8];
        let mut wc1 = [0f32; 8];
        let mut wc2 = [0f32; 8];
        let mut wc3 = [0f32; 8];
        for k in 0..n {
            wc0[k] = wc[k][0];
            wc1[k] = wc[k][1];
            wc2[k] = wc[k][2];
            wc3[k] = wc[k][3];
        }
        let wrv = vdupq_n_f32(wr);
        let wgv = vdupq_n_f32(wg);
        let wbv = vdupq_n_f32(wb);
        let wav = vdupq_n_f32(wa);
        let bits_lo_arr: [u32; 4] = [1, 2, 4, 8];
        let bits_hi_arr: [u32; 4] = [16, 32, 64, 128];
        let bits_lo = vld1q_u32(bits_lo_arr.as_ptr());
        let bits_hi = vld1q_u32(bits_hi_arr.as_ptr());
        let mut total_errf = 0f32;
        if n == 4 {
            let w0 = vld1q_f32(wc0.as_ptr());
            let w1 = vld1q_f32(wc1.as_ptr());
            let w2 = vld1q_f32(wc2.as_ptr());
            let w3 = vld1q_f32(wc3.as_ptr());
            for i in 0..num_pixels {
                let d0 = vsubq_f32(w0, vdupq_n_f32(pixels[i].c[0] as f32));
                let d1 = vsubq_f32(w1, vdupq_n_f32(pixels[i].c[1] as f32));
                let d2 = vsubq_f32(w2, vdupq_n_f32(pixels[i].c[2] as f32));
                let mut err = vaddq_f32(
                    vaddq_f32(
                        vmulq_f32(vmulq_f32(wrv, d0), d0),
                        vmulq_f32(vmulq_f32(wgv, d1), d1),
                    ),
                    vmulq_f32(vmulq_f32(wbv, d2), d2),
                );
                if has_alpha {
                    let d3 = vsubq_f32(w3, vdupq_n_f32(pixels[i].c[3] as f32));
                    err = vaddq_f32(err, vmulq_f32(vmulq_f32(wav, d3), d3));
                }
                let best = vminvq_f32(err);
                let eq = vceqq_f32(err, vdupq_n_f32(best));
                let mask = vaddvq_u32(vandq_u32(eq, bits_lo));
                total_errf += best;
                selectors_temp[i] = (31 - mask.leading_zeros()) as i32;
            }
        } else {
            let w0l = vld1q_f32(wc0.as_ptr());
            let w0h = vld1q_f32(wc0.as_ptr().add(4));
            let w1l = vld1q_f32(wc1.as_ptr());
            let w1h = vld1q_f32(wc1.as_ptr().add(4));
            let w2l = vld1q_f32(wc2.as_ptr());
            let w2h = vld1q_f32(wc2.as_ptr().add(4));
            let w3l = vld1q_f32(wc3.as_ptr());
            let w3h = vld1q_f32(wc3.as_ptr().add(4));
            for i in 0..num_pixels {
                let p0 = vdupq_n_f32(pixels[i].c[0] as f32);
                let p1 = vdupq_n_f32(pixels[i].c[1] as f32);
                let p2 = vdupq_n_f32(pixels[i].c[2] as f32);
                let d0l = vsubq_f32(w0l, p0);
                let d0h = vsubq_f32(w0h, p0);
                let d1l = vsubq_f32(w1l, p1);
                let d1h = vsubq_f32(w1h, p1);
                let d2l = vsubq_f32(w2l, p2);
                let d2h = vsubq_f32(w2h, p2);
                let mut errl = vaddq_f32(
                    vaddq_f32(
                        vmulq_f32(vmulq_f32(wrv, d0l), d0l),
                        vmulq_f32(vmulq_f32(wgv, d1l), d1l),
                    ),
                    vmulq_f32(vmulq_f32(wbv, d2l), d2l),
                );
                let mut errh = vaddq_f32(
                    vaddq_f32(
                        vmulq_f32(vmulq_f32(wrv, d0h), d0h),
                        vmulq_f32(vmulq_f32(wgv, d1h), d1h),
                    ),
                    vmulq_f32(vmulq_f32(wbv, d2h), d2h),
                );
                if has_alpha {
                    let p3 = vdupq_n_f32(pixels[i].c[3] as f32);
                    let d3l = vsubq_f32(w3l, p3);
                    let d3h = vsubq_f32(w3h, p3);
                    errl = vaddq_f32(errl, vmulq_f32(vmulq_f32(wav, d3l), d3l));
                    errh = vaddq_f32(errh, vmulq_f32(vmulq_f32(wav, d3h), d3h));
                }
                let best = vminvq_f32(vminq_f32(errl, errh));
                let bv = vdupq_n_f32(best);
                let mask = vaddvq_u32(vandq_u32(vceqq_f32(errl, bv), bits_lo))
                    | vaddvq_u32(vandq_u32(vceqq_f32(errh, bv), bits_hi));
                total_errf += best;
                selectors_temp[i] = (31 - mask.leading_zeros()) as i32;
            }
        }
        total_errf
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
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

    const WEIGHT_SETS: [[f32; 4]; 3] = [
        [1.0, 1.0, 1.0, 1.0],
        [2.0, 3.0, 5.0, 7.0],
        [128.0, 64.0, 16.0, 256.0],
    ];

    fn endpoints(rng: &mut Rng, case: usize) -> (ColorI, ColorI) {
        let mut lo = ColorI::default();
        let mut hi = ColorI::default();
        for j in 0..4 {
            lo.c[j] = match rng.next_u32() % 4 {
                0 => 0,
                1 => 255,
                _ => (rng.next_u32() % 256) as i32,
            };
            hi.c[j] = match rng.next_u32() % 4 {
                0 => 0,
                1 => 255,
                _ => (rng.next_u32() % 256) as i32,
            };
        }
        if case.is_multiple_of(9) {
            hi = lo;
        }
        (lo, hi)
    }

    fn build_wc_scalar(wc: &mut [[f32; 4]; 16], psel: &[u32], n: usize, nc: usize) {
        for i in 1..(n - 1) {
            for j in 0..nc {
                wc[i][j] =
                    ((wc[0][j] * (64.0 - psel[i] as f32) + wc[n - 1][j] * psel[i] as f32 + 32.0)
                        * (1.0 / 64.0))
                        .floor();
            }
        }
    }

    fn make_wc(lo: &ColorI, hi: &ColorI, psel: &[u32], n: usize, nc: usize) -> [[f32; 4]; 16] {
        let mut wc = [[0f32; 4]; 16];
        for j in 0..4 {
            wc[0][j] = lo.c[j] as f32;
            wc[n - 1][j] = hi.c[j] as f32;
        }
        build_wc_scalar(&mut wc, psel, n, nc);
        wc
    }

    /// Kind 2 places pixels on/between palette rows to stress min/argmin ties.
    fn gen_pixels(rng: &mut Rng, wc: &[[f32; 4]; 16], n: usize) -> [ColorI; 16] {
        let mut pixels = [ColorI::default(); 16];
        match rng.next_u32() % 5 {
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
            2 => {
                for px in pixels.iter_mut() {
                    let r0 = (rng.next_u32() as usize) % n;
                    let r1 = (rng.next_u32() as usize) % n;
                    for (j, v) in px.c.iter_mut().enumerate() {
                        *v = ((wc[r0][j] + wc[r1][j]) * 0.5) as i32;
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

    #[test]
    fn build_wc_table_neon_matches_scalar() {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let tabs: [(usize, &[u32]); 3] = [(4, &G_WEIGHTS2), (8, &G_WEIGHTS3), (16, &G_WEIGHTS4)];
        for case in 0..3000usize {
            let (n, psel) = tabs[case % 3];
            let nc = 3 + case % 2;
            let (lo, hi) = endpoints(&mut rng, case);
            let mut wc_s = [[0f32; 4]; 16];
            for j in 0..4 {
                wc_s[0][j] = lo.c[j] as f32;
                wc_s[n - 1][j] = hi.c[j] as f32;
            }
            let mut wc_n = wc_s;
            build_wc_scalar(&mut wc_s, psel, n, nc);
            unsafe { neon::build_wc_table_neon(&mut wc_n, psel, n, nc) };
            for i in 0..n {
                for j in 0..4 {
                    assert_eq!(
                        wc_s[i][j].to_bits(),
                        wc_n[i][j].to_bits(),
                        "row {i} comp {j} n={n} nc={nc} lo={:?} hi={:?}",
                        lo.c,
                        hi.c
                    );
                }
            }
        }
    }

    #[test]
    fn eval_solution_n16_rgb_neon_matches_scalar() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let n = 16usize;
        for case in 0..3000usize {
            let (lo, hi) = endpoints(&mut rng, case);
            let wc = make_wc(&lo, &hi, &G_WEIGHTS4, n, 3);
            let lr = lo.c[0] as f32;
            let lg = lo.c[1] as f32;
            let lb = lo.c[2] as f32;
            let dr = hi.c[0] as f32 - lr;
            let dg = hi.c[1] as f32 - lg;
            let db = hi.c[2] as f32 - lb;
            let f = n as f32 / (dr * dr + dg * dg + db * db);
            let (lr, lg, lb) = (lr * -dr, lg * -dg, lb * -db);
            let [wr, wg, wb, _] = WEIGHT_SETS[case % 3];
            let num_pixels = case % 16 + 1;
            let pixels = gen_pixels(&mut rng, &wc, n);
            let mut sel_s = [0i32; 16];
            let mut sel_n = [0i32; 16];
            let err_s = eval_solution_n16_rgb_scalar(
                num_pixels, &pixels, &wc, wr, wg, wb, dr, dg, db, lr, lg, lb, f, n, &mut sel_s,
            );
            let err_n = unsafe {
                neon::eval_solution_n16_rgb_neon(
                    num_pixels, &pixels, &wc, wr, wg, wb, dr, dg, db, lr, lg, lb, f, n, &mut sel_n,
                )
            };
            assert_eq!(
                err_s.to_bits(),
                err_n.to_bits(),
                "err mismatch lo={:?} hi={:?} pixels={:?}",
                lo.c,
                hi.c,
                pixels.map(|px| px.c)
            );
            assert_eq!(
                sel_s,
                sel_n,
                "selector mismatch lo={:?} hi={:?} pixels={:?}",
                lo.c,
                hi.c,
                pixels.map(|px| px.c)
            );
        }
    }

    fn eval_solution_n16_rgba_ref(
        num_pixels: usize,
        pixels: &[ColorI],
        wc: &[[f32; 4]; 16],
        wr: f32,
        wg: f32,
        wb: f32,
        wa: f32,
        dr: f32,
        dg: f32,
        db: f32,
        da: f32,
        lr: f32,
        lg: f32,
        lb: f32,
        la: f32,
        f: f32,
        n: usize,
        selectors_temp: &mut [i32; 16],
    ) -> f32 {
        let mut total_errf = 0f32;
        for i in 0..num_pixels {
            let r = pixels[i].c[0] as f32;
            let g = pixels[i].c[1] as f32;
            let b = pixels[i].c[2] as f32;
            let a = pixels[i].c[3] as f32;
            let mut best_sel =
                ((((r * dr + lr) + (g * dg + lg) + (b * db + lb) + (a * da + la)) * f) + 0.5)
                    .floor();
            best_sel = best_sel.clamp(1.0, (n - 1) as f32);
            let best_sel0 = best_sel - 1.0;
            let i0 = best_sel0 as i32 as usize;
            let i1 = best_sel as i32 as usize;
            let dr0 = wc[i0][0] - r;
            let dg0 = wc[i0][1] - g;
            let db0 = wc[i0][2] - b;
            let da0 = wc[i0][3] - a;
            let err0 = wr * dr0 * dr0 + wg * dg0 * dg0 + wb * db0 * db0 + wa * da0 * da0;
            let dr1 = wc[i1][0] - r;
            let dg1 = wc[i1][1] - g;
            let db1 = wc[i1][2] - b;
            let da1 = wc[i1][3] - a;
            let err1 = wr * dr1 * dr1 + wg * dg1 * dg1 + wb * db1 * db1 + wa * da1 * da1;
            let min_err = err0.min(err1);
            total_errf += min_err;
            selectors_temp[i] = if min_err == err0 { best_sel0 } else { best_sel } as i32;
        }
        total_errf
    }

    #[test]
    fn eval_solution_n16_rgba_neon_matches_scalar() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        let n = 16usize;
        for case in 0..3000usize {
            let (lo, hi) = endpoints(&mut rng, case);
            let wc = make_wc(&lo, &hi, &G_WEIGHTS4, n, 4);
            let lr = lo.c[0] as f32;
            let lg = lo.c[1] as f32;
            let lb = lo.c[2] as f32;
            let la = lo.c[3] as f32;
            let dr = hi.c[0] as f32 - lr;
            let dg = hi.c[1] as f32 - lg;
            let db = hi.c[2] as f32 - lb;
            let da = hi.c[3] as f32 - la;
            let f = n as f32 / (dr * dr + dg * dg + db * db + da * da);
            let (lr, lg, lb, la) = (lr * -dr, lg * -dg, lb * -db, la * -da);
            let [wr, wg, wb, wa] = WEIGHT_SETS[case % 3];
            let num_pixels = case % 16 + 1;
            let pixels = gen_pixels(&mut rng, &wc, n);
            let mut sel_s = [0i32; 16];
            let mut sel_n = [0i32; 16];
            let err_s = eval_solution_n16_rgba_ref(
                num_pixels, &pixels, &wc, wr, wg, wb, wa, dr, dg, db, da, lr, lg, lb, la, f, n,
                &mut sel_s,
            );
            let err_n = unsafe {
                neon::eval_solution_n16_rgba_neon(
                    num_pixels, &pixels, &wc, wr, wg, wb, wa, dr, dg, db, da, lr, lg, lb, la, f, n,
                    &mut sel_n,
                )
            };
            assert_eq!(
                err_s.to_bits(),
                err_n.to_bits(),
                "err mismatch lo={:?} hi={:?} pixels={:?}",
                lo.c,
                hi.c,
                pixels.map(|px| px.c)
            );
            assert_eq!(
                sel_s,
                sel_n,
                "selector mismatch lo={:?} hi={:?} pixels={:?}",
                lo.c,
                hi.c,
                pixels.map(|px| px.c)
            );
        }
    }

    fn eval_discrete_rgba_ref(
        num_pixels: usize,
        pixels: &[ColorI],
        wc: &[[f32; 4]; 16],
        wr: f32,
        wg: f32,
        wb: f32,
        wa: f32,
        n: usize,
        selectors_temp: &mut [i32; 16],
    ) -> f32 {
        let mut total_errf = 0f32;
        for i in 0..num_pixels {
            let pr = pixels[i].c[0] as f32;
            let pg = pixels[i].c[1] as f32;
            let pb = pixels[i].c[2] as f32;
            let pa = pixels[i].c[3] as f32;
            let mut errs = [0f32; 4];
            for k in 0..4usize {
                let d0 = wc[k][0] - pr;
                let d1 = wc[k][1] - pg;
                let d2 = wc[k][2] - pb;
                let d3 = wc[k][3] - pa;
                errs[k] = wr * d0 * d0 + wg * d1 * d1 + wb * d2 * d2 + wa * d3 * d3;
            }
            let mut best_err = errs[0].min(errs[1]).min(errs[2]).min(errs[3]);
            let mut best_sel = if best_err == errs[1] { 1 } else { 0 };
            if best_err == errs[2] {
                best_sel = 2;
            }
            if best_err == errs[3] {
                best_sel = 3;
            }
            if n == 8 {
                let mut e2 = [0f32; 4];
                for k in 0..4usize {
                    let d0 = wc[4 + k][0] - pr;
                    let d1 = wc[4 + k][1] - pg;
                    let d2 = wc[4 + k][2] - pb;
                    let d3 = wc[4 + k][3] - pa;
                    e2[k] = wr * d0 * d0 + wg * d1 * d1 + wb * d2 * d2 + wa * d3 * d3;
                }
                best_err = best_err.min(e2[0].min(e2[1]).min(e2[2]).min(e2[3]));
                if best_err == e2[0] {
                    best_sel = 4;
                }
                if best_err == e2[1] {
                    best_sel = 5;
                }
                if best_err == e2[2] {
                    best_sel = 6;
                }
                if best_err == e2[3] {
                    best_sel = 7;
                }
            }
            total_errf += best_err;
            selectors_temp[i] = best_sel;
        }
        total_errf
    }

    #[test]
    fn eval_discrete_neon_matches_scalar() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
        let tabs: [(usize, &[u32]); 2] = [(4, &G_WEIGHTS2), (8, &G_WEIGHTS3)];
        for case in 0..3000usize {
            let (n, psel) = tabs[case % 2];
            let has_alpha = case % 4 >= 2;
            let nc = if has_alpha { 4 } else { 3 };
            let (lo, hi) = endpoints(&mut rng, case);
            let wc = make_wc(&lo, &hi, psel, n, nc);
            let [wr, wg, wb, wa] = WEIGHT_SETS[case % 3];
            let num_pixels = case % 16 + 1;
            let pixels = gen_pixels(&mut rng, &wc, n);
            let mut sel_s = [0i32; 16];
            let mut sel_n = [0i32; 16];
            let err_s = if has_alpha {
                eval_discrete_rgba_ref(num_pixels, &pixels, &wc, wr, wg, wb, wa, n, &mut sel_s)
            } else if wr == 1.0 && wg == 1.0 && wb == 1.0 {
                eval_discrete_rgb_scalar::<false>(
                    num_pixels, &pixels, &wc, wr, wg, wb, n, &mut sel_s,
                )
            } else {
                eval_discrete_rgb_scalar::<true>(
                    num_pixels, &pixels, &wc, wr, wg, wb, n, &mut sel_s,
                )
            };
            let err_n = unsafe {
                neon::eval_discrete_neon(
                    num_pixels, &pixels, &wc, wr, wg, wb, wa, has_alpha, n, &mut sel_n,
                )
            };
            assert_eq!(
                err_s.to_bits(),
                err_n.to_bits(),
                "err mismatch n={n} alpha={has_alpha} lo={:?} hi={:?} pixels={:?}",
                lo.c,
                hi.c,
                pixels.map(|px| px.c)
            );
            assert_eq!(
                sel_s,
                sel_n,
                "selector mismatch n={n} alpha={has_alpha} lo={:?} hi={:?} pixels={:?}",
                lo.c,
                hi.c,
                pixels.map(|px| px.c)
            );
        }
    }
}
