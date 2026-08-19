//! Byte-parity dump for the vendored third-party codecs (crunch DXT5-CRN
//! encode, libjpeg9c decode). Run under two builds and compare hashes:
//!
//! ```sh
//! ABGEN_CRN_PARITY_OUT=/tmp/dump.bin cargo test --test crn_parity -- --nocapture
//! sha256sum /tmp/dump.bin
//! ```
//!
//! Without the env var the test is a no-op, so it never slows CI down.

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

fn noise(w: usize, h: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng(seed | 1);
    (0..w * h * 4)
        .map(|_| (rng.next_u32() & 0xff) as u8)
        .collect()
}

fn flat(w: usize, h: usize, px: [u8; 4]) -> Vec<u8> {
    px.iter().copied().cycle().take(w * h * 4).collect()
}

/// 0/255 checkerboard at texel scale: worst case for endpoint selection.
fn extremes(w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let v = if (x + y) % 2 == 0 { 255 } else { 0 };
            let i = (y * w + x) * 4;
            out[i..i + 4].copy_from_slice(&[v, 255 - v, v, 255]);
        }
    }
    out
}

/// Hard alpha edges on 8x8 tiles over noisy RGB.
fn alpha_edge(w: usize, h: usize) -> Vec<u8> {
    let mut out = noise(w, h, 0xA1FAED6E);
    for y in 0..h {
        for x in 0..w {
            out[(y * w + x) * 4 + 3] = if (x / 8 + y / 8) % 2 == 0 { 0 } else { 255 };
        }
    }
    out
}

fn push(all: &mut Vec<u8>, tag: &str, mips: i32, bytes: &[u8]) {
    all.extend_from_slice(tag.as_bytes());
    all.extend_from_slice(&mips.to_le_bytes());
    all.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    all.extend_from_slice(bytes);
}

#[test]
fn parity_dump() {
    let Ok(path) = std::env::var("ABGEN_CRN_PARITY_OUT") else {
        return;
    };
    let mut all: Vec<u8> = Vec::new();

    let cases: Vec<(&str, u32, u32, Vec<u8>)> = vec![
        ("noise64", 64, 64, noise(64, 64, 0x9E3779B9)),
        ("flat_mid64", 64, 64, flat(64, 64, [128, 128, 128, 255])),
        ("flat_black32", 32, 32, flat(32, 32, [0, 0, 0, 0])),
        ("flat_white32", 32, 32, flat(32, 32, [255, 255, 255, 255])),
        ("extremes32", 32, 32, extremes(32, 32)),
        ("alpha64", 64, 64, alpha_edge(64, 64)),
        ("rect48x16", 48, 16, noise(48, 16, 0x2545F491)),
    ];
    for (name, w, h, rgba) in &cases {
        for q in [64u32, 255] {
            let (bytes, mips) =
                abgen::bc5_pure::encode_dxt5_crn_mip_chain(rgba, *w, *h, None, true, q)
                    .unwrap_or_else(|| panic!("crn encode failed: {name} q{q}"));
            push(&mut all, &format!("{name}:q{q}"), mips, &bytes);
        }
    }

    let nm = noise(32, 32, 0xD1B54A32);
    let (bytes, mips) =
        abgen::bc5_pure::encode_dxt5_crn_dual_use_mip_chain(&nm, 32, 32, None, true, 255)
            .expect("dual-use crn encode failed");
    push(&mut all, "dualuse32:q255", mips, &bytes);

    let (w, h) = (256usize, 256usize);
    let rgba = noise(w, h, 0xC0FFEE);
    let rgb: Vec<u8> = rgba.chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpg, 85)
        .encode(&rgb, w as u32, h as u32, image::ExtendedColorType::Rgb8)
        .expect("jpeg encode");
    for fancy in [false, true] {
        let (out, dw, dh) = libjpeg9c::decode_rgba(&jpg, fancy).expect("jpeg decode");
        assert_eq!((dw as usize, dh as usize), (w, h));
        push(&mut all, &format!("jpeg:fancy{fancy}"), 0, &out);
    }

    std::fs::write(&path, &all).expect("write parity dump");
    println!("parity dump: {} bytes -> {path}", all.len());
}
