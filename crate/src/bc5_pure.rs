const BC5_BLOCK_SIZE: usize = 16;

fn fix_range(min: &mut u8, max: &mut u8, steps: u8) {
    if (*max - *min) < steps {
        *max = (i32::from(*min) + i32::from(steps)).min(i32::from(u8::MAX)) as u8;
    }
    if (*max - *min) < steps {
        *min = (i32::from(*max) - i32::from(steps)).max(0) as u8;
    }
}

/// Builds both codebooks from the (already clamped) channel ranges.
fn build_codebooks(min5: u8, max5: u8, min7: u8, max7: u8) -> ([u8; 8], [u8; 8]) {
    let mut codes5 = [0u8; 8];
    codes5[0] = min5;
    codes5[1] = max5;
    for i in 1..5i32 {
        codes5[1 + i as usize] = (((5 - i) * i32::from(min5) + i * i32::from(max5)) / 5) as u8;
    }
    codes5[6] = 0;
    codes5[7] = u8::MAX;

    let mut codes7 = [0u8; 8];
    codes7[0] = min5;
    codes7[1] = max5;
    for i in 1..7i32 {
        codes7[1 + i as usize] = (((7 - i) * i32::from(min7) + i * i32::from(max7)) / 7) as u8;
    }
    (codes5, codes7)
}

fn write_alpha_block(alpha0: u8, alpha1: u8, indices: &[u8; 16], block: &mut [u8]) {
    let mut buf = [0u8; 8];
    buf[0] = alpha0;
    buf[1] = alpha1;
    for i in 0..2 {
        let mut value = 0u32;
        for j in 0..8 {
            value |= u32::from(indices[8 * i + j]) << (3 * j);
        }
        for j in 0..3 {
            buf[2 + i * 3 + j] = ((value >> (8 * j)) & 0xFF) as u8;
        }
    }
    block.copy_from_slice(&buf);
}

fn write_alpha_block5(alpha0: u8, alpha1: u8, indices: &[u8; 16], block: &mut [u8]) {
    if alpha0 > alpha1 {
        let mut swapped = *indices;
        for index in &mut swapped[..] {
            *index = match *index {
                0 => 1,
                1 => 0,
                x @ 2..=5 => 7 - x,
                x => x,
            }
        }
        write_alpha_block(alpha1, alpha0, &swapped, block);
    } else {
        write_alpha_block(alpha0, alpha1, indices, block);
    }
}

fn write_alpha_block7(alpha0: u8, alpha1: u8, indices: &[u8; 16], block: &mut [u8]) {
    if alpha0 < alpha1 {
        let mut swapped = *indices;
        for index in &mut swapped[..] {
            *index = match *index {
                0 => 1,
                1 => 0,
                x => 9 - x,
            }
        }
        write_alpha_block(alpha1, alpha0, &swapped, block);
    } else {
        write_alpha_block(alpha0, alpha1, indices, block);
    }
}

fn fit_codes_scalar(vals: &[u8; 16], codes: &[u8; 8], indices: &mut [u8; 16]) -> u32 {
    let mut err = 0u32;
    for i in 0..16 {
        let value = vals[i];
        let mut least = u32::MAX;
        let mut index = 0u8;
        for (j, &code) in codes.iter().enumerate() {
            let dist = i32::from(value) - i32::from(code);
            let dist = (dist * dist) as u32;
            if dist < least {
                least = dist;
                index = j as u8;
            }
        }
        indices[i] = index;
        err += least;
    }
    err
}

