fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
}

#[test]
fn dxt1_corpus_hash() {
    let sizes: &[(u32, u32)] = &[
        (1, 1),
        (1, 8),
        (8, 1),
        (2, 2),
        (3, 5),
        (4, 4),
        (7, 7),
        (16, 16),
        (31, 33),
        (64, 64),
        (128, 128),
        (5, 128),
    ];
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    let mut s = 0x0123_4567_89AB_CDEFu64;
    for &(w, hh) in sizes {
        for variant in 0..4u32 {
            let n = (w * hh * 4) as usize;
            let mut rgba = vec![0u8; n];
            match variant {
                0 => {
                    for v in rgba.iter_mut() {
                        *v = xorshift(&mut s) as u8;
                    }
                }
                1 => {
                    for (i, v) in rgba.iter_mut().enumerate() {
                        *v = if i % 4 == 3 { 255 } else { 128 };
                    }
                }
                2 => {
                    for (i, v) in rgba.iter_mut().enumerate() {
                        let px = i / 4;
                        *v = match i % 4 {
                            3 => {
                                if px % 3 == 0 {
                                    0
                                } else {
                                    255
                                }
                            }
                            _ => {
                                if px % 2 == 0 {
                                    0
                                } else {
                                    255
                                }
                            }
                        };
                    }
                }
                _ => {
                    for (i, v) in rgba.iter_mut().enumerate() {
                        *v = ((i * 7 + 1) % 256) as u8;
                    }
                }
            }
            for &srgb in &[false, true] {
                for &flip in &[false, true] {
                    for &mips in &[None, Some(1), Some(3)] {
                        let (data, m) =
                            abgen::dxt1_pure::encode_dxt1_mip_chain(&rgba, w, hh, mips, flip, srgb);
                        fnv1a(&mut h, &data);
                        fnv1a(&mut h, &m.to_le_bytes());
                    }
                }
            }
        }
    }
    println!("DXT1_CORPUS_HASH={h:#018x}");
    assert_eq!(h, 0x31ad_dec8_4de0_f44b, "DXT1 corpus hash moved");
}
