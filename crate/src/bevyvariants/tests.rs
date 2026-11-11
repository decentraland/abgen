use super::model::{transform_model, KTX2_MIME};
use super::stats::{NoVariant, StageTimes};
use super::texture::{
    choose_format, decode_bc1_rgba, encode_texture, encode_texture_ungated, encode_texture_with,
    psnr_rgb_db, transcode_uastc_bc7, EncodePath, Slot, TexFormat, TexOptions, TextureVariant,
    Uses, BC1_MIN_PSNR_DB,
};
use super::*;
use crate::bvwebgpu::emit;
use crate::bvwebgpu::emit::tests::{mk_glb, noise_png_bytes, png_bytes, quad_bin_and_views};
use crate::catalyst::{ContentEntry, Scene};
use crate::ktx2::Ktx2Header;
use crate::ktx2_write::{read_levels, BcFormat};
use serde_json::{json, Value as J};
use std::collections::HashMap;

const UASTC_8: &[u8] = include_bytes!("../../testdata/uastc_8.ktx2");
const UASTC_6: &[u8] = include_bytes!("../../testdata/uastc_6.ktx2");

/// A variant KTX2: its header and its decompressed levels (level 0 first), checked against
/// the BC format it claims.
fn ktx(bytes: &[u8], format: BcFormat) -> (Ktx2Header, Vec<Vec<u8>>) {
    let h = crate::ktx2::read_header(bytes).expect("KTX2 header");
    assert_eq!(h.vk_format, format.vk_format());
    assert_eq!(h.supercompression, crate::ktx2_write::SUPERCOMPRESSION_ZSTD);
    let levels = read_levels(bytes).expect("levels decode");
    assert_eq!(levels.len() as u32, h.levels);
    for (m, l) in levels.iter().enumerate() {
        let (w, hh) = ((h.width >> m).max(1), (h.height >> m).max(1));
        assert_eq!(
            l.len(),
            crate::ktx2_write::level_bytes(format, w, hh),
            "level {m}"
        );
    }
    (h, levels)
}

/// What the explorer's local path transcodes: the first level at most 1024 on a side, to BC7.
fn client_bc7_level(raw: &[u8]) -> Vec<u8> {
    let t = basisu::Transcoder::new(raw).unwrap();
    let level = (0..t.level_count())
        .find(|&l| {
            t.image_level_info(l)
                .is_ok_and(|i| i.width.max(i.height) <= 1024)
        })
        .unwrap();
    t.transcode(
        level,
        basisu::TargetFormat::Bc7Rgba,
        basisu::DecodeFlags::NONE,
    )
    .unwrap()
}

fn noise_alpha_png(w: u32, h: u32) -> Vec<u8> {
    let mut s = 0x1234_5678u32;
    let mut img = image::RgbaImage::new(w, h);
    for p in img.pixels_mut() {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let b = s.to_le_bytes();
        *p = image::Rgba([b[0], b[1], b[2], b[3] | 1]);
    }
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

/// An opaque image BC1 represents well (a smooth two-axis gradient) that PNG still cannot
/// shrink much (a deterministic +-3 dither), so it passes both the BC1 PSNR rule and the size
/// gate at any size from 16 px up.
fn smooth_png_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut s = 0x9e37_79b9u32;
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, p) in img.enumerate_pixels_mut() {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let d = |k: u32| ((s >> (8 * k)) % 7) as i32 - 3;
        let r = (x * 200 / w.max(1)) as i32 + 20 + d(0);
        let g = (y * 200 / h.max(1)) as i32 + 20 + d(1);
        let b = ((x + y) * 100 / (w + h).max(1)) as i32 + 60 + d(2);
        *p = image::Rgba([
            r.clamp(0, 255) as u8,
            g.clamp(0, 255) as u8,
            b.clamp(0, 255) as u8,
            255,
        ]);
    }
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

/// An opaque image BC1 encodes losslessly: every 4x4 block has two 565 endpoints (c0 > c1) and
/// texels drawn from the encoder's palette of them, with both extremes present so the encoder's
/// principal-axis pick returns exactly those endpoints. Random per block, so PNG cannot shrink it
/// much.
fn bc1_exact_png_bytes(w: u32, h: u32) -> Vec<u8> {
    use crate::gpu::corelib::dxt1::{pack_565, unpack_565};
    let mut s = 0x1357_9bdfu32;
    let mut next = || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        s >> 8
    };
    let mut img = image::RgbaImage::new(w, h);
    for by in 0..h.div_ceil(4) {
        for bx in 0..w.div_ceil(4) {
            let (mut c0, mut c1) = (0u16, 0u16);
            while c0 == c1 {
                let a = next();
                let b = next();
                c0 = pack_565(
                    (a & 255) as u8,
                    ((a >> 8) & 255) as u8,
                    ((a >> 16) & 255) as u8,
                );
                c1 = pack_565(
                    (b & 255) as u8,
                    ((b >> 8) & 255) as u8,
                    ((b >> 16) & 255) as u8,
                );
            }
            if c0 < c1 {
                std::mem::swap(&mut c0, &mut c1);
            }
            let (e0, e1) = (unpack_565(c0), unpack_565(c1));
            let mix = |a: u8, b: u8, wa: u16, wb: u16| ((wa * a as u16 + wb * b as u16) / 3) as u8;
            let palette = [
                e0,
                e1,
                [
                    mix(e0[0], e1[0], 2, 1),
                    mix(e0[1], e1[1], 2, 1),
                    mix(e0[2], e1[2], 2, 1),
                ],
                [
                    mix(e0[0], e1[0], 1, 2),
                    mix(e0[1], e1[1], 1, 2),
                    mix(e0[2], e1[2], 1, 2),
                ],
            ];
            for t in 0..16u32 {
                let (x, y) = (bx * 4 + t % 4, by * 4 + t / 4);
                if x >= w || y >= h {
                    continue;
                }
                let k = match t {
                    0 => 0,
                    15 => 1,
                    _ => (next() % 4) as usize,
                };
                let c = palette[k];
                img.put_pixel(x, y, image::Rgba([c[0], c[1], c[2], 255]));
            }
        }
    }
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

/// The encode before the size gate.
fn ungated(bytes: &[u8], slot: Slot) -> TextureVariant {
    match encode_texture_ungated(
        bytes,
        slot,
        TexOptions::default(),
        &mut StageTimes::default(),
    )
    .unwrap()
    {
        Ok(v) => v,
        Err(m) => panic!("no variant: {m:?}"),
    }
}

fn miss(bytes: &[u8], slot: Slot) -> super::texture::Miss {
    encode_texture_with(
        bytes,
        slot,
        TexOptions::default(),
        &mut StageTimes::default(),
    )
    .unwrap()
    .expect_err("no variant")
}

fn split(glb: &[u8]) -> (J, Vec<u8>) {
    let (json_bytes, bin) = emit::split_container(glb).unwrap();
    (serde_json::from_slice(&json_bytes).unwrap(), bin)
}

fn view_bytes<'a>(doc: &J, bin: &'a [u8], view: usize) -> &'a [u8] {
    let v = &doc["bufferViews"][view];
    let off = v["byteOffset"].as_u64().unwrap_or(0) as usize;
    let len = v["byteLength"].as_u64().unwrap() as usize;
    &bin[off..off + len]
}

fn accessor_bytes(doc: &J, bin: &[u8], acc: usize, elem: usize) -> Vec<u8> {
    let a = &doc["accessors"][acc];
    let v = view_bytes(doc, bin, a["bufferView"].as_u64().unwrap() as usize);
    let off = a["byteOffset"].as_u64().unwrap_or(0) as usize;
    let n = a["count"].as_u64().unwrap() as usize;
    v[off..off + n * elem].to_vec()
}

fn image_ktx(doc: &J, bin: &[u8], image: usize, format: BcFormat) -> (Ktx2Header, Vec<Vec<u8>>) {
    let img = &doc["images"][image];
    assert_eq!(img["mimeType"], KTX2_MIME);
    assert!(img.get("uri").is_none());
    ktx(
        view_bytes(doc, bin, img["bufferView"].as_u64().unwrap() as usize),
        format,
    )
}

fn geometry(glb: &[u8]) -> Vec<(Vec<[f64; 3]>, Vec<u32>)> {
    let scene = crate::gltf::parse(glb, ".glb", None, false, false).unwrap();
    scene
        .nodes
        .iter()
        .flat_map(|n| n.primitives.iter())
        .map(|p| (p.positions.clone(), p.indices.clone()))
        .collect()
}