#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub(crate) fn encode_bc4_channel_scalar(vals: &[u8; 16], block: &mut [u8]) {
    let mut min5 = u8::MAX;
    let mut max5 = 0u8;
    let mut min7 = u8::MAX;
    let mut max7 = 0u8;
    for &value in vals {
        min7 = min7.min(value);
        max7 = max7.max(value);
        if value != 0 {
            min5 = min5.min(value);
        }
        if value != u8::MAX {
            max5 = max5.max(value);
        }
    }
    if min5 > max5 {
        min5 = max5;
    }
    if min7 > max7 {
        min7 = max7;
    }
    fix_range(&mut min5, &mut max5, 5);
    fix_range(&mut min7, &mut max7, 7);

    let (codes5, codes7) = build_codebooks(min5, max5, min7, max7);

    let mut indices5 = [0u8; 16];
    let mut indices7 = [0u8; 16];
    let err5 = fit_codes_scalar(vals, &codes5, &mut indices5);
    let err7 = fit_codes_scalar(vals, &codes7, &mut indices7);

    if err5 <= err7 {
        write_alpha_block5(min5, max5, &indices5, block);
    } else {
        write_alpha_block7(min7, max7, &indices7, block);
    }
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use std::arch::aarch64::*;

    /// 8-level index selection over 16 lanes. Squared error ordering equals
    /// absolute-difference ordering on u8, so the argmin over `vabdq_u8`
    /// distances (first-min tie-break, matching the scalar strict `<`) picks
    /// identical indices; the returned error re-squares the winning distances
    /// so the err5/err7 comparison is bit-identical to the scalar sum.
    #[inline]
    unsafe fn fit_codes(v: uint8x16_t, codes: &[u8; 8]) -> (u32, uint8x16_t) {
        let mut best_d = vabdq_u8(v, vdupq_n_u8(codes[0]));
        let mut best_i = vdupq_n_u8(0);
        for (j, &code) in codes.iter().enumerate().skip(1) {
            let d = vabdq_u8(v, vdupq_n_u8(code));
            let lt = vcltq_u8(d, best_d);
            best_d = vbslq_u8(lt, d, best_d);
            best_i = vbslq_u8(lt, vdupq_n_u8(j as u8), best_i);
        }
        let lo = vget_low_u8(best_d);
        let hi = vget_high_u8(best_d);
        let sum = vaddq_u32(vpaddlq_u16(vmull_u8(lo, lo)), vpaddlq_u16(vmull_u8(hi, hi)));
        (vaddvq_u32(sum), best_i)
    }

    pub(super) unsafe fn encode_bc4_channel(vals: &[u8; 16], block: &mut [u8]) {
        encode_bc4_channel_v(vld1q_u8(vals.as_ptr()), block)
    }

    pub(super) unsafe fn encode_bc4_channel_v(v: uint8x16_t, block: &mut [u8]) {
        let mut min7 = vminvq_u8(v);
        let mut max7 = vmaxvq_u8(v);
        let mut min5 = vminvq_u8(vbslq_u8(vceqzq_u8(v), vdupq_n_u8(u8::MAX), v));
        let mut max5 = vmaxvq_u8(vbslq_u8(vceqq_u8(v, vdupq_n_u8(u8::MAX)), vdupq_n_u8(0), v));
        if min5 > max5 {
            min5 = max5;
        }
        super::fix_range(&mut min5, &mut max5, 5);
        super::fix_range(&mut min7, &mut max7, 7);

        let (codes5, codes7) = super::build_codebooks(min5, max5, min7, max7);

        let (err5, indices5) = fit_codes(v, &codes5);
        let (err7, indices7) = fit_codes(v, &codes7);

        let mut idx = [0u8; 16];
        if err5 <= err7 {
            vst1q_u8(idx.as_mut_ptr(), indices5);
            super::write_alpha_block(min5, max5, &idx, block);
        } else {
            const T7: [u8; 16] = [1, 0, 7, 6, 5, 4, 3, 2, 0, 0, 0, 0, 0, 0, 0, 0];
            let inv = vqtbl1q_u8(vld1q_u8(T7.as_ptr()), indices7);
            vst1q_u8(idx.as_mut_ptr(), inv);
            super::write_alpha_block(max7, min7, &idx, block);
        }
    }
}

#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
#[inline]
fn encode_bc4_channel(vals: &[u8; 16], block: &mut [u8]) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        neon::encode_bc4_channel(vals, block)
    }
    #[cfg(not(target_arch = "aarch64"))]
    encode_bc4_channel_scalar(vals, block)
}

/// BC5-compresses `padded` (RGBA, dimensions multiples of 4); the X channel is
/// read from byte 0 and the Y channel from byte 1 of each texel, matching
/// `texpresso::Format::Bc5.compress` on `repack_for_bc5` output.
pub(crate) fn compress_bc5_blocks(padded: &[u8], pw: usize, ph: usize, out: &mut [u8]) {
    debug_assert!(pw.is_multiple_of(4) && ph.is_multiple_of(4));
    let bw = pw / 4;
    let bh = ph / 4;
    debug_assert_eq!(out.len(), bw * bh * BC5_BLOCK_SIZE);
    debug_assert_eq!(padded.len(), pw * ph * 4);
    // Fixed block-row chunking into a pre-sized slice (the
    // bc7_pure::encode_blocks pattern): every 16-byte slot is written by
    // block index, so output bytes are identical at any thread count.
    use rayon::prelude::*;
    out.par_chunks_mut(bw * BC5_BLOCK_SIZE)
        .enumerate()
        .for_each(|(by, row)| compress_bc5_block_row(padded, pw, bw, by, row));
}

