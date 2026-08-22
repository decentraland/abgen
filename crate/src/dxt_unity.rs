use image::RgbaImage;
use rayon::prelude::*;

pub fn encode_rgba32(image: &RgbaImage, flip: bool) -> Vec<u8> {
    let (w, h) = image.dimensions();
    let src = image.as_raw();
    let row_len = (w * 4) as usize;
    let mut out = vec![0u8; (w * h * 4) as usize];
    out.par_chunks_mut(row_len)
        .enumerate()
        .for_each(|(y, dst)| {
            let row = if flip { h - 1 - y as u32 } else { y as u32 };
            for x in 0..w as usize {
                let i = ((row * w) as usize + x) * 4;
                let o = x * 4;
                dst[o] = src[i + 3];
                dst[o + 1] = src[i];
                dst[o + 2] = src[i + 1];
                dst[o + 3] = src[i + 2];
            }
        });
    out
}

pub fn encode_rgb24(image: &RgbaImage, flip: bool) -> Vec<u8> {
    let (w, h) = image.dimensions();
    let src = image.as_raw();
    let row_len = (w * 3) as usize;
    let mut out = vec![0u8; (w * h * 3) as usize];
    out.par_chunks_mut(row_len)
        .enumerate()
        .for_each(|(y, dst)| {
            let row = if flip { h - 1 - y as u32 } else { y as u32 };
            for x in 0..w as usize {
                let i = ((row * w) as usize + x) * 4;
                let o = x * 3;
                dst[o] = src[i];
                dst[o + 1] = src[i + 1];
                dst[o + 2] = src[i + 2];
            }
        });
    out
}

#[cfg(test)]
fn encode_rgba32_serial(image: &RgbaImage, flip: bool) -> Vec<u8> {
    let (w, h) = image.dimensions();
    let src = image.as_raw();
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let row = if flip { h - 1 - y } else { y };
        for x in 0..w {
            let i = ((row * w + x) * 4) as usize;
            out.push(src[i + 3]);
            out.push(src[i]);
            out.push(src[i + 1]);
            out.push(src[i + 2]);
        }
    }
    out
}

#[cfg(test)]
fn encode_rgb24_serial(image: &RgbaImage, flip: bool) -> Vec<u8> {
    let (w, h) = image.dimensions();
    let src = image.as_raw();
    let mut out = Vec::with_capacity((w * h * 3) as usize);
    for y in 0..h {
        let row = if flip { h - 1 - y } else { y };
        for x in 0..w {
            let i = ((row * w + x) * 4) as usize;
            out.push(src[i]);
            out.push(src[i + 1]);
            out.push(src[i + 2]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_pixel(r: u8, g: u8, b: u8, a: u8) -> RgbaImage {
        RgbaImage::from_raw(1, 1, vec![r, g, b, a]).unwrap()
    }

    #[test]
    fn rgba32_is_argb_byte_order() {
        let img = one_pixel(0x10, 0x20, 0x30, 0x40);
        let argb = encode_rgba32(&img, false);
        assert_eq!(argb, vec![0x40, 0x10, 0x20, 0x30], "ARGB byte order");
    }

    fn lcg_image(w: u32, h: u32, mut seed: u32) -> RgbaImage {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        for b in buf.iter_mut() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            *b = (seed >> 24) as u8;
        }
        RgbaImage::from_raw(w, h, buf).unwrap()
    }

    #[test]
    fn rgba32_matches_serial_various_sizes() {
        for &(w, h) in &[(1u32, 1u32), (3, 5), (7, 4), (64, 64)] {
            for flip in [false, true] {
                let img = lcg_image(w, h, 0x1234_5678 ^ (w * 31 + h));
                let par = encode_rgba32(&img, flip);
                let serial = encode_rgba32_serial(&img, flip);
                assert_eq!(par, serial, "rgba32 mismatch at {}x{} flip={}", w, h, flip);
            }
        }
    }

    #[test]
    fn rgb24_matches_serial_various_sizes() {
        for &(w, h) in &[(1u32, 1u32), (3, 5), (7, 4), (64, 64)] {
            for flip in [false, true] {
                let img = lcg_image(w, h, 0x9abc_def0 ^ (w * 17 + h));
                let par = encode_rgb24(&img, flip);
                let serial = encode_rgb24_serial(&img, flip);
                assert_eq!(par, serial, "rgb24 mismatch at {}x{} flip={}", w, h, flip);
            }
        }
    }

    #[test]
    fn rgba32_thread_count_invariant() {
        let img = lcg_image(64, 48, 0x2468_ace0);
        let global = encode_rgba32(&img, true);
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let single = pool.install(|| encode_rgba32(&img, true));
        assert_eq!(global, single, "rgba32 must be invariant to thread count");
    }
}
