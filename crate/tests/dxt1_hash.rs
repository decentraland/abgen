//! End-to-end output-identity probe for the DXT1 encoder (throwaway).
//! Prints an FNV-1a hash of the full encoded mip chain for a corpus of
//! generated textures; run before and after a change and diff the lines.

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

fn texture_rgba(w: usize, h: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let wash = ((x * 255 / w.max(1)) as u32 + (y * 255 / h.max(1)) as u32) / 2;
            let jitter = rng.next_u32() % 64;
            out[i] = ((wash + jitter) % 256) as u8;
            out[i + 1] = ((wash * 2 + jitter) % 256) as u8;
            out[i + 2] = ((wash / 2 + jitter) % 256) as u8;
            out[i + 3] = if (x / 8 + y / 8) % 5 == 0 { 128 } else { 255 };
        }
    }
    out
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[test]
fn dxt1_output_hashes() {
    let cases: &[(usize, usize, u64)] = &[
        (256, 256, 0x2545F491),
        (1024, 1024, 0x2545F491),
        (64, 48, 0xDEADBEEF),
        (50, 34, 0x12345678),
        (7, 5, 0xABCDEF01),
        (1, 1, 0x55555555),
    ];
    for &(w, h, seed) in cases {
        let rgba = texture_rgba(w, h, seed);
        for srgb in [false, true] {
            for flip in [false, true] {
                for mips in [None, Some(1)] {
                    let (data, n) = abgen::dxt1_pure::encode_dxt1_mip_chain(
                        &rgba, w as u32, h as u32, mips, flip, srgb,
                    );
                    println!(
                        "HASH {w}x{h} seed={seed:#x} srgb={srgb} flip={flip} mips={mips:?} -> n={n} len={} h={:#018x}",
                        data.len(),
                        fnv1a(&data)
                    );
                }
            }
        }
    }
}