fn compress_bc5_block_row(padded: &[u8], pw: usize, bw: usize, by: usize, row: &mut [u8]) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        for bx in 0..bw {
            let s = ((by * 4) * pw + bx * 4) * 4;
            let r0 = vld1q_u8(padded.as_ptr().add(s));
            let r1 = vld1q_u8(padded.as_ptr().add(s + pw * 4));
            let r2 = vld1q_u8(padded.as_ptr().add(s + pw * 8));
            let r3 = vld1q_u8(padded.as_ptr().add(s + pw * 12));
            let e01 = vuzp1q_u8(r0, r1);
            let e23 = vuzp1q_u8(r2, r3);
            let o01 = vuzp2q_u8(r0, r1);
            let o23 = vuzp2q_u8(r2, r3);
            let ch0 = vuzp1q_u8(e01, e23);
            let ch1 = vuzp1q_u8(o01, o23);
            let o = bx * BC5_BLOCK_SIZE;
            neon::encode_bc4_channel_v(ch0, &mut row[o..o + 8]);
            neon::encode_bc4_channel_v(ch1, &mut row[o + 8..o + 16]);
        }
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        let mut ch0 = [0u8; 16];
        let mut ch1 = [0u8; 16];
        for bx in 0..bw {
            for ty in 0..4 {
                let s = ((by * 4 + ty) * pw + bx * 4) * 4;
                for tx in 0..4 {
                    ch0[ty * 4 + tx] = padded[s + tx * 4];
                    ch1[ty * 4 + tx] = padded[s + tx * 4 + 1];
                }
            }
            let o = bx * BC5_BLOCK_SIZE;
            encode_bc4_channel(&ch0, &mut row[o..o + 8]);
            encode_bc4_channel(&ch1, &mut row[o + 8..o + 16]);
        }
    }
}

pub(crate) fn repack_for_bc5(rgba: &[u8]) -> Vec<u8> {
    debug_assert!(rgba.len().is_multiple_of(4));
    let mut out = vec![0u8; rgba.len()];
    let n = rgba.len() / 4;
    #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
    let mut i = 0;
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        while i + 16 <= n {
            let v = vld4q_u8(rgba.as_ptr().add(i * 4));
            let res = uint8x16x4_t(v.3, v.1, vdupq_n_u8(0), vdupq_n_u8(255));
            vst4q_u8(out.as_mut_ptr().add(i * 4), res);
            i += 16;
        }
    }
    for i in i..n {
        let y = rgba[i * 4 + 1];
        let x = rgba[i * 4 + 3];
        out[i * 4] = x;
        out[i * 4 + 1] = y;
        out[i * 4 + 2] = 0;
        out[i * 4 + 3] = 255;
    }
    out
}

fn pad_to_block_size(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let pw = (w + 3) & !3;
    let ph = (h + 3) & !3;
    if pw == w && ph == h {
        return (rgba.to_vec(), w, h);
    }

    let mut out = vec![0u8; pw * ph * 4];
    for y in 0..ph {
        let sy = y % h;
        for x in 0..pw {
            let sx = x % w;
            let s = (sy * w + sx) * 4;
            let d = (y * pw + x) * 4;
            out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
        }
    }
    (out, pw, ph)
}

/// 2x2 rounding average for even dimensions: `(a+b+c+d+2)>>2` per channel,
/// identical to the scalar accumulation in `box_halve_rgba_u8`.
#[cfg(target_arch = "aarch64")]
unsafe fn box_halve_even_neon(rgba: &[u8], w: usize, h: usize) -> Vec<u8> {
    use std::arch::aarch64::*;
    let nw = w / 2;
    let nh = h / 2;
    let mut out = vec![0u8; nw * nh * 4];
    let two = vdupq_n_u16(2);
    for ny in 0..nh {
        let r0 = (ny * 2) * w * 4;
        let r1 = r0 + w * 4;
        let orow = ny * nw * 4;
        let mut nx = 0;
        while nx + 2 <= nw {
            let s = nx * 2 * 4;
            let a = vld1q_u8(rgba.as_ptr().add(r0 + s));
            let b = vld1q_u8(rgba.as_ptr().add(r1 + s));
            let lo = vaddl_u8(vget_low_u8(a), vget_low_u8(b));
            let hi = vaddl_u8(vget_high_u8(a), vget_high_u8(b));
            let p0 = vadd_u16(vget_low_u16(lo), vget_high_u16(lo));
            let p1 = vadd_u16(vget_low_u16(hi), vget_high_u16(hi));
            let avg = vshrq_n_u16::<2>(vaddq_u16(vcombine_u16(p0, p1), two));
            vst1_u8(out.as_mut_ptr().add(orow + nx * 4), vmovn_u16(avg));
            nx += 2;
        }
        while nx < nw {
            let s0 = r0 + nx * 2 * 4;
            let s1 = r1 + nx * 2 * 4;
            for ch in 0..4 {
                let acc = u32::from(rgba[s0 + ch])
                    + u32::from(rgba[s0 + 4 + ch])
                    + u32::from(rgba[s1 + ch])
                    + u32::from(rgba[s1 + 4 + ch]);
                out[orow + nx * 4 + ch] = ((acc + 2) / 4) as u8;
            }
            nx += 1;
        }
    }
    out
}