#[test]
fn cid_matches_known_vectors() {
    assert_eq!(
        cid_v1_raw_sha256(b""),
        "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku"
    );
    assert_eq!(
        cid_v1_raw_sha256(b"hello world"),
        "bafkreifzjut3te2nhyekklss27nh3k72ysco7y32koao5eei66wof36n5e"
    );
    assert!(is_id(&cid_v1_raw_sha256(b"x")));
}

#[test]
fn ids_and_paths_refuse_traversal_and_previews() {
    let root = std::path::Path::new("/out");
    assert_eq!(
        manifest_path(root, "bafkent").unwrap(),
        std::path::Path::new("/out/bevy/manifest/bafkent.json")
    );
    assert_eq!(
        variant_path(root, "t1", "bafkreiabc_srgb").unwrap(),
        std::path::Path::new("/out/bevy/t1/bafkreiabc_srgb")
    );
    assert_eq!(
        key_path(root, "bevy/m1/bafkreiabc.json").unwrap(),
        std::path::Path::new("/out/bevy/m1/bafkreiabc.json")
    );
    for bad in ["", "..", "a/b", "b64-abc", "x.json", "a b"] {
        assert!(manifest_path(root, bad).is_none(), "{bad}");
        assert!(variant_path(root, "t1", bad).is_none(), "{bad}");
    }
    for bad in [
        "", "manifest", "contents", "t", "x1", "t1z", "tz9", "t1/x", "t1.1", "t1e", "te2",
        "t1e2z9", "t1ee2",
    ] {
        assert!(!is_prefix(bad), "{bad}");
        assert!(variant_path(root, bad, "bafkreiabc").is_none(), "{bad}");
    }
    assert!(is_prefix("t1") && is_prefix("m12") && is_prefix("g1z9") && is_prefix("t1z19"));
    assert!(is_prefix("t1e2") && is_prefix("t1z9e2") && is_prefix("m1e0"));
    assert!(key_path(root, "bevy/contents/bafkreiabc").is_none());
    assert!(key_path(root, "bevy/t1/a/b").is_none());
    assert_eq!(manifest_key("e"), "bevy/manifest/e.json");
    assert_eq!(variant_key("t1", "c_srgb"), "bevy/t1/c_srgb");
    assert_eq!(record_key("bevy/t1/c_srgb"), "bevy/t1/c_srgb.json");
}

