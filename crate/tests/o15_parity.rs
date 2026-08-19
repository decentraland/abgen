//! Prints an FNV-1a hash of encoder output over a deterministic corpus of
//! random + edge-case blocks. Run before and after a change and diff the
//! printed hashes to verify byte-for-byte identical encoded blocks.
//!
//! ```sh
//! cargo test --no-default-features --test o15_parity -- --nocapture
//! ```

use abgen::bc7_pure::{encode_blocks, encode_rgba32_mip_chain, Params};

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

fn corpus() -> Vec<u8> {
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let nblocks = 2048usize;
    let mut out = vec![0u8; nblocks * 64];
    for (bi, block) in out.chunks_exact_mut(64).enumerate() {
        match bi % 8 {
            0 | 1 => {
                for b in block.iter_mut() {
                    *b = rng.next_u32() as u8;
                }
            }
            2 => {
                for px in block.chunks_exact_mut(4) {
                    px[0] = rng.next_u32() as u8;
                    px[1] = rng.next_u32() as u8;
                    px[2] = rng.next_u32() as u8;
                    px[3] = [0u8, 1, 254, 255][(rng.next_u32() % 4) as usize];
                }
            }
            3 => {
                let c = [
                    rng.next_u32() as u8,
                    rng.next_u32() as u8,
                    rng.next_u32() as u8,
                ];
                for px in block.chunks_exact_mut(4) {
                    px[0] = c[0];
                    px[1] = c[1];
                    px[2] = c[2];
                    px[3] = rng.next_u32() as u8;
                }
            }
            4 => {
                let a = (rng.next_u32() % 255) as u8;
                for px in block.chunks_exact_mut(4) {
                    px[0] = rng.next_u32() as u8;
                    px[1] = rng.next_u32() as u8;
                    px[2] = rng.next_u32() as u8;
                    px[3] = a;
                }
            }
            5 => {
                let c = [
                    rng.next_u32() as u8,
                    rng.next_u32() as u8,
                    rng.next_u32() as u8,
                    rng.next_u32() as u8,
                ];
                for px in block.chunks_exact_mut(4) {
                    px.copy_from_slice(&c);
                }
            }
            6 => {
                for b in block.iter_mut() {
                    *b = if rng.next_u32().is_multiple_of(2) {
                        0
                    } else {
                        255
                    };
                }
            }
            _ => {
                for px in block.chunks_exact_mut(4) {
                    px[0] = rng.next_u32() as u8;
                    px[1] = rng.next_u32() as u8;
                    px[2] = rng.next_u32() as u8;
                    px[3] = 255;
                }
            }
        }
    }
    out
}

#[test]
fn o15_corpus_hash() {
    let blocks = corpus();
    let n = blocks.len() / 64;
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for params in [
        Params::basic(false),
        Params::basic(true),
        Params::slow(false),
        Params::slow(true),
    ] {
        let enc = encode_blocks(&blocks, n, &params);
        fnv1a(&mut h, &enc);
    }
    let mut rng = Rng(0xfeed_face_dead_beef);
    for (w, h2, flip, srgb) in [
        (37u32, 53u32, false, false),
        (64, 64, true, true),
        (129, 96, false, true),
        (1, 17, false, false),
    ] {
        let mut rgba = vec![0u8; (w * h2 * 4) as usize];
        for b in rgba.iter_mut() {
            *b = rng.next_u32() as u8;
        }
        let (mips, count) = encode_rgba32_mip_chain(&rgba, w, h2, None, flip, srgb);
        fnv1a(&mut h, &mips);
        fnv1a(&mut h, &count.to_le_bytes());
    }
    println!("O15_CORPUS_HASH {h:016x}");
    assert_eq!(h, 0x4dfc_8f8b_0e7d_7be4, "O15 corpus hash moved");
}