pub(crate) fn box_halve_rgba_u8(rgba: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let c = 4usize;
    let nh = (h / 2).max(1);
    let nw = (w / 2).max(1);
    #[cfg(target_arch = "aarch64")]
    if w > 1 && h > 1 && w.is_multiple_of(2) && h.is_multiple_of(2) {
        return (unsafe { box_halve_even_neon(rgba, w, h) }, nw, nh);
    }
    let fh = if h > 1 { 2 } else { 1 };
    let fw = if w > 1 { 2 } else { 1 };
    let denom = (fh * fw) as u32;
    let mut out = vec![0u8; nh * nw * c];
    let row_stride = w * c;
    for ny in 0..nh {
        for nx in 0..nw {
            for ch in 0..c {
                let mut acc: u32 = 0;
                for dy in 0..fh {
                    for dx in 0..fw {
                        let y = ny * fh + dy;
                        let x = nx * fw + dx;
                        acc += rgba[y * row_stride + x * c + ch] as u32;
                    }
                }
                out[(ny * nw + nx) * c + ch] = ((acc + denom / 2) / denom) as u8;
            }
        }
    }
    (out, nw, nh)
}

pub fn encode_bc5_mip_chain(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
) -> (Vec<u8>, i32) {
    let w = width as usize;
    let h = height as usize;
    assert_eq!(rgba.len(), w * h * 4);

    let flipped: Vec<u8> = if flip {
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            let src = &rgba[(h - 1 - y) * w * 4..(h - 1 - y) * w * 4 + w * 4];
            out[y * w * 4..y * w * 4 + w * 4].copy_from_slice(src);
        }
        out
    } else {
        rgba.to_vec()
    };

    let mip_count = mip_count.unwrap_or_else(|| {
        let m = width.max(height).max(1) as f64;
        (crate::detmath::log2(m).floor() as i32) + 1
    });

    let mut cur = flipped;
    let mut cw = w;
    let mut ch = h;

    let mut parts: Vec<u8> = Vec::new();
    for m in 0..mip_count {
        let repacked = repack_for_bc5(&cur);
        let (padded, pw, ph) = pad_to_block_size(&repacked, cw, ch);
        let block_count = (pw / 4) * (ph / 4);
        let mut level_out = vec![0u8; block_count * BC5_BLOCK_SIZE];
        compress_bc5_blocks(&padded, pw, ph, &mut level_out);
        parts.extend_from_slice(&level_out);

        if m < mip_count - 1 {
            let (next, nw, nh) = box_halve_rgba_u8(&cur, cw, ch);
            cur = next;
            cw = nw;
            ch = nh;
        }
    }
    (parts, mip_count)
}

pub fn encode_dxt5_crn_mip_chain(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    quality_level: u32,
) -> Option<(Vec<u8>, i32)> {
    let params = [
        mip_count.map(i64::from).unwrap_or(-1),
        flip as i64,
        quality_level as i64,
    ];
    crate::texencode_cache::get_or_encode(
        crate::texencode_cache::Kind::Dxt5Crn,
        rgba,
        width,
        height,
        &params,
        || encode_dxt5_crn_mip_chain_uncached(rgba, width, height, mip_count, flip, quality_level),
    )
}