#[test]
fn lane_prefixes_follow_generations_and_the_zstd_level() {
    let d = crate::ktx2_write::DEFAULT_ZSTD_LEVEL;
    assert_eq!(Class::Texture.prefix(d, None), format!("t{TEX_GEN}"));
    assert_eq!(Class::Texture.prefix(9, None), format!("t{TEX_GEN}z9"));
    assert_eq!(Class::Mesh.prefix(9, None), format!("m{MESH_GEN}"));
    assert_eq!(Class::Glb.prefix(3, None), format!("g{TEX_GEN}z3"));
    // The epoch goes last, on every class.
    assert_eq!(Class::Texture.prefix(d, Some(2)), format!("t{TEX_GEN}e2"));
    assert_eq!(Class::Texture.prefix(9, Some(2)), format!("t{TEX_GEN}z9e2"));
    assert_eq!(Class::Mesh.prefix(9, Some(0)), format!("m{MESH_GEN}e0"));
    for p in [
        Class::Texture.prefix(9, Some(9999)),
        Class::Mesh.prefix(d, Some(7)),
        Class::Glb.prefix(3, Some(1)),
    ] {
        assert!(is_prefix(&p), "{p}");
    }
    assert_eq!(parse_epoch(""), Some(None));
    assert_eq!(parse_epoch(" 3 "), Some(Some(3)));
    assert_eq!(parse_epoch("9999"), Some(Some(9999)));
    for bad in ["10000", "-1", "e2", "2a", "1.5"] {
        assert_eq!(parse_epoch(bad), None, "{bad}");
    }
    // An epoch moves every lane, so a manifest built without one is stale under one.
    let epoch_lanes = BuildOptions {
        epoch: Some(2),
        ..BuildOptions::default()
    }
    .lanes();
    assert!(epoch_lanes.values().all(|p| p.ends_with("e2")));
    let lanes = BuildOptions::default().lanes();
    assert_eq!(
        lanes,
        [
            ("texture", format!("t{TEX_GEN}")),
            ("mesh", format!("m{MESH_GEN}"))
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    );
    assert!(BuildOptions {
        glb_variants: true,
        ..BuildOptions::default()
    }
    .lanes()
    .contains_key("glb"));
    let m = Manifest {
        v: MANIFEST_VERSION,
        lanes: lanes.clone(),
        files: Default::default(),
    };
    assert!(m.is_current(&lanes));
    assert!(!m.is_current(&epoch_lanes));
    // A class turned off since is fine; a class the manifest lacks, or names under another
    // prefix, or an older schema, is not.
    let textures_only = BuildOptions {
        glb_meshopt: false,
        ..BuildOptions::default()
    }
    .lanes();
    assert!(m.is_current(&textures_only));
    assert!(!m.is_current(
        &BuildOptions {
            glb_variants: true,
            ..BuildOptions::default()
        }
        .lanes()
    ));
    assert!(!m.is_current(
        &BuildOptions {
            zstd_level: 9,
            ..BuildOptions::default()
        }
        .lanes()
    ));
    let old = Manifest { v: 1, ..m.clone() };
    assert!(!old.is_current(&lanes));
}

#[test]
fn records_round_trip_and_refuse_other_versions() {
    let rec = Record {
        v: RECORD_VERSION,
        variant: Some(VariantEntry {
            path: "bevy/t1/bafkx_srgb".into(),
            cid: "bafkreicid".into(),
            kind: "texture".into(),
            format: "ktx2-bc1".into(),
            width: Some(64),
            height: Some(32),
            mips: Some(1),
            size: 1234,
        }),
        reason: None,
        br: false,
        uses: Default::default(),
    };
    let bytes = serde_json::to_vec(&rec).unwrap();
    assert_eq!(Record::parse(&bytes), Some(rec));
    let none = Record {
        v: RECORD_VERSION,
        variant: None,
        reason: Some("size-gate".into()),
        br: false,
        uses: [(
            "tex.png".to_string(),
            ImageUse {
                normal: true,
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
    };
    let raw: J = serde_json::from_slice(&serde_json::to_vec(&none).unwrap()).unwrap();
    assert_eq!(raw["reason"], "size-gate");
    assert_eq!(raw["uses"]["tex.png"], json!({"normal": true}));
    assert!(raw.get("variant").is_none() && raw.get("br").is_none());
    assert_eq!(
        Record::parse(&serde_json::to_vec(&none).unwrap()),
        Some(none)
    );
    assert!(Record::parse(br#"{"v":99}"#).is_none());
    assert!(Record::parse(b"nope").is_none());
}

#[test]
fn slot_rules() {
    let u = |srgb, normal, linear| Uses {
        srgb,
        normal,
        linear,
    };
    assert_eq!(u(false, false, false).slot(), Slot::Srgb);
    assert_eq!(u(true, true, true).slot(), Slot::Srgb);
    assert_eq!(u(false, true, false).slot(), Slot::Normal);
    // Normal plus any other use is not normal-only.
    assert_eq!(u(false, true, true).slot(), Slot::Linear);
    assert_eq!(u(false, false, true).slot(), Slot::Linear);
    let mut m = u(false, true, false);
    m.merge(u(true, false, false));
    assert_eq!(m.slot(), Slot::Srgb);
}

#[test]
fn format_selection() {
    assert_eq!(choose_format(Slot::Normal, true), TexFormat::Bc5);
    assert_eq!(choose_format(Slot::Normal, false), TexFormat::Bc5);
    assert_eq!(choose_format(Slot::Srgb, true), TexFormat::Bc1);
    assert_eq!(choose_format(Slot::Linear, true), TexFormat::Bc1);
    assert_eq!(choose_format(Slot::Srgb, false), TexFormat::Bc7);
    assert_eq!(choose_format(Slot::Linear, false), TexFormat::Bc7);
    assert_eq!(TexFormat::Bc7.manifest_name(), "ktx2-bc7");
    assert_eq!(TexFormat::Bc1.manifest_name(), "ktx2-bc1");
    assert_eq!(TexFormat::Bc5.manifest_name(), "ktx2-bc5");
    assert_eq!(TexFormat::Bc1.bc(), BcFormat::Bc1Rgb);
    assert_eq!(TexFormat::Bc5.bc(), BcFormat::Bc5);
    assert_eq!(TexFormat::Bc7.bc(), BcFormat::Bc7);
}

#[test]
fn opaque_png_becomes_bc1_alpha_png_bc7_normal_bc5() {
    let opaque = bc1_exact_png_bytes(32, 32);
    let v = ungated(&opaque, Slot::Srgb);
    assert_eq!(
        (v.format, v.width, v.height, v.mips),
        (TexFormat::Bc1, 32, 32, 1)
    );
    assert!(!v.bc1_rejected);
    // Near-lossless: the encoder's endpoint pick is not exact on every block.
    assert!(v.bc1_psnr_db.unwrap() > 50.0, "{:?}", v.bc1_psnr_db);
    let (h, levels) = ktx(&v.bytes, BcFormat::Bc1Rgb);
    assert_eq!((h.width, h.height, h.levels), (32, 32, 1));
    assert_eq!(levels[0].len(), 64 * 8);
    let src = image::load_from_memory(&opaque)
        .unwrap()
        .to_rgba8()
        .into_raw();
    assert!(psnr_rgb_db(&src, &decode_bc1_rgba(&levels[0], 32, 32)) > 50.0);

    let alpha = noise_alpha_png(32, 16);
    let v = ungated(&alpha, Slot::Srgb);
    assert_eq!(v.format, TexFormat::Bc7);
    assert!(v.bc1_psnr_db.is_none() && !v.bc1_rejected);
    let (h, _) = ktx(&v.bytes, BcFormat::Bc7);
    assert_eq!((h.width, h.height, h.levels), (32, 16, 1));

    let v = ungated(&noise_png_bytes(32, 32), Slot::Normal);
    assert_eq!(v.format, TexFormat::Bc5);
    let (h, _) = ktx(&v.bytes, BcFormat::Bc5);
    assert_eq!((v.mips, h.levels), (1, 1));
}

#[test]
fn opaque_srgb_image_bc1_cannot_hold_becomes_bc7_linear_keeps_bc1() {
    // Noise: four colours per block are nowhere near enough, so the BC1 try is rejected.
    let noise = noise_png_bytes(32, 32);
    let v = ungated(&noise, Slot::Srgb);
    assert_eq!(v.format, TexFormat::Bc7);
    assert!(v.bc1_rejected);
    assert!(
        v.bc1_psnr_db.unwrap() < BC1_MIN_PSNR_DB,
        "{:?}",
        v.bc1_psnr_db
    );
    ktx(&v.bytes, BcFormat::Bc7);
    // The same image as linear (ORM) data keeps BC1: the rule is for colour only.
    let v = ungated(&noise, Slot::Linear);
    assert_eq!(v.format, TexFormat::Bc1);
    assert!(v.bc1_psnr_db.is_none() && !v.bc1_rejected);
    // Both encodes are timed.
    let mut times = StageTimes::default();
    encode_texture_with(&noise, Slot::Srgb, TexOptions::default(), &mut times)
        .unwrap()
        .unwrap();
    assert!(times.bc1_ns > 0 && times.bc1_check_ns > 0 && times.bc7_ns > 0);
}

#[test]
fn bc1_decoder_and_psnr() {
    // Four-colour block: c0 = pure red (0xF800), c1 = pure blue (0x001F), indices 0,1,2,3 on
    // every row.
    let block = [
        0x00,
        0xF8,
        0x1F,
        0x00,
        0b1110_0100,
        0b1110_0100,
        0b1110_0100,
        0b1110_0100,
    ];
    let px = decode_bc1_rgba(&block, 4, 4);
    assert_eq!(&px[0..4], &[255, 0, 0, 255]);
    assert_eq!(&px[4..8], &[0, 0, 255, 255]);
    assert_eq!(&px[8..12], &[170, 0, 85, 255]);
    assert_eq!(&px[12..16], &[85, 0, 170, 255]);
    assert_eq!(&px[16..20], &[255, 0, 0, 255]);
    // Three-colour block (c0 <= c1): index 3 is black.
    let block = [0x1F, 0x00, 0x00, 0xF8, 0b1110_0100, 0, 0, 0];
    let px = decode_bc1_rgba(&block, 4, 4);
    assert_eq!(&px[8..12], &[128, 0, 128, 255]);
    assert_eq!(&px[12..16], &[0, 0, 0, 255]);
    // The decoder agrees with the encoder on a solid 565-representable colour (exact), and
    // PSNR is as defined.
    let c =
        crate::gpu::corelib::dxt1::unpack_565(crate::gpu::corelib::dxt1::pack_565(200, 100, 50));
    let solid: Vec<u8> = [c[0], c[1], c[2], 255].repeat(16);
    let (chain, _) = crate::dxt1_pure::encode_dxt1_mip_chain(&solid, 4, 4, Some(1), false, true);
    let back = decode_bc1_rgba(&chain[..8], 4, 4);
    assert_eq!(back, solid);
    assert_eq!(psnr_rgb_db(&solid, &back), f64::INFINITY);
    let mut off = solid.clone();
    off[0] = off[0].wrapping_add(1); // one channel of one texel off by one over 48 samples
    let expect = 10.0 * (255.0f64 * 255.0 * 48.0).log10();
    assert!((psnr_rgb_db(&solid, &off) - expect).abs() < 1e-9);
}

#[test]
fn size_gate_drops_a_variant_not_smaller_than_its_original() {
    // A solid 16x16 PNG is a few dozen bytes; any KTX2 of it is larger.
    let solid = png_bytes(16, 16, [10, 20, 30, 255]);
    let m = miss(&solid, Slot::Srgb);
    assert_eq!(m.reason, NoVariant::SizeGate);
    assert_eq!(m.gated_format, Some("ktx2-bc1"));
    assert!(m.gated_size.unwrap() as usize >= solid.len());
    let m = miss(&solid, Slot::Normal);
    assert_eq!(
        (m.reason, m.gated_format),
        (NoVariant::SizeGate, Some("ktx2-bc5"))
    );
    let m = miss(&png_bytes(16, 16, [10, 20, 30, 128]), Slot::Srgb);
    assert_eq!(
        (m.reason, m.gated_format),
        (NoVariant::SizeGate, Some("ktx2-bc7"))
    );
}

#[test]
fn encoded_level_equals_the_encoders_single_level() {
    // The container is a lossless wrapper around the encoder's one level: no mip chain.
    let png = noise_alpha_png(16, 16);
    let v = ungated(&png, Slot::Linear);
    assert_eq!(v.format, TexFormat::Bc7);
    let rgba = image::load_from_memory(&png).unwrap().to_rgba8().into_raw();
    let mut bled = rgba;
    crate::alpha_bleed::alpha_bleed_inplace(&mut bled, 16, 16);
    let (chain, mips) = crate::bc7_pure::encode_bc7_mip_chain_with_profile(
        &bled,
        16,
        16,
        Some(1),
        false,
        false,
        false,
        crate::bc7_pure::Bc7Profile::Basic,
    );
    assert_eq!((mips, v.mips), (1, 1));
    let (_, levels) = ktx(&v.bytes, BcFormat::Bc7);
    assert_eq!(levels, vec![chain]);
}

#[test]
fn tiny_unknown_and_oversize_inputs() {
    assert!(encode_texture(&png_bytes(2, 2, [1, 2, 3, 255]), Slot::Srgb)
        .unwrap()
        .is_none());
    assert!(
        encode_texture(&png_bytes(64, 1, [1, 2, 3, 255]), Slot::Srgb)
            .unwrap()
            .is_none()
    );
    assert!(encode_texture(b"ID3 not an image", Slot::Srgb)
        .unwrap()
        .is_none());
    // Sniffed by bytes: PNG magic with a broken body is an error, not "no variant".
    assert!(encode_texture(b"\x89PNG\r\n\x1a\nbroken", Slot::Srgb).is_err());
    let v = ungated(&noise_png_bytes(2048, 16), Slot::Srgb);
    assert_eq!((v.width, v.height, v.mips), (1024, 8, 1));
    // Elongated: the variant would be 1024x2, thinner than one 4x4 BC block.
    assert_eq!(
        miss(&noise_png_bytes(4096, 8), Slot::Srgb).reason,
        NoVariant::Tiny
    );
    // NPOT snaps to powers of two (whole blocks for wgpu).
    let v = ungated(&noise_png_bytes(24, 12), Slot::Srgb);
    assert_eq!((v.width, v.height), (32, 16));
}

#[test]
fn jpeg_is_sniffed_and_encoded() {
    let jpeg = include_bytes!("../../testdata/gradient-16x16.jpg");
    let v = ungated(jpeg, Slot::Srgb);
    assert_eq!((v.width, v.height), (16, 16));
    // Opaque colour: BC1 was tried and kept or rejected by the PSNR rule.
    let psnr = v.bc1_psnr_db.expect("BC1 was tried");
    assert_eq!(v.bc1_rejected, psnr < BC1_MIN_PSNR_DB);
    assert_eq!(
        v.format,
        if v.bc1_rejected {
            TexFormat::Bc7
        } else {
            TexFormat::Bc1
        }
    );
    assert_eq!(ungated(jpeg, Slot::Linear).format, TexFormat::Bc1);
}

#[test]
fn rows_are_top_down() {
    // Top half red, bottom half blue: the first block row of level 0 must be red.
    let mut img = image::RgbaImage::new(8, 8);
    for (_, y, p) in img.enumerate_pixels_mut() {
        *p = if y < 4 {
            image::Rgba([255, 0, 0, 255])
        } else {
            image::Rgba([0, 0, 255, 255])
        };
    }
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let v = ungated(&png, Slot::Linear);
    assert_eq!(v.format, TexFormat::Bc1);
    let (_, levels) = ktx(&v.bytes, BcFormat::Bc1Rgb);
    let l0 = &levels[0];
    let c0 = |block: usize| u16::from_le_bytes([l0[block * 8], l0[block * 8 + 1]]);
    let (red, blue) = (0xF800u16, 0x001Fu16);
    // Solid blocks keep the colour in endpoint 0 (endpoint 1 is one step below it).
    assert_eq!(c0(0), red);
    assert_eq!(c0(1), red);
    assert_eq!(c0(2), blue);
    assert_eq!(c0(3), blue);
}

#[test]
fn uastc_is_a_single_level_pure_transcode_matching_the_client() {
    let v = transcode_uastc_bc7(UASTC_8).expect("8x8 UASTC transcodes");
    assert_eq!(v.path, EncodePath::Ktx2Transcode);
    assert_eq!(
        (v.format, v.width, v.height, v.mips),
        (TexFormat::Bc7, 8, 8, 1)
    );
    // Exactly the one level the client's local path makes: no mips, nothing re-encoded.
    let (h, levels) = ktx(&v.bytes, BcFormat::Bc7);
    assert_eq!((h.width, h.height, h.levels), (8, 8, 1));
    assert_eq!(levels, vec![client_bc7_level(UASTC_8)]);
    // The slot never changes the creator-KTX2 path (no BC5, no BC1).
    for slot in [Slot::Srgb, Slot::Linear, Slot::Normal] {
        match encode_texture(UASTC_8, slot).unwrap() {
            Some(w) => {
                assert_eq!(w.bytes, v.bytes);
                assert_eq!(w.format, TexFormat::Bc7);
            }
            None => assert!(v.bytes.len() > UASTC_8.len()),
        }
    }
}

#[test]
fn uastc_variant_only_when_not_larger_than_the_original() {
    let v = transcode_uastc_bc7(UASTC_8).unwrap();
    let gated = encode_texture(UASTC_8, Slot::Srgb).unwrap();
    assert_eq!(gated.is_some(), v.bytes.len() < UASTC_8.len());
}

#[test]
fn uastc_that_needs_more_than_a_transcode_gets_no_variant() {
    // 6x6 is not whole 4x4 blocks: the explorer would decode and re-encode it, so no variant.
    assert!(transcode_uastc_bc7(UASTC_6).is_none());
    assert!(encode_texture(UASTC_6, Slot::Srgb).unwrap().is_none());
    // Not a UASTC KTX2 the gate takes (here: a BC7 KTX2 of our own).
    let ours = crate::ktx2_write::write_ktx2(
        BcFormat::Bc7,
        4,
        4,
        &[&[0u8; 16][..]],
        crate::ktx2_write::DEFAULT_ZSTD_LEVEL,
    )
    .unwrap();
    assert!(transcode_uastc_bc7(&ours).is_none());
}

fn textured_quad_glb() -> (Vec<u8>, Vec<u8>) {
    // Image 0 (normal map) is embedded in the quad's bin; image 1 is an opaque occlusion map
    // (linear: BC1 without the PSNR rule) appended after it; image 2, the base colour, is
    // external.
    let normal_png = noise_png_bytes(32, 32);
    let (mut bin, mut views, accessors) = quad_bin_and_views(&normal_png);
    emit::pad4(&mut bin, 0);
    let color_png = smooth_png_bytes(64, 64);
    let off = bin.len();
    bin.extend_from_slice(&color_png);
    views
        .as_array_mut()
        .unwrap()
        .push(json!({"buffer": 0, "byteOffset": off, "byteLength": color_png.len()}));
    let glb = mk_glb(
        json!({
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{
                "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
                "indices": 5, "material": 0
            }]}],
            "materials": [{
                "normalTexture": {"index": 0},
                "occlusionTexture": {"index": 1},
                "pbrMetallicRoughness": {"baseColorTexture": {"index": 2}}
            }],
            "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
            "images": [
                {"bufferView": 6, "mimeType": "image/png"},
                {"bufferView": 7, "mimeType": "image/png"},
                {"uri": "textures/color.png"}
            ],
            "bufferViews": views,
            "accessors": accessors
        }),
        &bin,
    );
    (glb, bin)
}

#[test]
fn glb_embedded_images_become_bc_ktx2_by_slot_and_geometry_is_kept() {
    let (glb, _) = textured_quad_glb();
    let v = transform_model(&glb, ".glb", None).unwrap().unwrap();
    assert!(!v.draco_decoded && !v.basisu_resolved);
    assert_eq!(v.images_replaced, vec![0, 1]);
    let (doc, bin) = split(&v.glb);
    image_ktx(&doc, &bin, 0, BcFormat::Bc5);
    image_ktx(&doc, &bin, 1, BcFormat::Bc1Rgb);
    // External URI untouched.
    assert_eq!(doc["images"][2], json!({"uri": "textures/color.png"}));
    // The PNG views are gone (6 geometry views + 2 KTX2 views remain).
    assert_eq!(doc["bufferViews"].as_array().unwrap().len(), 8);
    assert_eq!(doc["buffers"], json!([{"byteLength": bin.len()}]));
    // Geometry bytes identical.
    let (orig_doc, orig_bin) = split(&glb);
    for (acc, elem) in [(0usize, 12usize), (1, 12), (2, 8), (5, 2)] {
        assert_eq!(
            accessor_bytes(&doc, &bin, acc, elem),
            accessor_bytes(&orig_doc, &orig_bin, acc, elem),
            "accessor {acc}"
        );
    }
    assert_eq!(geometry(&v.glb), geometry(&glb));
}

#[test]
fn model_with_nothing_to_change_gets_no_variant() {
    let gltf = json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": "textures/wall.png"}],
        "textures": [{"source": 0}],
        "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}]
    });
    let bytes = serde_json::to_vec(&gltf).unwrap();
    assert!(transform_model(&bytes, ".gltf", None).unwrap().is_none());
    // A tiny embedded image is not worth a variant either.
    let png = png_bytes(2, 2, [1, 2, 3, 255]);
    let glb = mk_glb(
        json!({
            "images": [{"bufferView": 0, "mimeType": "image/png"}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": png.len()}]
        }),
        &png,
    );
    assert!(transform_model(&glb, ".glb", None).unwrap().is_none());
}

#[test]
fn basisu_texture_resolves_to_its_ktx2_source_as_bc7() {
    let glb = mk_glb(
        json!({
            "extensionsUsed": ["KHR_texture_basisu"],
            "extensionsRequired": ["KHR_texture_basisu"],
            "images": [{"bufferView": 0, "mimeType": "image/ktx2"}],
            "textures": [{"extensions": {"KHR_texture_basisu": {"source": 0}}}],
            "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": UASTC_8.len()}]
        }),
        UASTC_8,
    );
    let v = transform_model(&glb, ".glb", None).unwrap().unwrap();
    assert!(v.basisu_resolved);
    let (doc, bin) = split(&v.glb);
    assert_eq!(doc["textures"][0], json!({"source": 0}));
    assert!(doc.get("extensionsRequired").is_none());
    assert!(doc.get("extensionsUsed").is_none());
    let img = &doc["images"][0];
    let view = view_bytes(&doc, &bin, img["bufferView"].as_u64().unwrap() as usize);
    match encode_texture(UASTC_8, Slot::Srgb).unwrap() {
        // Same pure transcode and size gate as a standalone KTX2.
        Some(t) => {
            assert_eq!(v.images_replaced, vec![0]);
            assert_eq!(view, &t.bytes[..]);
            let (_, levels) = image_ktx(&doc, &bin, 0, BcFormat::Bc7);
            assert_eq!(levels, vec![client_bc7_level(UASTC_8)]);
        }
        // No variant: the creator's UASTC bytes stay, under their own MIME type.
        None => {
            assert!(v.images_replaced.is_empty());
            assert_eq!(img["mimeType"], "image/ktx2");
            assert_eq!(view, UASTC_8);
        }
    }
}

