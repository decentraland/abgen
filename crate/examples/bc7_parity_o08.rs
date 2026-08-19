fn xs(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn fnv1a(h: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *h ^= b as u64;
        *h = h.wrapping_mul(0x100000001b3);
    }
}

fn main() {
    let mut st = 0x243f6a8885a308d3u64;
    let mut blocks: Vec<u8> = Vec::new();

    for _ in 0..4096 {
        for _ in 0..64 {
            blocks.push((xs(&mut st) & 0xff) as u8);
        }
    }
    for _ in 0..512 {
        for i in 0..64 {
            if i % 4 == 3 {
                blocks.push(255);
            } else {
                blocks.push((xs(&mut st) & 0xff) as u8);
            }
        }
    }
    for c in [
        [0u8, 0, 0, 0],
        [255, 255, 255, 255],
        [0, 0, 0, 255],
        [255, 255, 255, 0],
        [1, 2, 3, 4],
        [128, 128, 128, 128],
        [255, 0, 0, 255],
        [0, 255, 0, 128],
        [0, 0, 255, 1],
        [254, 1, 254, 254],
    ] {
        for _ in 0..16 {
            blocks.extend_from_slice(&c);
        }
    }
    for _ in 0..256 {
        for i in 0..16 {
            blocks.push((xs(&mut st) & 0xff) as u8);
            blocks.push((xs(&mut st) & 0xff) as u8);
            blocks.push((xs(&mut st) & 0xff) as u8);
            blocks.push(if i < 8 { 0 } else { 255 });
        }
    }
    for _ in 0..256 {
        for i in 0..16u32 {
            blocks.push((xs(&mut st) & 0xff) as u8);
            blocks.push((xs(&mut st) & 0xff) as u8);
            blocks.push((xs(&mut st) & 0xff) as u8);
            blocks.push((i * 17) as u8);
        }
    }
    for _ in 0..256 {
        let base = (xs(&mut st) & 0xff) as u8;
        for _ in 0..16 {
            blocks.push(base);
            blocks.push(base.wrapping_add((xs(&mut st) & 1) as u8));
            blocks.push(base);
            blocks.push(255);
        }
    }
    for _ in 0..256 {
        for _ in 0..16 {
            for _ in 0..4 {
                blocks.push(if xs(&mut st) & 1 == 1 { 255 } else { 0 });
            }
        }
    }

    let n = blocks.len() / 64;
    let mut h = 0xcbf29ce484222325u64;
    for (name, params) in [
        ("basic-np", abgen::bc7_pure::Params::basic(false)),
        ("slow-np", abgen::bc7_pure::Params::slow(false)),
        ("basic-p", abgen::bc7_pure::Params::basic(true)),
        ("slow-p", abgen::bc7_pure::Params::slow(true)),
    ] {
        let out = abgen::bc7_pure::encode_blocks(&blocks, n, &params);
        let mut hp = 0xcbf29ce484222325u64;
        fnv1a(&mut hp, &out);
        println!("{name}: {hp:016x} ({} blocks)", n);
        fnv1a(&mut h, &out);
    }
    println!("TOTAL: {h:016x}");
}
