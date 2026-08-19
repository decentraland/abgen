use abgen::bc7_pure::{encode_blocks, Params};

fn xorshift64(s: &mut u64) -> u64 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    x
}

fn fnv1a(data: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn corpus(num_blocks: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    let mut out = Vec::with_capacity(num_blocks * 64);
    for bi in 0..num_blocks {
        let mut block = [0u8; 64];
        match bi % 8 {
            0 => {
                let r = (xorshift64(&mut s) & 0xff) as u8;
                let g = (xorshift64(&mut s) & 0xff) as u8;
                let b = (xorshift64(&mut s) & 0xff) as u8;
                for px in 0..16 {
                    block[px * 4..px * 4 + 4].copy_from_slice(&[r, g, b, 255]);
                }
            }
            1 => {
                for px in 0..16 {
                    let v = if (px + px / 4) % 2 == 0 { 0 } else { 255 };
                    block[px * 4..px * 4 + 4].copy_from_slice(&[v, v, v, 255]);
                }
            }
            2 => {
                for px in 0..16 {
                    let r = (xorshift64(&mut s) & 0xff) as u8;
                    let g = (xorshift64(&mut s) & 0xff) as u8;
                    let b = (xorshift64(&mut s) & 0xff) as u8;
                    let a = if px < 8 { 0 } else { 255 };
                    block[px * 4..px * 4 + 4].copy_from_slice(&[r, g, b, a]);
                }
            }
            3 => {
                for px in 0..16 {
                    let v = xorshift64(&mut s);
                    block[px * 4..px * 4 + 4].copy_from_slice(&(v as u32).to_le_bytes());
                }
            }
            4 => {
                let base = (xorshift64(&mut s) & 0x7f) as u8;
                for px in 0..16 {
                    let v = base + (px as u8) * 8;
                    block[px * 4..px * 4 + 4].copy_from_slice(&[v, v / 2 + base, 255 - v, 255]);
                }
            }
            5 => {
                for px in 0..16 {
                    let r = (xorshift64(&mut s) & 0xff) as u8;
                    let g = (xorshift64(&mut s) & 0xff) as u8;
                    let b = (xorshift64(&mut s) & 0xff) as u8;
                    block[px * 4..px * 4 + 4].copy_from_slice(&[r, g, b, (px as u8) * 17]);
                }
            }
            6 => {
                let r = (xorshift64(&mut s) & 0xff) as u8;
                for px in 0..16 {
                    block[px * 4..px * 4 + 4].copy_from_slice(&[r, r, r, 255]);
                }
                block[0..4].copy_from_slice(&[255 - r, r, 255, 255]);
            }
            _ => {
                let c0 = xorshift64(&mut s) as u32;
                let c1 = xorshift64(&mut s) as u32;
                for px in 0..16 {
                    let c = if px % 3 == 0 { c0 } else { c1 };
                    let mut bytes = c.to_le_bytes();
                    bytes[3] = if px % 5 == 0 { 0 } else { 255 };
                    block[px * 4..px * 4 + 4].copy_from_slice(&bytes);
                }
            }
        }
        out.extend_from_slice(&block);
    }
    out
}

#[test]
fn parity_hash() {
    let num_blocks = 768;
    let blocks = corpus(num_blocks, 0x9E3779B97F4A7C15);
    for (name, p, want) in [
        ("basic", Params::basic(false), 0x5682f7ac302e8f18),
        ("slow", Params::slow(false), 0xe9faa6536ebc09cc),
        ("basic-perceptual", Params::basic(true), 0xa50af210824b832a),
        ("slow-perceptual", Params::slow(true), 0x2c9adafb6d78b5c8),
    ] {
        let enc = encode_blocks(&blocks, num_blocks, &p);
        let got = fnv1a(&enc);
        println!("PARITY {name} {got:016x}");
        assert_eq!(got, want, "BC7 {name} corpus hash moved");
    }
}