#[test]
fn basisu_with_core_fallback_keeps_the_fallback_and_external_ktx2_stays_external() {
    let gltf = json!({
        "asset": {"version": "2.0"},
        "extensionsUsed": ["KHR_texture_basisu"],
        "images": [{"uri": "a.png"}, {"uri": "a.ktx2"}, {"uri": "b.ktx2"}],
        "textures": [
            {"source": 0, "extensions": {"KHR_texture_basisu": {"source": 1}}},
            {"extensions": {"KHR_texture_basisu": {"source": 2}}}
        ]
    });
    let bytes = serde_json::to_vec(&gltf).unwrap();
    let v = transform_model(&bytes, ".gltf", None).unwrap().unwrap();
    let (doc, _) = split(&v.glb);
    assert_eq!(doc["textures"], json!([{"source": 0}, {"source": 2}]));
    assert_eq!(doc["images"][2], json!({"uri": "b.ktx2"}));
    assert!(doc.get("extensionsUsed").is_none());
}

/// Draco needs a real encoder output; no fixture is vendored yet. Point
/// `ABGEN_TEST_DRACO_GLB` at any Draco-compressed GLB with embedded buffers and run with
/// `--ignored`.
#[test]
#[ignore]
fn draco_glb_is_decoded_and_keeps_geometry() {
    let path = std::env::var("ABGEN_TEST_DRACO_GLB").expect("ABGEN_TEST_DRACO_GLB");
    let glb = std::fs::read(path).unwrap();
    let v = transform_model(&glb, ".glb", None).unwrap().unwrap();
    assert!(v.draco_decoded);
    let text = String::from_utf8_lossy(&v.glb);
    assert!(!text.contains("KHR_draco_mesh_compression"));
    assert!(!text.contains("_abgen_from_draco"));
    // `parse` decodes the original's Draco itself, so both sides are plain geometry.
    assert_eq!(geometry(&v.glb), geometry(&glb));
}

