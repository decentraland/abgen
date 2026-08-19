use super::*;

#[derive(Clone, Copy, Default)]
pub(super) struct ColorI {
    pub(super) c: [i32; 4],
}
#[derive(Clone, Copy, Default)]
pub(super) struct Vec4F {
    pub(super) c: [f32; 4],
}

#[inline]
pub(super) const fn saturate(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

#[inline]
pub(super) fn vec4f_dot(a: &Vec4F, b: &Vec4F) -> f32 {
    a.c[0] * b.c[0] + a.c[1] * b.c[1] + a.c[2] * b.c[2] + a.c[3] * b.c[3]
}
#[inline]
pub(super) fn vec4f_normalize(v: &mut Vec4F) {
    let mut s = v.c[0] * v.c[0] + v.c[1] * v.c[1] + v.c[2] * v.c[2] + v.c[3] * v.c[3];
    if s != 0.0 {
        s = 1.0 / s.sqrt();
        v.c[0] *= s;
        v.c[1] *= s;
        v.c[2] *= s;
        v.c[3] *= s;
    }
}

#[inline]
pub(super) const fn iabs32(v: i32) -> i32 {
    v.abs()
}

#[inline]
pub(super) const fn itrunc(f: f32) -> i32 {
    f as i32
}

#[derive(Clone)]
pub(super) struct CCParams {
    pub(super) num_selector_weights: u32,
    pub(super) psel_weights: &'static [u32],
    pub(super) psel_weightsx: &'static [[f32; 4]],
    pub(super) comp_bits: u32,
    pub(super) weights: [u32; 4],
    pub(super) has_alpha: bool,
    pub(super) has_pbits: bool,
    pub(super) endpoints_share_pbit: bool,
    pub(super) perceptual: bool,
}
impl CCParams {
    pub(super) const fn clear() -> Self {
        CCParams {
            num_selector_weights: 0,
            psel_weights: &G_WEIGHTS2,
            psel_weightsx: &G_WEIGHTS2X,
            comp_bits: 0,
            weights: [1, 1, 1, 1],
            has_alpha: false,
            has_pbits: false,
            endpoints_share_pbit: false,
            perceptual: false,
        }
    }
}

#[derive(Clone)]
pub(super) struct CCResults {
    pub(super) best_overall_err: u64,
    pub(super) low: ColorI,
    pub(super) high: ColorI,
    pub(super) pbits: [u32; 2],
    pub(super) selectors: [i32; 16],
    pub(super) selectors_temp: [i32; 16],
}
impl CCResults {
    pub(super) fn new() -> Self {
        CCResults {
            best_overall_err: u64::MAX,
            low: ColorI::default(),
            high: ColorI::default(),
            pbits: [0, 0],
            selectors: [0; 16],
            selectors_temp: [0; 16],
        }
    }
}

#[inline]
pub(super) fn scale_color(c: &ColorI, p: &CCParams) -> ColorI {
    let n = p.comp_bits + if p.has_pbits { 1 } else { 0 };
    let mut r = ColorI::default();
    for i in 0..4 {
        let mut v = (c.c[i] as u32) << (8 - n);
        v |= v >> n;
        r.c[i] = v as i32;
    }
    r
}

#[inline]
pub(super) fn compute_color_distance_rgb(
    e1: &ColorI,
    e2: &ColorI,
    perceptual: bool,
    w: &[u32; 4],
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        if !perceptual && has_avx2() {
            unsafe {
                return compute_color_distance_rgb_avx2(e1, e2, w);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if !perceptual && has_neon() {
            unsafe {
                return compute_color_distance_rgb_neon(e1, e2, w);
            }
        }
    }
    compute_color_distance_rgb_scalar(e1, e2, perceptual, w)
}

#[inline]
fn compute_color_distance_rgb_scalar(
    e1: &ColorI,
    e2: &ColorI,
    perceptual: bool,
    w: &[u32; 4],
) -> u64 {
    if perceptual {
        let l1 = e1.c[0] as f32 * 0.2126 + e1.c[1] as f32 * 0.7152 + e1.c[2] as f32 * 0.0722;
        let cr1 = e1.c[0] as f32 - l1;
        let cb1 = e1.c[2] as f32 - l1;
        let l2 = e2.c[0] as f32 * 0.2126 + e2.c[1] as f32 * 0.7152 + e2.c[2] as f32 * 0.0722;
        let cr2 = e2.c[0] as f32 - l2;
        let cb2 = e2.c[2] as f32 - l2;
        let dl = l1 - l2;
        let dcr = cr1 - cr2;
        let dcb = cb1 - cb2;
        (w[0] as f32 * (dl * dl)
            + w[1] as f32 * PR_WEIGHT * (dcr * dcr)
            + w[2] as f32 * PB_WEIGHT * (dcb * dcb)) as i64 as u64
    } else {
        let dr = e1.c[0] as f32 - e2.c[0] as f32;
        let dg = e1.c[1] as f32 - e2.c[1] as f32;
        let db = e1.c[2] as f32 - e2.c[2] as f32;
        (w[0] as f32 * dr * dr + w[1] as f32 * dg * dg + w[2] as f32 * db * db) as i64 as u64
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn compute_color_distance_rgb_avx2(e1: &ColorI, e2: &ColorI, w: &[u32; 4]) -> u64 {
    use std::arch::x86_64::*;
    let v1 = _mm_loadu_si128(e1.c.as_ptr() as *const __m128i);
    let v2 = _mm_loadu_si128(e2.c.as_ptr() as *const __m128i);
    let f1 = _mm_cvtepi32_ps(v1);
    let f2 = _mm_cvtepi32_ps(v2);
    let d = _mm_sub_ps(f1, f2);
    let d2 = _mm_mul_ps(d, d);
    let wi = _mm_loadu_si128(w.as_ptr() as *const __m128i);
    let wf = _mm_cvtepi32_ps(wi);
    let wd2 = _mm_mul_ps(wf, d2);

    let r = _mm_cvtss_f32(wd2);
    let g = _mm_cvtss_f32(_mm_shuffle_ps(wd2, wd2, 0b01_01_01_01));
    let b = _mm_cvtss_f32(_mm_shuffle_ps(wd2, wd2, 0b10_10_10_10));
    (r + g + b) as i64 as u64
}

/// NEON port of the AVX2 kernel above (NEON is baseline on aarch64). The
/// weighted square is computed as `(w * d) * d` to match the scalar
/// evaluation order bit-for-bit.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn compute_color_distance_rgb_neon(e1: &ColorI, e2: &ColorI, w: &[u32; 4]) -> u64 {
    use std::arch::aarch64::*;
    let v1 = vld1q_s32(e1.c.as_ptr());
    let v2 = vld1q_s32(e2.c.as_ptr());
    let wi = vld1q_u32(w.as_ptr());
    let d = vsubq_f32(vcvtq_f32_s32(v1), vcvtq_f32_s32(v2));
    let wf = vcvtq_f32_u32(wi);
    let wd2 = vmulq_f32(vmulq_f32(wf, d), d);
    let r = vgetq_lane_f32::<0>(wd2);
    let g = vgetq_lane_f32::<1>(wd2);
    let b = vgetq_lane_f32::<2>(wd2);
    (r + g + b) as i64 as u64
}

#[inline]
pub(super) fn compute_color_distance_rgba(
    e1: &ColorI,
    e2: &ColorI,
    perceptual: bool,
    w: &[u32; 4],
) -> u64 {
    #[cfg(target_arch = "x86_64")]
    {
        if !perceptual && has_avx2() {
            unsafe {
                return compute_color_distance_rgba_avx2(e1, e2, w);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if !perceptual && has_neon() {
            unsafe {
                return compute_color_distance_rgba_neon(e1, e2, w);
            }
        }
    }
    compute_color_distance_rgba_scalar(e1, e2, perceptual, w)
}

#[inline]
fn compute_color_distance_rgba_scalar(
    e1: &ColorI,
    e2: &ColorI,
    perceptual: bool,
    w: &[u32; 4],
) -> u64 {
    let da = e1.c[3] as f32 - e2.c[3] as f32;
    let a_err = w[3] as f32 * (da * da);
    if perceptual {
        let l1 = e1.c[0] as f32 * 0.2126 + e1.c[1] as f32 * 0.7152 + e1.c[2] as f32 * 0.0722;
        let cr1 = e1.c[0] as f32 - l1;
        let cb1 = e1.c[2] as f32 - l1;
        let l2 = e2.c[0] as f32 * 0.2126 + e2.c[1] as f32 * 0.7152 + e2.c[2] as f32 * 0.0722;
        let cr2 = e2.c[0] as f32 - l2;
        let cb2 = e2.c[2] as f32 - l2;
        let dl = l1 - l2;
        let dcr = cr1 - cr2;
        let dcb = cb1 - cb2;
        (w[0] as f32 * (dl * dl)
            + w[1] as f32 * PR_WEIGHT * (dcr * dcr)
            + w[2] as f32 * PB_WEIGHT * (dcb * dcb)
            + a_err) as i64 as u64
    } else {
        let dr = e1.c[0] as f32 - e2.c[0] as f32;
        let dg = e1.c[1] as f32 - e2.c[1] as f32;
        let db = e1.c[2] as f32 - e2.c[2] as f32;
        (w[0] as f32 * dr * dr + w[1] as f32 * dg * dg + w[2] as f32 * db * db + a_err) as i64
            as u64
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn compute_color_distance_rgba_avx2(e1: &ColorI, e2: &ColorI, w: &[u32; 4]) -> u64 {
    use std::arch::x86_64::*;
    let v1 = _mm_loadu_si128(e1.c.as_ptr() as *const __m128i);
    let v2 = _mm_loadu_si128(e2.c.as_ptr() as *const __m128i);
    let f1 = _mm_cvtepi32_ps(v1);
    let f2 = _mm_cvtepi32_ps(v2);
    let d = _mm_sub_ps(f1, f2);
    let d2 = _mm_mul_ps(d, d);
    let wi = _mm_loadu_si128(w.as_ptr() as *const __m128i);
    let wf = _mm_cvtepi32_ps(wi);
    let wd2 = _mm_mul_ps(wf, d2);
    let r = _mm_cvtss_f32(wd2);
    let g = _mm_cvtss_f32(_mm_shuffle_ps(wd2, wd2, 0b01_01_01_01));
    let b = _mm_cvtss_f32(_mm_shuffle_ps(wd2, wd2, 0b10_10_10_10));
    let a = _mm_cvtss_f32(_mm_shuffle_ps(wd2, wd2, 0b11_11_11_11));
    (r + g + b + a) as i64 as u64
}

/// NEON port of the AVX2 kernel above; see `compute_color_distance_rgb_neon`
/// for the rounding-order notes.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn compute_color_distance_rgba_neon(e1: &ColorI, e2: &ColorI, w: &[u32; 4]) -> u64 {
    use std::arch::aarch64::*;
    let v1 = vld1q_s32(e1.c.as_ptr());
    let v2 = vld1q_s32(e2.c.as_ptr());
    let wi = vld1q_u32(w.as_ptr());
    let d = vsubq_f32(vcvtq_f32_s32(v1), vcvtq_f32_s32(v2));
    let wf = vcvtq_f32_u32(wi);
    let wd2 = vmulq_f32(vmulq_f32(wf, d), d);
    let r = vgetq_lane_f32::<0>(wd2);
    let g = vgetq_lane_f32::<1>(wd2);
    let b = vgetq_lane_f32::<2>(wd2);
    let da = vgetq_lane_f32::<3>(d);
    let a = vgetq_lane_f32::<3>(wf) * (da * da);
    (r + g + b + a) as i64 as u64
}

pub(super) fn compute_lsq_endpoints_rgba(
    n: usize,
    sel: &[i32],
    sw: &[[f32; 4]],
    xl: &mut Vec4F,
    xh: &mut Vec4F,
    colors: &[ColorI],
) {
    let (mut z00, mut z10, mut z11) = (0f32, 0f32, 0f32);
    let (mut q00_r, mut t_r) = (0f32, 0f32);
    let (mut q00_g, mut t_g) = (0f32, 0f32);
    let (mut q00_b, mut t_b) = (0f32, 0f32);
    let (mut q00_a, mut t_a) = (0f32, 0f32);
    for i in 0..n {
        let s = sel[i] as usize;
        z00 += sw[s][0];
        z10 += sw[s][1];
        z11 += sw[s][2];
        let w = sw[s][3];
        q00_r += w * colors[i].c[0] as f32;
        t_r += colors[i].c[0] as f32;
        q00_g += w * colors[i].c[1] as f32;
        t_g += colors[i].c[1] as f32;
        q00_b += w * colors[i].c[2] as f32;
        t_b += colors[i].c[2] as f32;
        q00_a += w * colors[i].c[3] as f32;
        t_a += colors[i].c[3] as f32;
    }
    let q10_r = t_r - q00_r;
    let q10_g = t_g - q00_g;
    let q10_b = t_b - q00_b;
    let q10_a = t_a - q00_a;
    let z01 = z10;
    let mut det = z00 * z11 - z01 * z10;
    if det != 0.0 {
        det = 1.0 / det;
    }
    let iz00 = z11 * det;
    let iz01 = -z01 * det;
    let iz10 = -z10 * det;
    let iz11 = z00 * det;
    xl.c[0] = iz00 * q00_r + iz01 * q10_r;
    xh.c[0] = iz10 * q00_r + iz11 * q10_r;
    xl.c[1] = iz00 * q00_g + iz01 * q10_g;
    xh.c[1] = iz10 * q00_g + iz11 * q10_g;
    xl.c[2] = iz00 * q00_b + iz01 * q10_b;
    xh.c[2] = iz10 * q00_b + iz11 * q10_b;
    xl.c[3] = iz00 * q00_a + iz01 * q10_a;
    xh.c[3] = iz10 * q00_a + iz11 * q10_a;
}

pub(super) fn compute_lsq_endpoints_rgb(
    n: usize,
    sel: &[i32],
    sw: &[[f32; 4]],
    xl: &mut Vec4F,
    xh: &mut Vec4F,
    colors: &[ColorI],
) {
    #[cfg(target_arch = "x86_64")]
    {
        if has_avx2() {
            unsafe {
                return compute_lsq_endpoints_rgb_avx2(n, sel, sw, xl, xh, colors);
            }
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        if has_neon() {
            unsafe {
                return compute_lsq_endpoints_rgb_neon(n, sel, sw, xl, xh, colors);
            }
        }
    }
    compute_lsq_endpoints_rgb_scalar(n, sel, sw, xl, xh, colors)
}

fn compute_lsq_endpoints_rgb_scalar(
    n: usize,
    sel: &[i32],
    sw: &[[f32; 4]],
    xl: &mut Vec4F,
    xh: &mut Vec4F,
    colors: &[ColorI],
) {
    let (mut z00, mut z10, mut z11) = (0f32, 0f32, 0f32);
    let (mut q00_r, mut t_r) = (0f32, 0f32);
    let (mut q00_g, mut t_g) = (0f32, 0f32);
    let (mut q00_b, mut t_b) = (0f32, 0f32);
    for i in 0..n {
        let s = sel[i] as usize;
        z00 += sw[s][0];
        z10 += sw[s][1];
        z11 += sw[s][2];
        let w = sw[s][3];
        q00_r += w * colors[i].c[0] as f32;
        t_r += colors[i].c[0] as f32;
        q00_g += w * colors[i].c[1] as f32;
        t_g += colors[i].c[1] as f32;
        q00_b += w * colors[i].c[2] as f32;
        t_b += colors[i].c[2] as f32;
    }
    let q10_r = t_r - q00_r;
    let q10_g = t_g - q00_g;
    let q10_b = t_b - q00_b;
    let z01 = z10;
    let mut det = z00 * z11 - z01 * z10;
    if det != 0.0 {
        det = 1.0 / det;
    }
    let iz00 = z11 * det;
    let iz01 = -z01 * det;
    let iz10 = -z10 * det;
    let iz11 = z00 * det;
    xl.c[0] = iz00 * q00_r + iz01 * q10_r;
    xh.c[0] = iz10 * q00_r + iz11 * q10_r;
    xl.c[1] = iz00 * q00_g + iz01 * q10_g;
    xh.c[1] = iz10 * q00_g + iz11 * q10_g;
    xl.c[2] = iz00 * q00_b + iz01 * q10_b;
    xh.c[2] = iz10 * q00_b + iz11 * q10_b;
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn compute_lsq_endpoints_rgb_avx2(
    n: usize,
    sel: &[i32],
    sw: &[[f32; 4]],
    xl: &mut Vec4F,
    xh: &mut Vec4F,
    colors: &[ColorI],
) {
    use std::arch::x86_64::*;
    let mut z = _mm_setzero_ps();
    let mut q00 = _mm_setzero_ps();
    let mut t = _mm_setzero_ps();
    for i in 0..n {
        let s = sel[i] as usize;
        let sw_v = _mm_loadu_ps(sw[s].as_ptr());
        z = _mm_add_ps(z, sw_v);
        let w = _mm_shuffle_ps(sw_v, sw_v, 0b11_11_11_11);
        let ci = _mm_loadu_si128(colors[i].c.as_ptr() as *const __m128i);
        let cf = _mm_cvtepi32_ps(ci);
        q00 = _mm_add_ps(q00, _mm_mul_ps(w, cf));
        t = _mm_add_ps(t, cf);
    }
    let z00 = _mm_cvtss_f32(z);
    let z10 = _mm_cvtss_f32(_mm_shuffle_ps(z, z, 0b01_01_01_01));
    let z11 = _mm_cvtss_f32(_mm_shuffle_ps(z, z, 0b10_10_10_10));
    let q00_r = _mm_cvtss_f32(q00);
    let q00_g = _mm_cvtss_f32(_mm_shuffle_ps(q00, q00, 0b01_01_01_01));
    let q00_b = _mm_cvtss_f32(_mm_shuffle_ps(q00, q00, 0b10_10_10_10));
    let t_r = _mm_cvtss_f32(t);
    let t_g = _mm_cvtss_f32(_mm_shuffle_ps(t, t, 0b01_01_01_01));
    let t_b = _mm_cvtss_f32(_mm_shuffle_ps(t, t, 0b10_10_10_10));
    let q10_r = t_r - q00_r;
    let q10_g = t_g - q00_g;
    let q10_b = t_b - q00_b;
    let z01 = z10;
    let mut det = z00 * z11 - z01 * z10;
    if det != 0.0 {
        det = 1.0 / det;
    }
    let iz00 = z11 * det;
    let iz01 = -z01 * det;
    let iz10 = -z10 * det;
    let iz11 = z00 * det;
    xl.c[0] = iz00 * q00_r + iz01 * q10_r;
    xh.c[0] = iz10 * q00_r + iz11 * q10_r;
    xl.c[1] = iz00 * q00_g + iz01 * q10_g;
    xh.c[1] = iz10 * q00_g + iz11 * q10_g;
    xl.c[2] = iz00 * q00_b + iz01 * q10_b;
    xh.c[2] = iz10 * q00_b + iz11 * q10_b;
}

/// NEON port of the AVX2 kernel above. Per-lane accumulation happens in the
/// same iteration order with unfused mul-then-add, so every partial sum is
/// bit-identical to the scalar path.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
unsafe fn compute_lsq_endpoints_rgb_neon(
    n: usize,
    sel: &[i32],
    sw: &[[f32; 4]],
    xl: &mut Vec4F,
    xh: &mut Vec4F,
    colors: &[ColorI],
) {
    use std::arch::aarch64::*;
    let mut z = vdupq_n_f32(0.0);
    let mut q00 = vdupq_n_f32(0.0);
    let mut t = vdupq_n_f32(0.0);
    for i in 0..n {
        let s = sel[i] as usize;
        let sw_v = vld1q_f32(sw[s].as_ptr());
        let ci = vld1q_s32(colors[i].c.as_ptr());
        z = vaddq_f32(z, sw_v);
        let w = vdupq_laneq_f32::<3>(sw_v);
        let cf = vcvtq_f32_s32(ci);
        q00 = vaddq_f32(q00, vmulq_f32(w, cf));
        t = vaddq_f32(t, cf);
    }
    let z00 = vgetq_lane_f32::<0>(z);
    let z10 = vgetq_lane_f32::<1>(z);
    let z11 = vgetq_lane_f32::<2>(z);
    let q00_r = vgetq_lane_f32::<0>(q00);
    let q00_g = vgetq_lane_f32::<1>(q00);
    let q00_b = vgetq_lane_f32::<2>(q00);
    let t_r = vgetq_lane_f32::<0>(t);
    let t_g = vgetq_lane_f32::<1>(t);
    let t_b = vgetq_lane_f32::<2>(t);
    let q10_r = t_r - q00_r;
    let q10_g = t_g - q00_g;
    let q10_b = t_b - q00_b;
    let z01 = z10;
    let mut det = z00 * z11 - z01 * z10;
    if det != 0.0 {
        det = 1.0 / det;
    }
    let iz00 = z11 * det;
    let iz01 = -z01 * det;
    let iz10 = -z10 * det;
    let iz11 = z00 * det;
    xl.c[0] = iz00 * q00_r + iz01 * q10_r;
    xh.c[0] = iz10 * q00_r + iz11 * q10_r;
    xl.c[1] = iz00 * q00_g + iz01 * q10_g;
    xh.c[1] = iz10 * q00_g + iz11 * q10_g;
    xl.c[2] = iz00 * q00_b + iz01 * q10_b;
    xh.c[2] = iz10 * q00_b + iz11 * q10_b;
}

pub(super) fn compute_lsq_endpoints_a(
    n: usize,
    sel: &[i32],
    sw: &[[f32; 4]],
    xl: &mut f32,
    xh: &mut f32,
    colors: &[ColorI],
) {
    let (mut z00, mut z10, mut z11) = (0f32, 0f32, 0f32);
    let (mut q00_a, mut t_a) = (0f32, 0f32);
    for i in 0..n {
        let s = sel[i] as usize;
        z00 += sw[s][0];
        z10 += sw[s][1];
        z11 += sw[s][2];
        let w = sw[s][3];
        q00_a += w * colors[i].c[3] as f32;
        t_a += colors[i].c[3] as f32;
    }
    let q10_a = t_a - q00_a;
    let z01 = z10;
    let mut det = z00 * z11 - z01 * z10;
    if det != 0.0 {
        det = 1.0 / det;
    }
    let iz00 = z11 * det;
    let iz01 = -z01 * det;
    let iz10 = -z10 * det;
    let iz11 = z00 * det;
    *xl = iz00 * q00_a + iz01 * q10_a;
    *xh = iz10 * q00_a + iz11 * q10_a;
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

    fn color(rng: &mut Rng, extreme: bool) -> ColorI {
        let mut c = ColorI::default();
        for i in 0..4 {
            c.c[i] = if extreme {
                match rng.next_u32() % 4 {
                    0 => 0,
                    1 => 255,
                    2 => -(rng.next_u32() as i32 % 1024),
                    _ => rng.next_u32() as i32 % 1024,
                }
            } else {
                (rng.next_u32() % 256) as i32
            };
        }
        c
    }

    #[test]
    fn distance_neon_matches_scalar() {
        let mut rng = Rng(0x2545F4914F6CDD1D);
        for iter in 0..200_000 {
            let extreme = iter % 4 == 0;
            let e1 = color(&mut rng, extreme);
            let e2 = color(&mut rng, extreme);
            let w = [
                rng.next_u32() % 256,
                rng.next_u32() % 256,
                rng.next_u32() % 256,
                rng.next_u32() % 256,
            ];
            assert_eq!(
                unsafe { compute_color_distance_rgb_neon(&e1, &e2, &w) },
                compute_color_distance_rgb_scalar(&e1, &e2, false, &w),
                "rgb mismatch e1={:?} e2={:?} w={:?}",
                e1.c,
                e2.c,
                w
            );
            assert_eq!(
                unsafe { compute_color_distance_rgba_neon(&e1, &e2, &w) },
                compute_color_distance_rgba_scalar(&e1, &e2, false, &w),
                "rgba mismatch e1={:?} e2={:?} w={:?}",
                e1.c,
                e2.c,
                w
            );
        }
    }

    #[test]
    fn lsq_rgb_neon_matches_scalar() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        for (wi, sw) in [
            (2usize, &G_WEIGHTS2X as &[[f32; 4]]),
            (3, &G_WEIGHTS3X),
            (4, &G_WEIGHTS4X),
        ] {
            let nsel = 1usize << wi;
            for iter in 0..20_000 {
                let n = 1 + (rng.next_u32() as usize % 16);
                let mut sel = [0i32; 16];
                let mut colors = [ColorI::default(); 16];
                for i in 0..n {
                    sel[i] = (rng.next_u32() as usize % nsel) as i32;
                    colors[i] = color(&mut rng, iter % 4 == 0);
                }
                if iter % 7 == 0 {
                    let c0 = colors[0];
                    let s0 = sel[0];
                    for i in 0..n {
                        colors[i] = c0;
                        sel[i] = s0;
                    }
                }
                let (mut xl_s, mut xh_s) = (Vec4F::default(), Vec4F::default());
                let (mut xl_n, mut xh_n) = (Vec4F::default(), Vec4F::default());
                compute_lsq_endpoints_rgb_scalar(n, &sel, sw, &mut xl_s, &mut xh_s, &colors);
                unsafe {
                    compute_lsq_endpoints_rgb_neon(n, &sel, sw, &mut xl_n, &mut xh_n, &colors)
                };
                for k in 0..3 {
                    assert_eq!(xl_s.c[k].to_bits(), xl_n.c[k].to_bits(), "xl lane {k}");
                    assert_eq!(xh_s.c[k].to_bits(), xh_n.c[k].to_bits(), "xh lane {k}");
                }
            }
        }
    }
}

#[cfg(test)]
mod encode_corpus_hash {
    /// Prints an FNV-1a hash of every encoded block over a corpus of random,
    /// flat, extreme, and alpha-edge blocks. Run once with `ABGEN_BC7_SCALAR=1`
    /// and once without (separate processes; the detectors latch) and compare
    /// the printed hashes to check SIMD/scalar output parity end to end.
    #[test]
    fn corpus_hash_print() {
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
        let mut rng = Rng(0xA5A5A5A55A5A5A5A);
        let num_blocks = 1024usize;
        let mut corpus = vec![0u8; num_blocks * 64];
        for b in 0..num_blocks {
            let block = &mut corpus[b * 64..b * 64 + 64];
            match b % 8 {
                0 => {
                    let (r, g, bl, a) = (
                        (b % 256) as u8,
                        (b * 7 % 256) as u8,
                        (b * 13 % 256) as u8,
                        if b % 16 == 0 { 0 } else { 255 },
                    );
                    for t in 0..16 {
                        block[t * 4..t * 4 + 4].copy_from_slice(&[r, g, bl, a]);
                    }
                }
                1 => {
                    for t in 0..16 {
                        for k in 0..4 {
                            block[t * 4 + k] = if rng.next_u32().is_multiple_of(2) {
                                0
                            } else {
                                255
                            };
                        }
                    }
                }
                2 => {
                    for t in 0..16 {
                        for k in 0..3 {
                            block[t * 4 + k] = rng.next_u32() as u8;
                        }
                        block[t * 4 + 3] = [0u8, 128, 255][t % 3];
                    }
                }
                _ => {
                    for v in block.iter_mut() {
                        *v = rng.next_u32() as u8;
                    }
                }
            }
        }
        let mut h: u64 = 0xcbf29ce484222325;
        for params in [
            crate::bc7_pure::Params::basic(false),
            crate::bc7_pure::Params::basic(true),
            crate::bc7_pure::Params::slow(false),
            crate::bc7_pure::Params::slow(true),
        ] {
            let out = crate::bc7_pure::encode_blocks(&corpus, num_blocks, &params);
            for &byte in &out {
                h ^= byte as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
        }
        println!("BC7_CORPUS_HASH={h:016x}");
    }
}
