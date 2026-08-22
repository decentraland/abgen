use super::*;

pub(super) struct LaneF32 {
    pub(super) r: [f32; 16],
    pub(super) g: [f32; 16],
    pub(super) b: [f32; 16],
    pub(super) a: [f32; 16],
}

impl LaneF32 {
    pub(super) fn new(pixels: &[ColorI; 16]) -> Self {
        let mut l = LaneF32 {
            r: [0.0; 16],
            g: [0.0; 16],
            b: [0.0; 16],
            a: [0.0; 16],
        };
        for i in 0..16 {
            l.r[i] = pixels[i].c[0] as f32;
            l.g[i] = pixels[i].c[1] as f32;
            l.b[i] = pixels[i].c[2] as f32;
            l.a[i] = pixels[i].c[3] as f32;
        }
        l
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,avx512f,avx512vl")]
#[inline]
unsafe fn perm16_vl(
    lo: std::arch::x86_64::__m256,
    hi: std::arch::x86_64::__m256,
    pix: std::arch::x86_64::__m256i,
) -> std::arch::x86_64::__m256 {
    std::arch::x86_64::_mm256_permutex2var_ps(lo, pix, hi)
}

/// AVX2 stand-in for `_mm256_permutex2var_ps`: `permutevar8x32` reads idx&7,
/// so select the hi half for idx >= 8.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn perm16_avx2(
    lo: std::arch::x86_64::__m256,
    hi: std::arch::x86_64::__m256,
    pix: std::arch::x86_64::__m256i,
) -> std::arch::x86_64::__m256 {
    use std::arch::x86_64::*;
    let hi_sel = _mm256_castsi256_ps(_mm256_cmpgt_epi32(pix, _mm256_set1_epi32(7)));
    _mm256_blendv_ps(
        _mm256_permutevar8x32_ps(lo, pix),
        _mm256_permutevar8x32_ps(hi, pix),
        hi_sel,
    )
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
struct EstPreRgb {
    r0: std::arch::x86_64::__m256,
    r1: std::arch::x86_64::__m256,
    g0: std::arch::x86_64::__m256,
    g1: std::arch::x86_64::__m256,
    b0: std::arch::x86_64::__m256,
    b1: std::arch::x86_64::__m256,
    v255: std::arch::x86_64::__m256,
    v0: std::arch::x86_64::__m256,
    wrv: std::arch::x86_64::__m256,
    wgv: std::arch::x86_64::__m256,
    wbv: std::arch::x86_64::__m256,
    invnv: std::arch::x86_64::__m256,
    halfv: std::arch::x86_64::__m256,
    onev: std::arch::x86_64::__m256,
    nm1: f32,
}

#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
struct EstPreRgba {
    r0: std::arch::x86_64::__m256,
    r1: std::arch::x86_64::__m256,
    g0: std::arch::x86_64::__m256,
    g1: std::arch::x86_64::__m256,
    b0: std::arch::x86_64::__m256,
    b1: std::arch::x86_64::__m256,
    a0: std::arch::x86_64::__m256,
    a1: std::arch::x86_64::__m256,
    v255: std::arch::x86_64::__m256,
    v0: std::arch::x86_64::__m256,
    wrv: std::arch::x86_64::__m256,
    wgv: std::arch::x86_64::__m256,
    wbv: std::arch::x86_64::__m256,
    wav: std::arch::x86_64::__m256,
    invnv: std::arch::x86_64::__m256,
    halfv: std::arch::x86_64::__m256,
    onev: std::arch::x86_64::__m256,
    nm1: f32,
}

/// Generates the batched partition-estimation kernels once per x86 feature
/// level; the two instantiations differ only in the 16-lane permute. Every
/// arithmetic op is elementwise-identical to the scalar reference in
/// estimate.rs, so all variants stay bit-exact (probed by `qualified`).
#[cfg(target_arch = "x86_64")]
macro_rules! est_x86_kernels {
    ($feat:literal, $perm:ident, $pre_rgb:ident, $pre_rgba:ident, $err_rgb:ident,
     $err_rgba:ident, $est_idx:ident, $est_mode7:ident, $lane_fn:ident, $list_fn:ident) => {
        #[target_feature(enable = $feat)]
        unsafe fn $pre_rgb(mode: usize, p: &CCParams, lf: &LaneF32) -> EstPreRgb {
            use std::arch::x86_64::*;
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
            EstPreRgb {
                r0: _mm256_loadu_ps(lf.r.as_ptr()),
                r1: _mm256_loadu_ps(lf.r.as_ptr().add(8)),
                g0: _mm256_loadu_ps(lf.g.as_ptr()),
                g1: _mm256_loadu_ps(lf.g.as_ptr().add(8)),
                b0: _mm256_loadu_ps(lf.b.as_ptr()),
                b1: _mm256_loadu_ps(lf.b.as_ptr().add(8)),
                v255: _mm256_set1_ps(255.0),
                v0: _mm256_setzero_ps(),
                wrv: _mm256_set1_ps(wr),
                wgv: _mm256_set1_ps(wg),
                wbv: _mm256_set1_ps(wb),
                invnv: _mm256_set1_ps(inv_n),
                halfv: _mm256_set1_ps(0.5),
                onev: _mm256_set1_ps(1.0),
                nm1,
            }
        }

        #[target_feature(enable = $feat)]
        unsafe fn $pre_rgba(p: &CCParams, lf: &LaneF32) -> EstPreRgba {
            use std::arch::x86_64::*;
            let n = 4f32;
            let nm1 = n - 1.0;
            let inv_n = 1.0 / nm1;
            let (wr, wg, wb, wa) = if !p.perceptual
                && (p.weights[0] != 1
                    || p.weights[1] != 1
                    || p.weights[2] != 1
                    || p.weights[3] != 1)
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
            EstPreRgba {
                r0: _mm256_loadu_ps(lf.r.as_ptr()),
                r1: _mm256_loadu_ps(lf.r.as_ptr().add(8)),
                g0: _mm256_loadu_ps(lf.g.as_ptr()),
                g1: _mm256_loadu_ps(lf.g.as_ptr().add(8)),
                b0: _mm256_loadu_ps(lf.b.as_ptr()),
                b1: _mm256_loadu_ps(lf.b.as_ptr().add(8)),
                a0: _mm256_loadu_ps(lf.a.as_ptr()),
                a1: _mm256_loadu_ps(lf.a.as_ptr().add(8)),
                v255: _mm256_set1_ps(255.0),
                v0: _mm256_setzero_ps(),
                wrv: _mm256_set1_ps(wr),
                wgv: _mm256_set1_ps(wg),
                wbv: _mm256_set1_ps(wb),
                wav: _mm256_set1_ps(wa),
                invnv: _mm256_set1_ps(inv_n),
                halfv: _mm256_set1_ps(0.5),
                onev: _mm256_set1_ps(1.0),
                nm1,
            }
        }

        #[target_feature(enable = $feat)]
        #[inline]
        unsafe fn $err_rgb(pre: &EstPreRgb, idxs: &[i32; 16], num_pixels: usize) -> u64 {
            use std::arch::x86_64::*;
            if num_pixels == 0 {
                return 0;
            }
            let lane = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
            let nchunks = num_pixels.div_ceil(8);
            let mut rv = [_mm256_setzero_ps(); 2];
            let mut gv = [_mm256_setzero_ps(); 2];
            let mut bv = [_mm256_setzero_ps(); 2];

            let v255 = pre.v255;
            let v0 = pre.v0;
            let (mut minr, mut ming, mut minb) = (v255, v255, v255);
            let (mut maxr, mut maxg, mut maxb) = (v0, v0, v0);
            for c in 0..nchunks {
                let pix = _mm256_loadu_si256(idxs.as_ptr().add(c * 8) as *const __m256i);
                rv[c] = $perm(pre.r0, pre.r1, pix);
                gv[c] = $perm(pre.g0, pre.g1, pix);
                bv[c] = $perm(pre.b0, pre.b1, pix);
                let valid = _mm256_castsi256_ps(_mm256_cmpgt_epi32(
                    _mm256_set1_epi32((num_pixels - c * 8) as i32),
                    lane,
                ));
                minr = _mm256_min_ps(minr, _mm256_blendv_ps(v255, rv[c], valid));
                ming = _mm256_min_ps(ming, _mm256_blendv_ps(v255, gv[c], valid));
                minb = _mm256_min_ps(minb, _mm256_blendv_ps(v255, bv[c], valid));
                maxr = _mm256_max_ps(maxr, _mm256_blendv_ps(v0, rv[c], valid));
                maxg = _mm256_max_ps(maxg, _mm256_blendv_ps(v0, gv[c], valid));
                maxb = _mm256_max_ps(maxb, _mm256_blendv_ps(v0, bv[c], valid));
            }
            let lr = hmin_ps256(minr);
            let lg = hmin_ps256(ming);
            let lb = hmin_ps256(minb);
            let hr = hmax_ps256(maxr);
            let hg = hmax_ps256(maxg);
            let hb = hmax_ps256(maxb);

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

            let farv = _mm256_set1_ps(far);
            let fagv = _mm256_set1_ps(fag);
            let fabv = _mm256_set1_ps(fab);
            let lowv = _mm256_set1_ps(low);
            let scalev = _mm256_set1_ps(scale);
            let srv = _mm256_set1_ps(sr);
            let sgv = _mm256_set1_ps(sg);
            let sbv = _mm256_set1_ps(sb);
            let dirv = _mm256_set1_ps(dir);
            let digv = _mm256_set1_ps(dig);
            let dibv = _mm256_set1_ps(dib);

            let mut total_errf = 0f32;
            let mut t_arr = [0f32; 8];
            for c in 0..nchunks {
                let d = _mm256_add_ps(
                    _mm256_add_ps(_mm256_mul_ps(farv, rv[c]), _mm256_mul_ps(fagv, gv[c])),
                    _mm256_mul_ps(fabv, bv[c]),
                );
                let t1 = _mm256_add_ps(_mm256_mul_ps(_mm256_sub_ps(d, lowv), scalev), pre.halfv);
                let s0 = _mm256_mul_ps(_mm256_floor_ps(t1), pre.invnv);
                let lt = _mm256_cmp_ps::<_CMP_LT_OQ>(s0, v0);
                let s1 = _mm256_blendv_ps(s0, v0, lt);
                let gt = _mm256_cmp_ps::<_CMP_GT_OQ>(s0, pre.onev);
                let s = _mm256_blendv_ps(s1, pre.onev, gt);
                let itr = _mm256_add_ps(srv, _mm256_mul_ps(dirv, s));
                let itg = _mm256_add_ps(sgv, _mm256_mul_ps(digv, s));
                let itb = _mm256_add_ps(sbv, _mm256_mul_ps(dibv, s));
                let dr = _mm256_sub_ps(itr, rv[c]);
                let dg = _mm256_sub_ps(itg, gv[c]);
                let db = _mm256_sub_ps(itb, bv[c]);
                let term = _mm256_add_ps(
                    _mm256_add_ps(
                        _mm256_mul_ps(_mm256_mul_ps(pre.wrv, dr), dr),
                        _mm256_mul_ps(_mm256_mul_ps(pre.wgv, dg), dg),
                    ),
                    _mm256_mul_ps(_mm256_mul_ps(pre.wbv, db), db),
                );
                _mm256_storeu_ps(t_arr.as_mut_ptr(), term);
                let cnt = (num_pixels - c * 8).min(8);
                if cnt == 8 {
                    total_errf += t_arr[0];
                    total_errf += t_arr[1];
                    total_errf += t_arr[2];
                    total_errf += t_arr[3];
                    total_errf += t_arr[4];
                    total_errf += t_arr[5];
                    total_errf += t_arr[6];
                    total_errf += t_arr[7];
                } else {
                    for &t in &t_arr[..cnt] {
                        total_errf += t;
                    }
                }
            }
            total_errf as i64 as u64
        }

        #[target_feature(enable = $feat)]
        #[inline]
        unsafe fn $err_rgba(pre: &EstPreRgba, idxs: &[i32; 16], num_pixels: usize) -> u64 {
            use std::arch::x86_64::*;
            if num_pixels == 0 {
                return 0;
            }
            let lane = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
            let nchunks = num_pixels.div_ceil(8);
            let mut rv = [_mm256_setzero_ps(); 2];
            let mut gv = [_mm256_setzero_ps(); 2];
            let mut bv = [_mm256_setzero_ps(); 2];
            let mut av = [_mm256_setzero_ps(); 2];

            let v255 = pre.v255;
            let v0 = pre.v0;
            let (mut minr, mut ming, mut minb, mut mina) = (v255, v255, v255, v255);
            let (mut maxr, mut maxg, mut maxb, mut maxa) = (v0, v0, v0, v0);
            for c in 0..nchunks {
                let pix = _mm256_loadu_si256(idxs.as_ptr().add(c * 8) as *const __m256i);
                rv[c] = $perm(pre.r0, pre.r1, pix);
                gv[c] = $perm(pre.g0, pre.g1, pix);
                bv[c] = $perm(pre.b0, pre.b1, pix);
                av[c] = $perm(pre.a0, pre.a1, pix);
                let valid = _mm256_castsi256_ps(_mm256_cmpgt_epi32(
                    _mm256_set1_epi32((num_pixels - c * 8) as i32),
                    lane,
                ));
                minr = _mm256_min_ps(minr, _mm256_blendv_ps(v255, rv[c], valid));
                ming = _mm256_min_ps(ming, _mm256_blendv_ps(v255, gv[c], valid));
                minb = _mm256_min_ps(minb, _mm256_blendv_ps(v255, bv[c], valid));
                mina = _mm256_min_ps(mina, _mm256_blendv_ps(v255, av[c], valid));
                maxr = _mm256_max_ps(maxr, _mm256_blendv_ps(v0, rv[c], valid));
                maxg = _mm256_max_ps(maxg, _mm256_blendv_ps(v0, gv[c], valid));
                maxb = _mm256_max_ps(maxb, _mm256_blendv_ps(v0, bv[c], valid));
                maxa = _mm256_max_ps(maxa, _mm256_blendv_ps(v0, av[c], valid));
            }
            let lr = hmin_ps256(minr);
            let lg = hmin_ps256(ming);
            let lb = hmin_ps256(minb);
            let la = hmin_ps256(mina);
            let hr = hmax_ps256(maxr);
            let hg = hmax_ps256(maxg);
            let hb = hmax_ps256(maxb);
            let ha = hmax_ps256(maxa);

            let (sr, sg, sb, sa) = (lr, lg, lb, la);
            let dir = hr - lr;
            let dig = hg - lg;
            let dib = hb - lb;
            let dia = ha - la;
            let (far, fag, fab, faa) = (dir, dig, dib, dia);
            let low = far * sr + fag * sg + fab * sb + faa * sa;
            let high = far * hr + fag * hg + fab * hb + faa * ha;
            let scale = pre.nm1 / (high - low);

            let farv = _mm256_set1_ps(far);
            let fagv = _mm256_set1_ps(fag);
            let fabv = _mm256_set1_ps(fab);
            let faav = _mm256_set1_ps(faa);
            let lowv = _mm256_set1_ps(low);
            let scalev = _mm256_set1_ps(scale);
            let srv = _mm256_set1_ps(sr);
            let sgv = _mm256_set1_ps(sg);
            let sbv = _mm256_set1_ps(sb);
            let sav = _mm256_set1_ps(sa);
            let dirv = _mm256_set1_ps(dir);
            let digv = _mm256_set1_ps(dig);
            let dibv = _mm256_set1_ps(dib);
            let diav = _mm256_set1_ps(dia);

            let mut total_errf = 0f32;
            let mut t_arr = [0f32; 8];
            for c in 0..nchunks {
                let d = _mm256_add_ps(
                    _mm256_add_ps(
                        _mm256_add_ps(_mm256_mul_ps(farv, rv[c]), _mm256_mul_ps(fagv, gv[c])),
                        _mm256_mul_ps(fabv, bv[c]),
                    ),
                    _mm256_mul_ps(faav, av[c]),
                );
                let t1 = _mm256_add_ps(_mm256_mul_ps(_mm256_sub_ps(d, lowv), scalev), pre.halfv);
                let s0 = _mm256_mul_ps(_mm256_floor_ps(t1), pre.invnv);
                let lt = _mm256_cmp_ps::<_CMP_LT_OQ>(s0, v0);
                let s1 = _mm256_blendv_ps(s0, v0, lt);
                let gt = _mm256_cmp_ps::<_CMP_GT_OQ>(s0, pre.onev);
                let s = _mm256_blendv_ps(s1, pre.onev, gt);
                let dr = _mm256_sub_ps(_mm256_add_ps(srv, _mm256_mul_ps(dirv, s)), rv[c]);
                let dg = _mm256_sub_ps(_mm256_add_ps(sgv, _mm256_mul_ps(digv, s)), gv[c]);
                let db = _mm256_sub_ps(_mm256_add_ps(sbv, _mm256_mul_ps(dibv, s)), bv[c]);
                let da = _mm256_sub_ps(_mm256_add_ps(sav, _mm256_mul_ps(diav, s)), av[c]);
                let term = _mm256_add_ps(
                    _mm256_add_ps(
                        _mm256_add_ps(
                            _mm256_mul_ps(_mm256_mul_ps(pre.wrv, dr), dr),
                            _mm256_mul_ps(_mm256_mul_ps(pre.wgv, dg), dg),
                        ),
                        _mm256_mul_ps(_mm256_mul_ps(pre.wbv, db), db),
                    ),
                    _mm256_mul_ps(_mm256_mul_ps(pre.wav, da), da),
                );
                _mm256_storeu_ps(t_arr.as_mut_ptr(), term);
                let cnt = (num_pixels - c * 8).min(8);
                if cnt == 8 {
                    total_errf += t_arr[0];
                    total_errf += t_arr[1];
                    total_errf += t_arr[2];
                    total_errf += t_arr[3];
                    total_errf += t_arr[4];
                    total_errf += t_arr[5];
                    total_errf += t_arr[6];
                    total_errf += t_arr[7];
                } else {
                    for &t in &t_arr[..cnt] {
                        total_errf += t;
                    }
                }
            }
            total_errf as i64 as u64
        }

        #[target_feature(enable = $feat)]
        unsafe fn $est_idx(
            mode: usize,
            p: &CCParams,
            idxs: &[i32; 16],
            num_pixels: usize,
            lf: &LaneF32,
        ) -> u64 {
            let pre = $pre_rgb(mode, p, lf);
            $err_rgb(&pre, idxs, num_pixels)
        }

        #[target_feature(enable = $feat)]
        unsafe fn $est_mode7(
            p: &CCParams,
            idxs: &[i32; 16],
            num_pixels: usize,
            lf: &LaneF32,
        ) -> u64 {
            let pre = $pre_rgba(p, lf);
            $err_rgba(&pre, idxs, num_pixels)
        }

        #[target_feature(enable = $feat)]
        unsafe fn $lane_fn(
            mode: usize,
            p: &CCParams,
            lf: &LaneF32,
            tab: &[SubsetIdx; 64],
            total_partitions: u32,
            total_subsets: usize,
        ) -> u32 {
            debug_assert!(mode != 7);
            let pre = $pre_rgb(mode, p, lf);
            let mut best_err = u64::MAX;
            let mut best_partition = 0u32;
            for partition in 0..total_partitions {
                let si = &tab[partition as usize];
                let mut total_subset_err = 0u64;
                for subset in 0..total_subsets {
                    let err = $err_rgb(&pre, &si.idx[subset], si.total[subset]);
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

        #[target_feature(enable = $feat)]
        #[allow(clippy::too_many_arguments)]
        unsafe fn $list_fn(
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
            let pre_rgb = if mode != 7 {
                Some($pre_rgb(mode, p, lf))
            } else {
                None
            };
            let pre_rgba = if mode == 7 {
                Some($pre_rgba(p, lf))
            } else {
                None
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
                    let err = if let Some(pre) = &pre_rgba {
                        $err_rgba(pre, &si.idx[subset], si.total[subset])
                    } else {
                        $err_rgb(
                            pre_rgb.as_ref().unwrap_unchecked(),
                            &si.idx[subset],
                            si.total[subset],
                        )
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
    };
}

#[cfg(target_arch = "x86_64")]
est_x86_kernels!(
    "avx2,avx512f,avx512vl",
    perm16_vl,
    est_pre_rgb_vl,
    est_pre_rgba_vl,
    subset_err_rgb_pre_vl,
    subset_err_rgba_pre_vl,
    est_idx_lane_vl,
    est_mode7_idx_lane_vl,
    est_partition_lane_vl,
    est_partition_list_lane_vl
);

#[cfg(target_arch = "x86_64")]
est_x86_kernels!(
    "avx2",
    perm16_avx2,
    est_pre_rgb_avx2,
    est_pre_rgba_avx2,
    subset_err_rgb_pre_avx2,
    subset_err_rgba_pre_avx2,
    est_idx_lane_avx2,
    est_mode7_idx_lane_avx2,
    est_partition_lane_avx2,
    est_partition_list_lane_avx2
);

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn est_idx_lane(
    mode: usize,
    p: &CCParams,
    idxs: &[i32; 16],
    num_pixels: usize,
    lf: &LaneF32,
) -> u64 {
    if has_avx512vl() {
        est_idx_lane_vl(mode, p, idxs, num_pixels, lf)
    } else {
        est_idx_lane_avx2(mode, p, idxs, num_pixels, lf)
    }
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn est_mode7_idx_lane(
    p: &CCParams,
    idxs: &[i32; 16],
    num_pixels: usize,
    lf: &LaneF32,
) -> u64 {
    if has_avx512vl() {
        est_mode7_idx_lane_vl(p, idxs, num_pixels, lf)
    } else {
        est_mode7_idx_lane_avx2(p, idxs, num_pixels, lf)
    }
}

#[cfg(target_arch = "x86_64")]
pub(super) unsafe fn est_partition_lane_vperm(
    mode: usize,
    p: &CCParams,
    lf: &LaneF32,
    tab: &[SubsetIdx; 64],
    total_partitions: u32,
    total_subsets: usize,
) -> u32 {
    if has_avx512vl() {
        est_partition_lane_vl(mode, p, lf, tab, total_partitions, total_subsets)
    } else {
        est_partition_lane_avx2(mode, p, lf, tab, total_partitions, total_subsets)
    }
}

#[cfg(target_arch = "x86_64")]
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn est_partition_list_lane_vperm(
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
    if has_avx512vl() {
        est_partition_list_lane_vl(
            mode,
            p,
            lf,
            tab,
            part_lo,
            part_hi,
            total_subsets,
            solutions,
            num_solutions,
            max_solutions,
        )
    } else {
        est_partition_list_lane_avx2(
            mode,
            p,
            lf,
            tab,
            part_lo,
            part_hi,
            total_subsets,
            solutions,
            num_solutions,
            max_solutions,
        )
    }
}

#[inline]
pub(super) fn est_subset_err(
    mode: usize,
    p: &CCParams,
    idxs: &[i32; 16],
    num_pixels: usize,
    pixels: &[ColorI; 16],
    lf: Option<&LaneF32>,
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    if let Some(lf) = lf {
        unsafe {
            return if mode == 7 {
                est_mode7_idx_lane(p, idxs, num_pixels, lf)
            } else {
                est_idx_lane(mode, p, idxs, num_pixels, lf)
            };
        }
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    if let Some(lf) = lf {
        return if mode == 7 {
            super::est_wasm128::est_mode7_idx_w128(p, idxs, num_pixels, lf)
        } else {
            super::est_wasm128::est_idx_w128(mode, p, idxs, num_pixels, lf)
        };
    }
    #[cfg(target_arch = "aarch64")]
    if let Some(lf) = lf {
        return if mode == 7 {
            super::est_neon::est_mode7_idx_neon(p, idxs, num_pixels, lf)
        } else {
            super::est_neon::est_idx_neon(mode, p, idxs, num_pixels, lf)
        };
    }
    let _ = lf;
    if mode == 7 {
        ccc_est_mode7_idx(p, idxs, num_pixels, pixels)
    } else {
        ccc_est_idx(mode, p, idxs, num_pixels, pixels)
    }
}

pub(super) fn lanes_f32_if_supported(lanes: &[&[ColorI; 16]]) -> Option<Vec<LaneF32>> {
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    if super::est_wasm128::qualified() {
        return Some(lanes.iter().map(|p| LaneF32::new(p)).collect());
    }
    #[cfg(target_arch = "aarch64")]
    if super::est_neon::qualified() {
        return Some(lanes.iter().map(|p| LaneF32::new(p)).collect());
    }
    if qualified() {
        Some(lanes.iter().map(|p| LaneF32::new(p)).collect())
    } else {
        None
    }
}

/// Startup gate for the x86 batched fast path: the active kernel set (VL or
/// plain AVX2, per `has_avx512vl`) must reproduce the scalar reference
/// bit-exactly or we stay on the scalar path.
#[cfg(target_arch = "x86_64")]
pub(super) fn qualified() -> bool {
    static Q: OnceLock<bool> = OnceLock::new();
    *Q.get_or_init(probe_matches_scalar)
}

#[cfg(not(target_arch = "x86_64"))]
#[inline]
pub(super) fn qualified() -> bool {
    false
}

#[cfg(target_arch = "x86_64")]
fn xs(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[cfg(target_arch = "x86_64")]
fn probe_matches_scalar() -> bool {
    if !has_avx2() {
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
        let ok = unsafe {
            if mode == 7 {
                est_mode7_idx_lane(&p, &idxs, num_pixels, &lf)
                    == ccc_est_mode7_idx_scalar(&p, &idxs, num_pixels, &pixels)
            } else {
                est_idx_lane(mode, &p, &idxs, num_pixels, &lf)
                    == ccc_est_idx_scalar(mode, &p, &idxs, num_pixels, &pixels)
            }
        };
        if !ok {
            return false;
        }
    }
    true
}

#[cfg(all(test, target_arch = "x86_64"))]
mod x86_est_tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn next_u32(&mut self) -> u32 {
            self.next_u64() as u32
        }
    }

    fn params(rng: &mut Rng) -> CCParams {
        let mut p = CCParams::clear();
        p.weights = match rng.next_u32() % 4 {
            0 => [1, 1, 1, 1],
            1 => [128, 64, 16, 256],
            2 => [2, 3, 5, 7],
            _ => [37, 1, 250, 128],
        };
        p.perceptual = rng.next_u32().is_multiple_of(2);
        p
    }

    fn gen_pixels(rng: &mut Rng) -> [ColorI; 16] {
        let mut pixels = [ColorI::default(); 16];
        for px in pixels.iter_mut() {
            px.c = [
                (rng.next_u32() & 0xff) as i32,
                (rng.next_u32() & 0xff) as i32,
                (rng.next_u32() & 0xff) as i32,
                (rng.next_u32() & 0xff) as i32,
            ];
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
    fn est_idx_avx2_matches_scalar() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        for case in 0..4000usize {
            let mode = (rng.next_u32() % 4) as usize;
            let num_pixels = case % 16 + 1;
            let p = params(&mut rng);
            let pixels = gen_pixels(&mut rng);
            let idxs = gen_idxs(&mut rng);
            let lf = LaneF32::new(&pixels);
            assert_eq!(
                unsafe { est_idx_lane_avx2(mode, &p, &idxs, num_pixels, &lf) },
                ccc_est_idx_scalar(mode, &p, &idxs, num_pixels, &pixels),
                "mode={mode} n={num_pixels} w={:?} pixels={:?} idxs={idxs:?}",
                p.weights,
                pixels.map(|px| px.c)
            );
        }
    }

    #[test]
    fn est_mode7_idx_avx2_matches_scalar() {
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for case in 0..4000usize {
            let num_pixels = case % 16 + 1;
            let p = params(&mut rng);
            let pixels = gen_pixels(&mut rng);
            let idxs = gen_idxs(&mut rng);
            let lf = LaneF32::new(&pixels);
            assert_eq!(
                unsafe { est_mode7_idx_lane_avx2(&p, &idxs, num_pixels, &lf) },
                ccc_est_mode7_idx_scalar(&p, &idxs, num_pixels, &pixels),
                "n={num_pixels} w={:?} perceptual={} pixels={:?} idxs={idxs:?}",
                p.weights,
                p.perceptual,
                pixels.map(|px| px.c)
            );
        }
    }

    #[test]
    fn est_idx_vl_matches_scalar() {
        if !std::is_x86_feature_detected!("avx512f") || !std::is_x86_feature_detected!("avx512vl") {
            return;
        }
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        for case in 0..4000usize {
            let mode = (rng.next_u32() % 4) as usize;
            let num_pixels = case % 16 + 1;
            let p = params(&mut rng);
            let pixels = gen_pixels(&mut rng);
            let idxs = gen_idxs(&mut rng);
            let lf = LaneF32::new(&pixels);
            assert_eq!(
                unsafe { est_idx_lane_vl(mode, &p, &idxs, num_pixels, &lf) },
                ccc_est_idx_scalar(mode, &p, &idxs, num_pixels, &pixels),
                "mode={mode} n={num_pixels} w={:?}",
                p.weights,
            );
            assert_eq!(
                unsafe { est_mode7_idx_lane_vl(&p, &idxs, num_pixels, &lf) },
                ccc_est_mode7_idx_scalar(&p, &idxs, num_pixels, &pixels),
                "mode7 n={num_pixels} w={:?}",
                p.weights,
            );
        }
    }

    #[test]
    fn probe_qualifies_with_avx2() {
        assert_eq!(qualified(), has_avx2());
    }
}