/// An `n x n` vertex grid with positions, normals, UVs and u16 indices, plus an external image:
/// big enough that its meshopt variant is smaller than the original.
fn grid_glb(n: u32) -> Vec<u8> {
    let mut bin: Vec<u8> = Vec::new();
    fn put(bin: &mut Vec<u8>, v: &[f32]) {
        for x in v {
            bin.extend_from_slice(&x.to_le_bytes());
        }
    }
    let mut s = 7u32;
    for y in 0..n {
        for x in 0..n {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let h = (s >> 16) as f32 / 65536.0 * 0.3;
            put(&mut bin, &[x as f32 * 0.37, h, y as f32 * 0.37]);
        }
    }
    let pos_len = bin.len();
    let mut s2 = 11u32;
    for _ in 0..n * n {
        s2 = s2.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let a = (s2 >> 8) as f32 / 16_777_216.0 * std::f32::consts::TAU;
        let (sa, ca) = a.sin_cos();
        let z = 0.8f32;
        let r = (1.0 - z * z).sqrt();
        put(&mut bin, &[r * ca, r * sa, z]);
    }
    let nrm_len = bin.len() - pos_len;
    for y in 0..n {
        for x in 0..n {
            put(
                &mut bin,
                &[x as f32 / (n - 1) as f32 * 3.0, y as f32 / (n - 1) as f32],
            );
        }
    }
    let uv_len = bin.len() - pos_len - nrm_len;
    let idx_off = bin.len();
    let mut count = 0;
    for y in 0..n - 1 {
        for x in 0..n - 1 {
            let a = (y * n + x) as u16;
            for i in [
                a,
                a + 1,
                a + n as u16,
                a + 1,
                a + n as u16 + 1,
                a + n as u16,
            ] {
                bin.extend_from_slice(&i.to_le_bytes());
                count += 1;
            }
        }
    }
    let idx_len = bin.len() - idx_off;
    mk_glb(
        json!({
            "scene": 0,
            "scenes": [{"nodes": [0]}],
            "nodes": [{"mesh": 0}],
            "meshes": [{"primitives": [{
                "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
                "indices": 3, "material": 0
            }]}],
            "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
            "textures": [{"source": 0}],
            "images": [{"uri": "grid.png"}],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": pos_len},
                {"buffer": 0, "byteOffset": pos_len, "byteLength": nrm_len},
                {"buffer": 0, "byteOffset": pos_len + nrm_len, "byteLength": uv_len},
                {"buffer": 0, "byteOffset": idx_off, "byteLength": idx_len}
            ],
            "accessors": [
                {"bufferView": 0, "componentType": 5126, "count": n * n, "type": "VEC3",
                 "min": [0.0, 0.0, 0.0], "max": [(n - 1) as f32 * 0.37, 0.3, (n - 1) as f32 * 0.37]},
                {"bufferView": 1, "componentType": 5126, "count": n * n, "type": "VEC3"},
                {"bufferView": 2, "componentType": 5126, "count": n * n, "type": "VEC2"},
                {"bufferView": 3, "componentType": 5123, "count": count, "type": "SCALAR"}
            ]
        }),
        &bin,
    )
}

#[test]
fn meshopt_gltf_size_gate_counts_its_external_bin() {
    use super::meshopt;
    let (mut doc, bin) = split(&grid_glb(24));
    doc["buffers"] = json!([{"uri": "grid.bin", "byteLength": bin.len()}]);
    let gltf = serde_json::to_vec(&doc).unwrap();
    let bin2 = bin.clone();
    let resolve = move |uri: &str| (uri == "grid.bin").then(|| bin2.clone());
    let v = meshopt::compress_model(&gltf, ".gltf", Some(&resolve))
        .unwrap()
        .unwrap();
    assert_eq!(v.source_size, (gltf.len() + bin.len()) as u64);
    // Larger than the JSON alone, smaller than what the client loads for the original.
    assert!(v.glb.len() > gltf.len());
    assert!((v.glb.len() as u64) < v.source_size);
}

#[test]
fn meshopt_refuses_a_second_buffer_without_uri() {
    use super::meshopt;
    let grid = grid_glb(8);
    let (mut doc, bin) = split(&grid);
    let one = json!({"byteLength": bin.len()});
    doc["buffers"] = json!([one.clone(), one]);
    let glb = emit::glb_frame(serde_json::to_vec(&doc).unwrap(), &bin);
    assert!(meshopt::compress_model(&glb, ".glb", None).is_err());
    // The standard single-BIN GLB is unaffected.
    assert!(meshopt::compress_model(&grid, ".glb", None).is_ok());
}