fn encode_dxt5_crn_mip_chain_uncached(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    quality_level: u32,
) -> Option<(Vec<u8>, i32)> {
    let w = width as usize;
    let h = height as usize;
    assert_eq!(rgba.len(), w * h * 4);

    let flipped: Vec<u8> = if flip {
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            let src = &rgba[(h - 1 - y) * w * 4..(h - 1 - y) * w * 4 + w * 4];
            out[y * w * 4..y * w * 4 + w * 4].copy_from_slice(src);
        }
        out
    } else {
        rgba.to_vec()
    };

    let mip_count = mip_count.unwrap_or_else(|| {
        let m = width.max(height).max(1) as f64;
        (crate::detmath::log2(m).floor() as i32) + 1
    });

    let mut cur = flipped;
    let mut cw = w;
    let mut ch = h;
    let mut mip_w_vec: Vec<u32> = Vec::with_capacity(mip_count as usize);
    let mut mip_h_vec: Vec<u32> = Vec::with_capacity(mip_count as usize);
    let mut mip_rgba: Vec<u8> = Vec::new();
    for m in 0..mip_count {
        mip_rgba.extend_from_slice(&cur);
        mip_w_vec.push(cw as u32);
        mip_h_vec.push(ch as u32);
        if m < mip_count - 1 {
            let (next, nw, nh) = box_halve_rgba_u8(&cur, cw, ch);
            cur = next;
            cw = nw;
            ch = nh;
        }
    }

    Some((
        crunch_ffi::crn_compress_dxt5(&mip_rgba, &mip_w_vec, &mip_h_vec, quality_level)?,
        mip_count,
    ))
}

fn pack_normal_map_linearized(rgba: &[u8]) -> Vec<u8> {
    use crate::bc7_pure::{round_half_up_u8, srgb_to_linear_u8};
    let n = rgba.len() / 4;
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let lin_r = round_half_up_u8(srgb_to_linear_u8(rgba[i * 4]) * 255.0);
        let lin_g = round_half_up_u8(srgb_to_linear_u8(rgba[i * 4 + 1]) * 255.0);
        out[i * 4] = 255;
        out[i * 4 + 1] = lin_g;
        out[i * 4 + 2] = lin_g;
        out[i * 4 + 3] = lin_r;
    }
    out
}

