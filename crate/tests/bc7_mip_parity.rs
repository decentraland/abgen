//! Byte-identity gate for `encode_bc7_mip_chain_with_profile` over a matrix of
//! sizes (aligned, odd, 1-pixel, extreme aspect), flip, srgb, perceptual and
//! both profiles. o15_parity covers the RGBA32 mip chain and raw block
//! encoding; nothing covered the BC7 chain driver itself, which is where the
//! per-level copies and the block->task partitioning live.
//!
//! ```sh
//! cargo test --no-default-features --test bc7_mip_parity -- --nocapture
//! ```

use abgen::bc7_pure::{encode_bc7_mip_chain_with_profile, Bc7Profile};

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

fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

fn source(w: u32, h: u32, rng: &mut Rng) -> Vec<u8> {
    let n = (w * h * 4) as usize;
    let mut rgba = vec![0u8; n];
    for (i, px) in rgba.chunks_exact_mut(4).enumerate() {
        let wash = ((i as u32 % w) * 255 / w.max(1) + (i as u32 / w) * 255 / h.max(1)) / 2;
        let jitter = rng.next_u32() % 64;
        px[0] = ((wash + jitter) % 256) as u8;
        px[1] = ((wash * 2 + jitter) % 256) as u8;
        px[2] = ((wash / 2 + jitter) % 256) as u8;
        px[3] = if i % 7 == 0 {
            (rng.next_u32() % 256) as u8
        } else {
            255
        };
    }
    rgba
}

#[test]
fn bc7_mip_chain_hash() {
    let sizes: &[(u32, u32)] = &[
        (1, 1),
        (3, 5),
        (4, 4),
        (7, 7),
        (16, 16),
        (31, 33),
        (1, 64),
        (64, 1),
        (64, 64),
        (129, 96),
        (128, 128),
    ];
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for &(w, h) in sizes {
        let rgba = source(w, h, &mut rng);
        for &(mips, flip, srgb, perceptual) in &[
            (None, false, false, false),
            (None, true, true, true),
            (Some(1i32), false, true, false),
            (Some(3i32), true, false, true),
        ] {
            let (out, count) = encode_bc7_mip_chain_with_profile(
                &rgba,
                w,
                h,
                mips,
                flip,
                srgb,
                perceptual,
                Bc7Profile::Basic,
            );
            fnv1a(&mut hash, &out);
            fnv1a(&mut hash, &count.to_le_bytes());
        }
        // Slow is the production profile for basecolor; keep it on the small
        // sizes so the test stays under a few seconds.
        if w * h <= 64 * 64 {
            let (out, count) = encode_bc7_mip_chain_with_profile(
                &rgba,
                w,
                h,
                None,
                false,
                true,
                true,
                Bc7Profile::Slow,
            );
            fnv1a(&mut hash, &out);
            fnv1a(&mut hash, &count.to_le_bytes());
        }
    }
    println!("BC7_MIP_CHAIN_HASH {hash:016x}");
    assert_eq!(hash, 0x7bc8_4aef_323c_1292, "BC7 mip chain hash moved");
}