#[test]
fn entity_build_adds_glb_meshopt_variants_with_br_sidecars() {
    use super::meshopt::{self, read_accessor_f32, read_indices};
    let grid = grid_glb(24);
    let (quad, _) = textured_quad_glb();
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkgrid", grid.clone());
    blobs.insert("bafkquad", quad);
    blobs.insert("bafkgridpng", smooth_png_bytes(64, 64));
    let scene = scene_of(
        "bafkentitym",
        &[
            ("models/grid.glb", "bafkgrid"),
            ("models/grid.png", "bafkgridpng"),
            ("models/quad.glb", "bafkquad"),
        ],
    );
    let fetch = |h: &str| blobs.get(h).cloned();
    let probe = |_: &str| None;
    let lane = Lane {
        fetch: &fetch,
        probe: &probe,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    let out = std::env::temp_dir().join(format!("abgen-bevy-meshopt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let built = build_entity_variants(
        &scene,
        &lane,
        &out,
        BuildOptions {
            precompress_br: true,
            ..BuildOptions::default()
        },
    )
    .unwrap();
    let entry = &built.entries["bafkgrid"][0];
    assert_eq!(built.manifest.files["bafkgrid"], vec![entry.path.clone()]);
    assert_eq!(
        (entry.kind.as_str(), entry.format.as_str()),
        ("model", "glb-meshopt")
    );
    assert!(entry.width.is_none() && entry.mips.is_none());
    assert_eq!(entry.path, format!("bevy/m{MESH_GEN}/bafkgrid"));
    let glb = std::fs::read(key_path(&out, &entry.path).unwrap()).unwrap();
    assert_eq!(cid_v1_raw_sha256(&glb), entry.cid);
    assert_eq!(glb.len() as u64, entry.size);
    assert!(glb.len() < grid.len());
    // The sidecar holds the Brotli of the plain bytes; bytes, sidecar and record are listed
    // for upload in that order, and the record says the sidecar exists and how the model
    // samples its image.
    let br_path = meshopt::br_sidecar(&key_path(&out, &entry.path).unwrap());
    let br = std::fs::read(&br_path).unwrap();
    assert_eq!(meshopt::brotli_decompress(&br).unwrap(), glb);
    let grid_uploads: Vec<&Upload> = built
        .uploads
        .iter()
        .filter(|u| u.key.starts_with(&entry.path))
        .collect();
    assert_eq!(
        grid_uploads
            .iter()
            .map(|u| (u.key.as_str(), u.record))
            .collect::<Vec<_>>(),
        vec![
            (entry.path.as_str(), false),
            (&*format!("{}.br", entry.path), false),
            (&*format!("{}.json", entry.path), true),
        ]
    );
    assert_eq!(grid_uploads[1].path, br_path);
    let rec = Record::parse(&std::fs::read(&grid_uploads[2].path).unwrap()).unwrap();
    assert_eq!(rec.variant.as_ref(), Some(entry));
    assert!(rec.br);
    assert_eq!(
        rec.uses.get("grid.png"),
        Some(&ImageUse {
            srgb: true,
            ..Default::default()
        })
    );
    // Geometry round-trips; the external image URI is untouched.
    let (o, ob) = meshopt::load_merged(&grid, None).unwrap();
    let (v, vb) = meshopt::load_merged(&glb, None).unwrap();
    assert_eq!(v.images[0].uri.as_deref(), Some("grid.png"));
    // Same triangles, same winding; the index codec may rotate a triangle's vertices.
    let (ia, ib) = (
        read_indices(&o, &ob, 3).unwrap(),
        read_indices(&v, &vb, 3).unwrap(),
    );
    assert!(meshopt::same_triangles(&ia, &ib).is_ok());
    let (a, b) = (
        read_accessor_f32(&o, &ob, 0).unwrap(),
        read_accessor_f32(&v, &vb, 0).unwrap(),
    );
    for (ra, rb) in a.iter().zip(&b) {
        for (x, y) in ra.iter().zip(rb) {
            assert!((x - y).abs() <= 1e-3, "{x} vs {y}");
        }
    }
    // Stats: one row per model, totals, stage times.
    let st = &built.stats;
    assert_eq!(st.glb_meshopt.considered, 2);
    assert_eq!(st.glb_meshopt.variants + st.glb_meshopt.size_gated.count, 2);
    assert_eq!(st.glb_meshopt.variant_bytes, glb.len() as u64);
    assert_eq!(st.glb_meshopt.br_bytes, br.len() as u64);
    assert!(st.stages.meshopt_ns > 0 && st.stages.brotli_ns > 0);
    let row = st
        .model_files
        .iter()
        .find(|r| r.hash == "bafkgrid")
        .unwrap();
    assert_eq!(
        (row.format, row.br_size),
        (Some("glb-meshopt"), Some(br.len() as u64))
    );
    assert!(st.summary_json().get("model_files").is_none());
    assert_eq!((st.reused_models, st.reused_textures), (0, 0));

    // A second build into the same root reuses everything from the records: no model is
    // fetched, nothing is encoded, nothing is listed for upload, the manifest is identical.
    let fetches = std::sync::atomic::AtomicUsize::new(0);
    let counting = |h: &str| {
        fetches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        blobs.get(h).cloned()
    };
    let lane2 = Lane {
        fetch: &counting,
        probe: &probe,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    let again = build_entity_variants(
        &scene,
        &lane2,
        &out,
        BuildOptions {
            precompress_br: true,
            ..BuildOptions::default()
        },
    )
    .unwrap();
    assert_eq!(again.manifest_bytes, built.manifest_bytes);
    assert_eq!(fetches.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(again.uploads.is_empty());
    assert_eq!(again.stats.reused_models, 2);
    assert_eq!(again.stats.reused_textures, 1);
    assert_eq!(again.stats.unique_variants, 0);
    assert!(again.stats.stages.meshopt_ns == 0 && again.stats.stages.probe_ns > 0);
    let row = again
        .stats
        .model_files
        .iter()
        .find(|r| r.hash == "bafkgrid")
        .unwrap();
    assert!(row.reused && row.variant_path.as_deref() == Some(entry.path.as_str()));
    // The size-gated model's record is reused too: its reason survives the round trip.
    let gated = again
        .stats
        .model_files
        .iter()
        .find(|r| r.hash == "bafkquad")
        .unwrap();
    assert!(gated.reused);
    assert_eq!(
        gated.reason,
        st.model_files
            .iter()
            .find(|r| r.hash == "bafkquad")
            .unwrap()
            .reason
    );

    // The lane's probe answers for keys that are not on disk: a record from elsewhere fills
    // the row without a fetch, and a negative record stops the encode.
    let out5 =
        std::env::temp_dir().join(format!("abgen-bevy-meshopt-probe-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out5);
    let remote_entry = entry.clone();
    let remote = move |key: &str| -> Option<Record> {
        if key == remote_entry.path {
            Some(Record {
                v: RECORD_VERSION,
                variant: Some(remote_entry.clone()),
                reason: None,
                br: true,
                uses: [(
                    "grid.png".to_string(),
                    ImageUse {
                        normal: true,
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
            })
        } else if key.ends_with("/bafkgridpng_normal") {
            Some(Record {
                v: RECORD_VERSION,
                variant: None,
                reason: Some("size-gate".into()),
                br: false,
                uses: Default::default(),
            })
        } else {
            None
        }
    };
    let lane5 = Lane {
        fetch: &counting,
        probe: &remote,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    fetches.store(0, std::sync::atomic::Ordering::Relaxed);
    let probed = build_entity_variants(&scene, &lane5, &out5, BuildOptions::default()).unwrap();
    assert_eq!(probed.entries["bafkgrid"], vec![entry.clone()]);
    // The remote record says the image is a normal map, so the texture key is the normal
    // slot, whose negative record was honoured: no entry, no fetch of the image.
    assert!(!probed.entries.contains_key("bafkgridpng"));
    let png = probed
        .stats
        .files
        .iter()
        .find(|f| f.hash == "bafkgridpng")
        .unwrap();
    assert!(png.reused && png.reason == Some(NoVariant::SizeGate));
    // Only the quad model (no record anywhere) was fetched.
    assert_eq!(fetches.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert!(probed.uploads.iter().all(|u| u.key.contains("/bafkquad")));
    let _ = std::fs::remove_dir_all(&out5);

    // Off: no model entries at all.
    let out2 = std::env::temp_dir().join(format!("abgen-bevy-meshopt-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out2);
    let built2 = build_entity_variants(
        &scene,
        &lane,
        &out2,
        BuildOptions {
            glb_meshopt: false,
            ..BuildOptions::default()
        },
    )
    .unwrap();
    assert!(built2
        .entries
        .values()
        .flatten()
        .all(|e| e.kind == "texture"));
    assert!(built2.stats.model_files.is_empty());
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_dir_all(&out2);
}

/// A variant first built without a `.br` sidecar gets one once precompression is on: the
/// sidecar is written and listed for upload before the replaced record (`br: true`), and
/// nothing is fetched or re-encoded.
#[test]
fn reused_meshopt_variant_without_br_gets_a_sidecar_without_a_fetch() {
    use super::meshopt;
    let grid = grid_glb(24);
    let scene = scene_of("bafkentitybr", &[("models/grid.glb", "bafkgrid")]);
    let fetches = std::sync::atomic::AtomicUsize::new(0);
    let fetch = |h: &str| {
        fetches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        (h == "bafkgrid").then(|| grid.clone())
    };
    let probe = |_: &str| None;
    let lane = Lane {
        fetch: &fetch,
        probe: &probe,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    let out = std::env::temp_dir().join(format!("abgen-bevy-backfill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let plain = BuildOptions {
        precompress_br: false,
        ..BuildOptions::default()
    };
    let first = build_entity_variants(&scene, &lane, &out, plain).unwrap();
    let entry = first.entries["bafkgrid"][0].clone();
    let vpath = key_path(&out, &entry.path).unwrap();
    let br_path = meshopt::br_sidecar(&vpath);
    assert!(!br_path.exists());
    let rec_path = key_path(&out, &record_key(&entry.path)).unwrap();
    assert!(
        !Record::parse(&std::fs::read(&rec_path).unwrap())
            .unwrap()
            .br
    );

    fetches.store(0, std::sync::atomic::Ordering::Relaxed);
    let again = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert_eq!(fetches.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(again.entries["bafkgrid"][0], entry);
    assert_eq!(again.stats.unique_variants, 0);
    let plain_bytes = std::fs::read(&vpath).unwrap();
    assert_eq!(
        meshopt::brotli_decompress(&std::fs::read(&br_path).unwrap()).unwrap(),
        plain_bytes
    );
    assert!(
        Record::parse(&std::fs::read(&rec_path).unwrap())
            .unwrap()
            .br
    );
    assert_eq!(
        again
            .uploads
            .iter()
            .map(|u| (u.key.as_str(), u.owner.as_str(), u.record))
            .collect::<Vec<_>>(),
        vec![
            (&*format!("{}.br", entry.path), entry.path.as_str(), false),
            (&*format!("{}.json", entry.path), entry.path.as_str(), true),
        ]
    );
    // Once it has its sidecar, the next build has nothing to add.
    let third = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert!(third.uploads.is_empty());
    let _ = std::fs::remove_dir_all(&out);
}

/// A record whose variant is gone from disk is a miss and is deleted, so the rebuild
/// re-encodes that key alone; every other key is reused.
#[test]
fn record_without_its_variant_is_rebuilt_alone() {
    let grid = grid_glb(24);
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkgrid", grid);
    // Noise: its BC variant is always smaller than the PNG, so it passes the size gate.
    blobs.insert("bafkgridpng", noise_png_bytes(64, 64));
    let scene = scene_of(
        "bafkentityev",
        &[
            ("models/grid.glb", "bafkgrid"),
            ("models/grid.png", "bafkgridpng"),
        ],
    );
    let fetches = std::sync::Mutex::new(Vec::<String>::new());
    let fetch = |h: &str| {
        fetches.lock().unwrap().push(h.to_string());
        blobs.get(h).cloned()
    };
    let probe = |_: &str| None;
    let lane = Lane {
        fetch: &fetch,
        probe: &probe,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    let out = std::env::temp_dir().join(format!("abgen-bevy-evict-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let first = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    let tex = first.entries["bafkgridpng"][0].clone();
    let tex_path = key_path(&out, &tex.path).unwrap();
    let tex_rec = key_path(&out, &record_key(&tex.path)).unwrap();
    std::fs::remove_file(&tex_path).unwrap();
    fetches.lock().unwrap().clear();
    let again = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert_eq!(*fetches.lock().unwrap(), vec!["bafkgridpng".to_string()]);
    assert_eq!(again.manifest_bytes, first.manifest_bytes);
    assert!(tex_path.is_file() && tex_rec.is_file());
    assert!(again.uploads.iter().all(|u| u.owner == tex.path));
    let _ = std::fs::remove_dir_all(&out);
}

fn scene_of(entity: &str, content: &[(&str, &str)]) -> Scene {
    Scene {
        entity_id: entity.into(),
        entity_type: "scene".into(),
        pointers: vec!["0,0".into()],
        content: content
            .iter()
            .map(|(f, h)| ContentEntry {
                file: (*f).into(),
                hash: (*h).into(),
            })
            .collect(),
        metadata: json!({}),
    }
}

#[test]
fn entity_build_writes_cid_addressed_variants_and_a_deterministic_manifest() {
    let (embedded_glb, _) = textured_quad_glb();
    // A model whose images are external: no model variant, but one image is normal-only and
    // the other linear (occlusion).
    let ext_glb = {
        let gltf = json!({
            "asset": {"version": "2.0"},
            "images": [{"uri": "../tex/normal.png"}, {"uri": "../color.png"}],
            "textures": [{"source": 0}, {"source": 1}],
            "materials": [{"normalTexture": {"index": 0}, "occlusionTexture": {"index": 1}}]
        });
        serde_json::to_vec(&gltf).unwrap()
    };
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkcolor", smooth_png_bytes(64, 64));
    blobs.insert("bafknormal", noise_png_bytes(32, 32));
    blobs.insert("bafktiny", png_bytes(2, 2, [1, 1, 1, 255]));
    blobs.insert("bafkembed", embedded_glb);
    blobs.insert("bafkext", ext_glb);
    blobs.insert("bafkjs", b"console.log(1)".to_vec());
    blobs.insert("bafkmp3", b"ID3audio".to_vec());
    blobs.insert("bafkktx", UASTC_8.to_vec());
    let scene = scene_of(
        "bafkentity",
        &[
            ("color.png", "bafkcolor"),
            ("tex/normal.png", "bafknormal"),
            ("tiny.png", "bafktiny"),
            ("models/embed.glb", "bafkembed"),
            ("models/ext.gltf", "bafkext"),
            ("bin/index.js", "bafkjs"),
            ("bin/index.js.map", "bafkjs"),
            ("boom.mp3", "bafkmp3"),
            ("creator.ktx2", "bafkktx"),
            ("missing.png", "bafkmissing"),
        ],
    );
    let fetch = |h: &str| blobs.get(h).cloned();
    let probe = |_: &str| None;
    let lane = Lane {
        fetch: &fetch,
        probe: &probe,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    let run = |tag: &str, opts: BuildOptions| {
        let out =
            std::env::temp_dir().join(format!("abgen-bevy-variants-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let built = build_entity_variants(&scene, &lane, &out, opts).unwrap();
        (out, built)
    };
    // Default: texture and glb-meshopt variants with `.br` sidecars (GLB variants with KTX2
    // inside are opt-in).
    assert_eq!(
        BuildOptions::default(),
        BuildOptions {
            glb_variants: false,
            glb_meshopt: true,
            precompress_br: true,
            zstd_level: crate::ktx2_write::DEFAULT_ZSTD_LEVEL,
            epoch: None,
        }
    );
    // These runs look at textures only.
    let textures_only_opts = BuildOptions {
        glb_meshopt: false,
        ..BuildOptions::default()
    };
    let (out, built) = run("a", textures_only_opts);
    let manifest = &built.manifest;
    assert_eq!(manifest.v, 3);
    assert_eq!(manifest.lanes, textures_only_opts.lanes());
    // The manifest is the entries' paths and nothing else.
    let m = &built.entries;
    assert_eq!(
        manifest.files,
        m.iter()
            .map(|(k, es)| (k.clone(), es.iter().map(|e| e.path.clone()).collect()))
            .collect::<std::collections::BTreeMap<String, Vec<String>>>()
    );
    let ktx_variant = encode_texture(UASTC_8, Slot::Srgb).unwrap();
    let keys: Vec<&str> = m.keys().map(String::as_str).collect();
    let mut expected = vec!["bafkcolor", "bafknormal"];
    if ktx_variant.is_some() {
        expected.insert(1, "bafkktx");
        assert_eq!(m["bafkktx"][0].format, "ktx2-bc7");
    }
    assert_eq!(keys, expected);
    assert_eq!(m["bafkcolor"][0].format, "ktx2-bc1");
    assert_eq!(m["bafknormal"][0].format, "ktx2-bc5");
    // The images embedded in embed.glb get no texture variant of their own.
    assert!(m.values().flatten().all(|e| e.kind == "texture"));
    let tex = &m["bafkcolor"][0];
    assert_eq!(
        (tex.kind.as_str(), tex.width, tex.height, tex.mips),
        ("texture", Some(64), Some(64), Some(1))
    );

    // Every listed variant is stored at its path, named by source and slot, with the CID of
    // its bytes and its size; its record beside it names the same entry.
    for (hash, entries) in m {
        for e in entries {
            let bytes = std::fs::read(key_path(&out, &e.path).unwrap()).unwrap();
            assert_eq!(cid_v1_raw_sha256(&bytes), e.cid);
            assert_eq!(bytes.len() as u64, e.size);
            assert!(e.cid.starts_with("bafkrei"));
            let rec = Record::parse(
                &std::fs::read(key_path(&out, &record_key(&e.path)).unwrap()).unwrap(),
            )
            .unwrap();
            assert_eq!(rec.variant.as_ref(), Some(e));
            assert!(e.path.starts_with(&format!("bevy/t{TEX_GEN}/{hash}_")));
        }
    }
    assert_eq!(
        m["bafkcolor"][0].path,
        format!("bevy/t{TEX_GEN}/bafkcolor_linear")
    );
    assert_eq!(
        m["bafknormal"][0].path,
        format!("bevy/t{TEX_GEN}/bafknormal_normal")
    );
    let written: Vec<&Upload> = built.uploads.iter().filter(|u| !u.record).collect();
    assert_eq!(written.len(), expected.len());
    // The tiny image has a negative record (final), the missing one has none (not final).
    let tiny = Record::parse(
        &std::fs::read(key_path(&out, &format!("bevy/t{TEX_GEN}/bafktiny_srgb.json")).unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!((tiny.variant, tiny.reason.as_deref()), (None, Some("tiny")));
    assert!(
        !key_path(&out, &format!("bevy/t{TEX_GEN}/bafkmissing_srgb.json"))
            .unwrap()
            .exists()
    );

    // The manifest on disk is the returned bytes and parses back to the same value.
    let on_disk = std::fs::read(manifest_path(&out, "bafkentity").unwrap()).unwrap();
    assert_eq!(on_disk, built.manifest_bytes);
    let parsed: Manifest = serde_json::from_slice(&on_disk).unwrap();
    assert_eq!(&parsed, manifest);
    let raw: J = serde_json::from_slice(&on_disk).unwrap();
    assert_eq!(
        raw,
        json!({"v": 3, "lanes": textures_only_opts.lanes(), "files": manifest.files})
    );
    assert_eq!(
        raw["files"]["bafkcolor"][0],
        m["bafkcolor"][0].path.as_str()
    );

    // Stats: one row per standalone image, with the reason for each miss.
    let st = &built.stats;
    let reason = |hash: &str| {
        st.files
            .iter()
            .find(|f| f.hash == hash)
            .and_then(|f| f.reason)
    };
    assert_eq!(st.zstd_level, crate::ktx2_write::DEFAULT_ZSTD_LEVEL);
    assert_eq!(st.textures, 5); // color, normal, tiny, creator.ktx2, missing
    assert_eq!(reason("bafktiny"), Some(super::stats::NoVariant::Tiny));
    assert_eq!(
        reason("bafkmissing"),
        Some(super::stats::NoVariant::Missing)
    );
    assert_eq!(
        reason("bafkktx"),
        ktx_variant.is_none().then_some(NoVariant::SizeGate)
    );
    assert_eq!(st.variants["ktx2-bc1 from png"].count, 1);
    assert_eq!(st.variants["ktx2-bc5 from png"].count, 1);
    assert_eq!(st.bc1_rejected, 0);
    let color = st.files.iter().find(|f| f.hash == "bafkcolor").unwrap();
    // Linear: BC1 without the PSNR check.
    assert!(color.bc1_rejected.is_none() && color.bc1_psnr_db.is_none());
    if ktx_variant.is_none() {
        assert_eq!(st.size_gated["ktx2-bc7"].count, 1);
    }
    assert_eq!(st.unique_variants as usize, written.len());
    assert!(st.stages.bc1_ns > 0 && st.stages.bc5_ns > 0 && st.stages.zstd_ns > 0);
    assert!(st.summary_json().get("files").is_none());

    // Same inputs, same bytes.
    let (out2, built2) = run("b", textures_only_opts);
    assert_eq!(built2.manifest_bytes, built.manifest_bytes);

    // Another zstd level: same pixels, different variant bytes (so different CIDs) under
    // another prefix, same manifest shape.
    let (out4, built4) = run(
        "z3",
        BuildOptions {
            zstd_level: 3,
            ..textures_only_opts
        },
    );
    let m4 = &built4.entries;
    assert_eq!(m4.keys().collect::<Vec<_>>(), m.keys().collect::<Vec<_>>());
    assert_eq!(built4.manifest.lanes["texture"], format!("t{TEX_GEN}z3"));
    for (k, e) in m4 {
        assert!(e[0].path.starts_with(&format!("bevy/t{TEX_GEN}z3/")));
        let a = std::fs::read(key_path(&out4, &e[0].path).unwrap()).unwrap();
        let b = std::fs::read(key_path(&out, &m[k][0].path).unwrap()).unwrap();
        assert_eq!(
            read_levels(&a).unwrap(),
            read_levels(&b).unwrap(),
            "{k}: same blocks"
        );
    }
    let _ = std::fs::remove_dir_all(&out4);

    // Opt-in: the GLB with embedded images gets a model variant (no size fields); the GLB whose
    // only image is external still gets none; the texture entries are unchanged.
    let (out3, built3) = run(
        "g",
        BuildOptions {
            glb_variants: true,
            ..textures_only_opts
        },
    );
    let m3 = &built3.entries;
    let model = &m3["bafkembed"][0];
    assert_eq!(
        (model.kind.as_str(), model.format.as_str()),
        ("model", "glb")
    );
    assert!(model.width.is_none() && model.mips.is_none());
    assert!(!m3.contains_key("bafkext"));
    let raw3: J = serde_json::from_slice(&built3.manifest_bytes).unwrap();
    assert_eq!(raw3["files"]["bafkembed"], json!([model.path]));
    let mut textures_only = m3.clone();
    textures_only.remove("bafkembed");
    assert_eq!(&textures_only, m);
    assert_eq!(model.path, format!("bevy/g{TEX_GEN}/bafkembed"));
    let glb = std::fs::read(key_path(&out3, &model.path).unwrap()).unwrap();
    assert_eq!(cid_v1_raw_sha256(&glb), model.cid);
    // The GLB nothing changes in has a negative record; the `.gltf` with no external buffer
    // keeps its plain name.
    let ext_rec = Record::parse(
        &std::fs::read(key_path(&out3, &format!("bevy/g{TEX_GEN}/bafkext.json")).unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(ext_rec.reason.as_deref(), Some("nothing-to-change"));
    assert!(ext_rec.uses.contains_key("../tex/normal.png"));

    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_dir_all(&out2);
    let _ = std::fs::remove_dir_all(&out3);
}

/// The store-backed build skips an entity only for a manifest that parses and was built from
/// the prefixes it would use now.
#[test]
fn only_a_current_manifest_skips_the_store_backed_build() {
    let lanes = BuildOptions::default().lanes();
    let current = Manifest {
        v: MANIFEST_VERSION,
        lanes: lanes.clone(),
        files: Default::default(),
    };
    assert!(super::manifest_bytes_current(
        &serde_json::to_vec(&current).unwrap(),
        &lanes
    ));
    let old_schema = Manifest {
        v: MANIFEST_VERSION - 1,
        ..current.clone()
    };
    assert!(!super::manifest_bytes_current(
        &serde_json::to_vec(&old_schema).unwrap(),
        &lanes
    ));
    let other_prefixes = BuildOptions {
        epoch: Some(7),
        ..BuildOptions::default()
    }
    .lanes();
    assert!(!super::manifest_bytes_current(
        &serde_json::to_vec(&current).unwrap(),
        &other_prefixes
    ));
    assert!(!super::manifest_bytes_current(b"not json", &lanes));
}

/// A build that writes to a shared store does not count records left in its output directory
/// (an earlier run may have died before uploading): with `local_records` off every key is
/// asked of the probe, and a miss there is rebuilt and listed for upload again.
#[test]
fn local_records_do_not_count_for_a_store_backed_build() {
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkgridpng", noise_png_bytes(64, 64));
    let scene = scene_of("bafkentitylr", &[("tex/grid.png", "bafkgridpng")]);
    let fetches = std::sync::atomic::AtomicUsize::new(0);
    let fetch = |h: &str| {
        fetches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        blobs.get(h).cloned()
    };
    let probe = |_: &str| None;
    let out = std::env::temp_dir().join(format!("abgen-bevy-localrec-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let stored = |_: &str| -> Option<Vec<u8>> { None };
    let lane = |local_records: bool| Lane {
        fetch: &fetch,
        probe: &probe,
        stored: &stored,
        local_records,
    };
    let first = build_entity_variants(&scene, &lane(false), &out, BuildOptions::default()).unwrap();
    assert!(!first.uploads.is_empty());
    // The output directory now holds the variant and its record, but the "store" (the probe)
    // has nothing: a store-backed build builds and lists it again.
    fetches.store(0, std::sync::atomic::Ordering::Relaxed);
    let again = build_entity_variants(&scene, &lane(false), &out, BuildOptions::default()).unwrap();
    assert_eq!(fetches.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(
        again.uploads.iter().map(|u| &u.key).collect::<Vec<_>>(),
        first.uploads.iter().map(|u| &u.key).collect::<Vec<_>>()
    );
    // A build whose directory is the store of record reuses it.
    fetches.store(0, std::sync::atomic::Ordering::Relaxed);
    let local = build_entity_variants(&scene, &lane(true), &out, BuildOptions::default()).unwrap();
    assert_eq!(fetches.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(local.uploads.is_empty());
    let _ = std::fs::remove_dir_all(&out);
}