pub fn encode_dxt5_crn_dual_use_mip_chain(
    rgba: &[u8],
    width: u32,
    height: u32,
    mip_count: Option<i32>,
    flip: bool,
    quality_level: u32,
) -> Option<(Vec<u8>, i32)> {
    let packed = pack_normal_map_linearized(rgba);
    encode_dxt5_crn_mip_chain(&packed, width, height, mip_count, flip, quality_level)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn solid_4x4_block_is_16_bytes() {
        let mut rgba = vec![0u8; 16 * 4];
        for i in 0..16 {
            rgba[i * 4] = 255;
            rgba[i * 4 + 1] = 128;
            rgba[i * 4 + 2] = 127;
            rgba[i * 4 + 3] = 200;
        }
        let (data, mips) = encode_bc5_mip_chain(&rgba, 4, 4, Some(1), false);
        assert_eq!(data.len(), 16);
        assert_eq!(mips, 1);

        assert!(data[0..8].iter().any(|&b| b != 0));
        assert!(data[8..16].iter().any(|&b| b != 0));
    }

    #[test]
    fn mip_chain_1024_matches_prod_raw_byte_count() {
        let mut rgba = vec![0u8; 1024 * 1024 * 4];
        for i in 0..(1024 * 1024) {
            rgba[i * 4] = 255;
            rgba[i * 4 + 1] = 100;
            rgba[i * 4 + 2] = 127;
            rgba[i * 4 + 3] = 200;
        }
        let (data, mips) = encode_bc5_mip_chain(&rgba, 1024, 1024, None, false);
        assert_eq!(mips, 11);

        let bc_blocks =
            256 * 256 + 128 * 128 + 64 * 64 + 32 * 32 + 16 * 16 + 8 * 8 + 4 * 4 + 2 * 2 + 1 + 1 + 1;
        assert_eq!(data.len(), bc_blocks * 16);

        assert_eq!(data.len(), 1_398_128);
    }

    /// xorshift64, deterministic corpus without a dev-dependency.
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
        fn next_u8(&mut self) -> u8 {
            (self.next_u32() & 0xFF) as u8
        }
    }

    fn texpresso_bc5(padded: &[u8], pw: usize, ph: usize) -> Vec<u8> {
        let params = texpresso::Params {
            algorithm: texpresso::Algorithm::RangeFit,
            weights: [1.0, 1.0, 1.0],
            weigh_colour_by_alpha: false,
        };
        let mut out = vec![0u8; (pw / 4) * (ph / 4) * BC5_BLOCK_SIZE];
        texpresso::Format::Bc5.compress(padded, pw, ph, params, &mut out);
        out
    }

    /// The native encoder (and its NEON path on aarch64) must be
    /// byte-for-byte identical to texpresso on random and edge-case inputs.
    #[test]
    fn bc5_native_matches_texpresso_bit_exact() {
        let mut rng = Rng(0xC0FFEE1234ABCDEF);

        for &(w, h) in &[
            (4usize, 4usize),
            (8, 8),
            (16, 16),
            (64, 64),
            (3, 5),
            (7, 9),
            (20, 12),
            (1, 1),
            (2, 2),
            (5, 4),
        ] {
            let mut rgba = vec![0u8; w * h * 4];
            for b in rgba.iter_mut() {
                *b = rng.next_u8();
            }
            let repacked = repack_for_bc5(&rgba);
            let (padded, pw, ph) = pad_to_block_size(&repacked, w, h);
            let mut ours = vec![0u8; (pw / 4) * (ph / 4) * BC5_BLOCK_SIZE];
            compress_bc5_blocks(&padded, pw, ph, &mut ours);
            assert_eq!(
                ours,
                texpresso_bc5(&padded, pw, ph),
                "mismatch for random {w}x{h}"
            );
        }

        let mut edge_blocks: Vec<[u8; 64]> = Vec::new();
        for v in [0u8, 1, 127, 128, 254, 255] {
            edge_blocks.push([v; 64]);
        }
        let mut mix = [0u8; 64];
        for (i, b) in mix.iter_mut().enumerate() {
            *b = if i % 2 == 0 { 0 } else { 255 };
        }
        edge_blocks.push(mix);
        let mut grad = [0u8; 64];
        for (i, b) in grad.iter_mut().enumerate() {
            *b = (i * 4) as u8;
        }
        edge_blocks.push(grad);
        let mut near_flat = [128u8; 64];
        near_flat[0] = 130;
        edge_blocks.push(near_flat);
        let mut one_zero = [200u8; 64];
        one_zero[5] = 0;
        one_zero[9] = 255;
        edge_blocks.push(one_zero);
        for _ in 0..200 {
            let mut blk = [0u8; 64];
            let choices = [0u8, 1, 2, 127, 128, 253, 254, 255];
            for b in blk.iter_mut() {
                *b = choices[(rng.next_u32() % 8) as usize];
            }
            edge_blocks.push(blk);
        }
        for blk in &edge_blocks {
            let mut ours = vec![0u8; BC5_BLOCK_SIZE];
            compress_bc5_blocks(blk, 4, 4, &mut ours);
            assert_eq!(ours, texpresso_bc5(blk, 4, 4), "mismatch for block {blk:?}");
        }

        for &(w, h, flip) in &[(16u32, 16u32, false), (16, 16, true), (24, 10, true)] {
            let mut rgba = vec![0u8; (w * h * 4) as usize];
            for b in rgba.iter_mut() {
                *b = rng.next_u8();
            }
            let (ours, _) = encode_bc5_mip_chain(&rgba, w, h, None, flip);
            let ws = w as usize;
            let hs = h as usize;
            let flipped: Vec<u8> = if flip {
                let mut out = vec![0u8; ws * hs * 4];
                for y in 0..hs {
                    let src = &rgba[(hs - 1 - y) * ws * 4..(hs - y) * ws * 4];
                    out[y * ws * 4..(y + 1) * ws * 4].copy_from_slice(src);
                }
                out
            } else {
                rgba.clone()
            };
            let mip_count = (crate::detmath::log2(w.max(h).max(1) as f64).floor() as i32) + 1;
            let mut cur = flipped;
            let (mut cw, mut ch) = (ws, hs);
            let mut reference: Vec<u8> = Vec::new();
            for m in 0..mip_count {
                let repacked = repack_for_bc5(&cur);
                let (padded, pw, ph) = pad_to_block_size(&repacked, cw, ch);
                reference.extend_from_slice(&texpresso_bc5(&padded, pw, ph));
                if m < mip_count - 1 {
                    let (next, nw, nh) = box_halve_rgba_u8(&cur, cw, ch);
                    cur = next;
                    cw = nw;
                    ch = nh;
                }
            }
            assert_eq!(
                ours, reference,
                "mip-chain mismatch for {w}x{h} flip={flip}"
            );
        }
    }

    /// The (possibly NEON) halver and repacker must match plain scalar
    /// references bit-for-bit on random inputs at even, odd, and degenerate
    /// dimensions.
    #[test]
    fn box_halve_and_repack_match_scalar_reference() {
        let mut rng = Rng(0x0ABCDEF987654321);
        for &(w, h) in &[
            (2usize, 2usize),
            (4, 4),
            (16, 8),
            (6, 10),
            (5, 4),
            (4, 5),
            (7, 7),
            (1, 8),
            (8, 1),
            (1, 1),
            (64, 2),
            (2, 64),
            (34, 34),
        ] {
            let mut rgba = vec![0u8; w * h * 4];
            for b in rgba.iter_mut() {
                *b = rng.next_u8();
            }

            let (ours, nw, nh) = box_halve_rgba_u8(&rgba, w, h);
            let (rnw, rnh) = ((w / 2).max(1), (h / 2).max(1));
            let (fh, fw) = (if h > 1 { 2 } else { 1 }, if w > 1 { 2 } else { 1 });
            let denom = (fh * fw) as u32;
            let mut reference = vec![0u8; rnw * rnh * 4];
            for ny in 0..rnh {
                for nx in 0..rnw {
                    for ch in 0..4 {
                        let mut acc = 0u32;
                        for dy in 0..fh {
                            for dx in 0..fw {
                                acc +=
                                    rgba[(ny * fh + dy) * w * 4 + (nx * fw + dx) * 4 + ch] as u32;
                            }
                        }
                        reference[(ny * rnw + nx) * 4 + ch] = ((acc + denom / 2) / denom) as u8;
                    }
                }
            }
            assert_eq!((nw, nh), (rnw, rnh));
            assert_eq!(ours, reference, "box_halve mismatch at {w}x{h}");

            let repacked = repack_for_bc5(&rgba);
            let mut ref_repack = vec![0u8; rgba.len()];
            for i in 0..(rgba.len() / 4) {
                ref_repack[i * 4] = rgba[i * 4 + 3];
                ref_repack[i * 4 + 1] = rgba[i * 4 + 1];
                ref_repack[i * 4 + 2] = 0;
                ref_repack[i * 4 + 3] = 255;
            }
            assert_eq!(repacked, ref_repack, "repack mismatch at {w}x{h}");
        }
    }

    /// On aarch64 the NEON channel encoder must match the scalar port exactly.
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn bc4_channel_neon_matches_scalar() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        let mut vals = [0u8; 16];
        for round in 0..20_000 {
            for v in vals.iter_mut() {
                *v = if round % 3 == 0 {
                    [0u8, 1, 127, 128, 254, 255][(rng.next_u32() % 6) as usize]
                } else {
                    rng.next_u8()
                };
            }
            let mut scalar = [0u8; 8];
            let mut simd = [0u8; 8];
            encode_bc4_channel_scalar(&vals, &mut scalar);
            encode_bc4_channel(&vals, &mut simd);
            assert_eq!(scalar, simd, "NEON/scalar mismatch for {vals:?}");
        }
    }

    #[test]
    fn repack_maps_alpha_to_red() {
        let rgba = vec![255u8, 200, 127, 99];
        let out = repack_for_bc5(&rgba);
        assert_eq!(out, vec![99u8, 200, 0, 255]);
    }

    #[test]
    fn mip_chain_8x8_byte_count() {
        let rgba = vec![128u8; 8 * 8 * 4];
        let (data, mips) = encode_bc5_mip_chain(&rgba, 8, 8, None, false);
        assert_eq!(mips, 4);
        assert_eq!(data.len(), 7 * 16);
    }

    #[test]
    fn dxt5_crn_chain_is_plain_dxt5_and_flips_rows() {
        let (w, h) = (8usize, 8usize);
        let mut rgba = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                rgba[i] = if y == 0 { 255 } else { 10 };
                rgba[i + 1] = (x * 30) as u8;
                rgba[i + 2] = 180;
                rgba[i + 3] = 100 + (y * 15) as u8;
            }
        }

        let (crn, mips) = encode_dxt5_crn_mip_chain(&rgba, 8, 8, Some(4), true, 255)
            .expect("dxt5 crunch should succeed");
        assert_eq!(mips, 4);
        assert_eq!(&crn[0..2], b"Hx");
        assert_eq!(crn[16], 4, "CRN header levels");
        assert_eq!(
            crn[18] as u32,
            crunch_ffi::CRN_HEADER_FMT_DXT5,
            "fmt-29 payload must declare cCRNFmtDXT5, not DXN"
        );

        let dec = crunch_ffi::crn_decompress_level0(&crn).expect("decode");
        assert_eq!((dec.width, dec.height, dec.levels), (8, 8, 4));

        let bottom_red = dec.rgba[((h - 1) * w) * 4];
        let top_red = dec.rgba[0];
        assert!(
            bottom_red > 200 && top_red < 60,
            "expected flipped rows, got top={top_red} bottom={bottom_red}"
        );

        let mid = dec.rgba[((h / 2) * w + 4) * 4 + 2];
        assert!(mid > 140, "blue channel should survive ~180, got {mid}");
    }

    #[test]
    fn dual_use_chain_is_linearized_dxtnm() {
        let (w, h) = (8usize, 8usize);
        let mut rgba = vec![0u8; w * h * 4];
        for px in rgba.chunks_exact_mut(4) {
            px.copy_from_slice(&[180, 60, 200, 255]);
        }

        let (crn, mips) = encode_dxt5_crn_dual_use_mip_chain(&rgba, 8, 8, Some(1), true, 255)
            .expect("dual-use crunch should succeed");
        assert_eq!(mips, 1);
        assert_eq!(&crn[0..2], b"Hx");
        assert_eq!(crn[18] as u32, crunch_ffi::CRN_HEADER_FMT_DXT5);

        let dec = crunch_ffi::crn_decompress_level0(&crn).expect("decode");
        let lin_r =
            crate::bc7_pure::round_half_up_u8(crate::bc7_pure::srgb_to_linear_u8(180) * 255.0);
        let lin_g =
            crate::bc7_pure::round_half_up_u8(crate::bc7_pure::srgb_to_linear_u8(60) * 255.0);
        let px = &dec.rgba[0..4];
        let close = |a: u8, b: u8| (a as i16 - b as i16).abs() <= 8;
        assert_eq!(px[0], 255, "DXTnm red must be constant 255, got {}", px[0]);
        assert!(
            close(px[1], lin_g) && close(px[2], lin_g),
            "G/B must carry linearized source green {lin_g}, got ({}, {})",
            px[1],
            px[2]
        );
        assert!(
            close(px[3], lin_r),
            "A must carry linearized source red {lin_r}, got {}",
            px[3]
        );
        assert!(
            !close(px[1], 60),
            "content looks PLAIN (g=60), linearization missing"
        );
    }

    #[test]
    fn dxt5_crn_reference_compare() {
        let (Ok(ours_p), Ok(ref_p)) = (
            std::env::var("ABGEN_TEST_CRN_OURS"),
            std::env::var("ABGEN_TEST_CRN_REF"),
        ) else {
            return;
        };
        let ours = std::fs::read(&ours_p).expect("read ABGEN_TEST_CRN_OURS");
        let reff = std::fs::read(&ref_p).expect("read ABGEN_TEST_CRN_REF");

        let od = crunch_ffi::crn_decompress_level0(&ours).expect("decode ours");
        let rd = crunch_ffi::crn_decompress_level0(&reff).expect("decode reference");

        eprintln!(
            "ours: fmt={} {}x{} levels={} bytes={} | ref: fmt={} {}x{} levels={} bytes={}",
            od.format,
            od.width,
            od.height,
            od.levels,
            ours.len(),
            rd.format,
            rd.width,
            rd.height,
            rd.levels,
            reff.len()
        );
        assert_eq!(od.format, crunch_ffi::CRN_HEADER_FMT_DXT5);
        assert_eq!(rd.format, crunch_ffi::CRN_HEADER_FMT_DXT5);
        assert_eq!(
            (od.width, od.height, od.levels),
            (rd.width, rd.height, rd.levels)
        );

        if let Ok(dir) = std::env::var("ABGEN_TEST_CRN_DUMP") {
            std::fs::write(format!("{dir}/ours-l0.rgba"), &od.rgba).unwrap();
            std::fs::write(format!("{dir}/ref-l0.rgba"), &rd.rgba).unwrap();
        }

        let chan_mean = |img: &[u8]| -> [f64; 4] {
            let mut m = [0f64; 4];
            for px in img.chunks_exact(4) {
                for c in 0..4 {
                    m[c] += px[c] as f64;
                }
            }
            let n = (img.len() / 4) as f64;
            m.map(|s| s / n)
        };
        eprintln!(
            "chan means ours={:?} ref={:?}",
            chan_mean(&od.rgba),
            chan_mean(&rd.rgba)
        );

        let mut sum = [0f64; 4];
        let n = (od.width * od.height) as f64;
        for (a, b) in od.rgba.chunks_exact(4).zip(rd.rgba.chunks_exact(4)) {
            for c in 0..4 {
                sum[c] += (a[c] as f64 - b[c] as f64).abs();
            }
        }
        let mae: Vec<f64> = sum.iter().map(|s| s / n).collect();
        eprintln!("per-channel level-0 MAE vs reference: {mae:?}");
    }
}
