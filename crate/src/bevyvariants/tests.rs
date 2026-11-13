use super::model::{transform_model, transform_model_with, NoGlbVariant, KTX2_MIME};
use super::stats::{NoVariant, StageTimes};
use super::texture::{
    choose_format, decode_bc1_rgba, encode_texture, encode_texture_with, psnr_rgb_db,
    transcode_uastc_bc7, EncodePath, Slot, TexFormat, TexOptions, TextureVariant, Uses,
    BC1_MIN_PSNR_DB,
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

/// The encode, which must give a variant.
fn ungated(bytes: &[u8], slot: Slot) -> TextureVariant {
    match encode_texture_with(
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

/// `g` variants carry the texture encoders' bytes, so `GLB_GEN` must move with `TEX_GEN`.
/// Update both numbers here together: bumping one alone fails this test.
#[test]
fn glb_generation_moves_with_the_texture_generation() {
    assert_eq!(
        (TEX_GEN, GLB_GEN),
        (4, 4),
        "bump GLB_GEN whenever TEX_GEN is bumped, then update this pin"
    );
}

#[test]
fn lane_prefixes_follow_generations_and_the_zstd_level() {
    let d = crate::ktx2_write::DEFAULT_ZSTD_LEVEL;
    assert_eq!(Class::Texture.prefix(d, None), format!("t{TEX_GEN}"));
    assert_eq!(Class::Texture.prefix(9, None), format!("t{TEX_GEN}z9"));
    assert_eq!(Class::Mesh.prefix(9, None), format!("m{MESH_GEN}"));
    assert_eq!(Class::Glb.prefix(3, None), format!("g{GLB_GEN}z3"));
    // Off the `g3` the class had when it was named after the texture generation.
    assert_ne!(Class::Glb.prefix(d, None), "g3");
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
            ("mesh", format!("m{MESH_GEN}")),
            ("glb", format!("g{GLB_GEN}"))
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    );
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
        models: false,
        ..BuildOptions::default()
    }
    .lanes();
    assert!(m.is_current(&textures_only));
    // A manifest built without GLB variants (before they were on by default) is stale.
    let no_glb = Manifest {
        lanes: BuildOptions {
            glb_variants: false,
            ..BuildOptions::default()
        }
        .lanes(),
        ..m.clone()
    };
    assert!(!no_glb.is_current(&lanes));
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
        reason: Some("tiny".into()),
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
    assert_eq!(raw["reason"], "tiny");
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

/// No size gate: a variant larger than its original is still the variant, because the
/// explorer would otherwise decode the PNG to RGBA8 and convert it itself.
#[test]
fn a_variant_larger_than_its_original_is_kept() {
    // A solid 16x16 PNG is a few dozen bytes; any KTX2 of it is larger.
    let solid = png_bytes(16, 16, [10, 20, 30, 255]);
    for (bytes, slot, format) in [
        (solid.clone(), Slot::Srgb, TexFormat::Bc1),
        (solid.clone(), Slot::Normal, TexFormat::Bc5),
        (
            png_bytes(16, 16, [10, 20, 30, 128]),
            Slot::Srgb,
            TexFormat::Bc7,
        ),
    ] {
        let v = encode_texture(&bytes, slot).unwrap().expect("a variant");
        assert_eq!(v.format, format);
        assert!(v.bytes.len() >= bytes.len());
    }
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
fn uastc_variant_is_its_transcode_whatever_its_size() {
    let v = transcode_uastc_bc7(UASTC_8).unwrap();
    let e = encode_texture(UASTC_8, Slot::Srgb).unwrap().unwrap();
    assert_eq!(e.bytes, v.bytes);
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
    // Same pure transcode as a standalone KTX2, without the size gate.
    let t = ungated(UASTC_8, Slot::Srgb);
    assert_eq!(v.images_replaced, vec![0]);
    assert_eq!(view, &t.bytes[..]);
    let (_, levels) = image_ktx(&doc, &bin, 0, BcFormat::Bc7);
    assert_eq!(levels, vec![client_bc7_level(UASTC_8)]);
}

/// An embedded KTX2 that does not transcode would leave a KTX2 the explorer cannot read inside
/// the variant: the GLB gets none, with a reason, and the client loads the original.
#[test]
fn glb_with_an_embedded_ktx2_that_does_not_transcode_gets_no_variant() {
    let mut junk = crate::ktx2::KTX2_MAGIC.to_vec();
    junk.extend_from_slice(&[0u8; 100]);
    let png = smooth_png_bytes(32, 32);
    let mut bin = png.clone();
    emit::pad4(&mut bin, 0);
    let off = bin.len();
    bin.extend_from_slice(&junk);
    let glb = mk_glb(
        json!({
            "images": [
                {"bufferView": 0, "mimeType": "image/png"},
                {"bufferView": 1, "mimeType": "image/ktx2"}
            ],
            "textures": [{"source": 0}, {"source": 1}],
            "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": png.len()},
                {"buffer": 0, "byteOffset": off, "byteLength": junk.len()}
            ]
        }),
        &bin,
    );
    let o = transform_model_with(&glb, ".glb", None, TexOptions::default()).unwrap();
    assert!(o.variant.is_none());
    assert_eq!(o.reason, Some(NoGlbVariant::EmbeddedKtx2Kept));
    assert_eq!(o.images.len(), 2);
    // The PNG beside it converts (BC1 or BC7 by the PSNR rule); the KTX2 does not.
    assert!(o.images[0].format.is_some());
    assert_eq!(o.images[1].reason, Some(NoVariant::Ktx2HeaderGate));
}

/// No size gate inside a GLB either: an image whose BC KTX2 is larger than its PNG/JPEG (here a
/// solid PNG; in scenes mostly JPEGs) is still replaced, because what it saves is GPU memory. The outcome rows say what each image became and its GPU bytes before and after.
#[test]
fn embedded_image_is_replaced_even_when_its_ktx2_is_larger() {
    let png = png_bytes(16, 16, [10, 20, 30, 255]);
    let glb = mk_glb(
        json!({
            "images": [{"bufferView": 0, "mimeType": "image/png"}],
            "textures": [{"source": 0}],
            "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": png.len()}]
        }),
        &png,
    );
    let o = transform_model_with(&glb, ".glb", None, TexOptions::default()).unwrap();
    assert!(o.reason.is_none());
    let v = o.variant.unwrap();
    assert_eq!(v.images_replaced, vec![0]);
    let t = ungated(&png, Slot::Srgb);
    let (doc, bin) = split(&v.glb);
    assert_eq!(doc["images"][0]["mimeType"], KTX2_MIME);
    image_ktx(&doc, &bin, 0, t.format.bc());
    let row = &o.images[0];
    assert_eq!(row.source, super::stats::SourceKind::Png);
    assert_eq!(row.format, Some(t.format.manifest_name()));
    assert_eq!(row.original_size, png.len() as u64);
    assert_eq!(row.variant_size, Some(t.bytes.len() as u64));
    assert!(t.bytes.len() >= png.len());
    assert_eq!(row.original_gpu_bytes, Some(16 * 16 * 4));
    assert_eq!(
        row.variant_gpu_bytes,
        Some(crate::ktx2_write::level_bytes(t.format.bc(), 16, 16) as u64)
    );
}

/// A basisu texture with a core PNG/JPEG `source` keeps the fallback; the KTX2 no texture
/// samples any more is dropped (even one that would not transcode), and sources are remapped.
#[test]
fn basisu_fallback_wins_and_the_unused_ktx2_is_dropped() {
    let mut junk = crate::ktx2::KTX2_MAGIC.to_vec();
    junk.extend_from_slice(&[0u8; 100]);
    let png = smooth_png_bytes(32, 32);
    let mut bin = junk.clone();
    emit::pad4(&mut bin, 0);
    let off = bin.len();
    bin.extend_from_slice(&png);
    let glb = mk_glb(
        json!({
            "extensionsUsed": ["KHR_texture_basisu"],
            "images": [
                {"bufferView": 0, "mimeType": "image/ktx2"},
                {"bufferView": 1, "mimeType": "image/png"}
            ],
            "textures": [{"source": 1, "extensions": {"KHR_texture_basisu": {"source": 0}}}],
            "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
            "bufferViews": [
                {"buffer": 0, "byteOffset": 0, "byteLength": junk.len()},
                {"buffer": 0, "byteOffset": off, "byteLength": png.len()}
            ]
        }),
        &bin,
    );
    let o = transform_model_with(&glb, ".glb", None, TexOptions::default()).unwrap();
    assert!(o.reason.is_none(), "{:?}", o.reason);
    let v = o.variant.unwrap();
    let (doc, bin) = split(&v.glb);
    assert_eq!(doc["textures"], json!([{"source": 0}]));
    assert_eq!(doc["images"].as_array().unwrap().len(), 1);
    assert_eq!(doc["images"][0]["mimeType"], KTX2_MIME);
    assert!(doc.get("extensionsUsed").is_none());
    assert_eq!(v.images_replaced, vec![0]);
    // The junk KTX2's bytes are not in the variant: one view, the PNG's BC KTX2.
    assert_eq!(doc["bufferViews"].as_array().unwrap().len(), 1);
    assert!(!bin.windows(junk.len()).any(|w| w == junk.as_slice()));
}

/// A basisu-only texture whose KTX2 is an external file would become a plain core KTX2 image
/// the explorer may not read: no variant.
#[test]
fn basisu_with_an_external_ktx2_gets_no_variant() {
    for img in [
        json!({"uri": "b.ktx2"}),
        json!({"uri": "b?v=1", "mimeType": "image/ktx2"}),
    ] {
        let gltf = json!({
            "asset": {"version": "2.0"},
            "extensionsUsed": ["KHR_texture_basisu"],
            "extensionsRequired": ["KHR_texture_basisu"],
            "images": [img],
            "textures": [{"extensions": {"KHR_texture_basisu": {"source": 0}}}]
        });
        let bytes = serde_json::to_vec(&gltf).unwrap();
        let o = transform_model_with(&bytes, ".gltf", None, TexOptions::default()).unwrap();
        assert!(o.variant.is_none());
        assert_eq!(o.reason, Some(NoGlbVariant::ExternalKtx2));
    }
    // An external PNG stays external and needs no variant of its own.
    let gltf = json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": "a.png"}],
        "textures": [{"source": 0}]
    });
    let bytes = serde_json::to_vec(&gltf).unwrap();
    let o = transform_model_with(&bytes, ".gltf", None, TexOptions::default()).unwrap();
    assert_eq!(o.reason, Some(NoGlbVariant::NothingToChange));
}

/// A `.gltf` whose image is a data URI becomes a GLB with the image as a BC KTX2.
#[test]
fn gltf_with_a_data_uri_image_becomes_a_glb_with_bc() {
    fn b64(bytes: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in bytes.chunks(3) {
            let n = (u32::from(c[0]) << 16)
                | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
                | u32::from(*c.get(2).unwrap_or(&0));
            for k in 0..4 {
                if k <= c.len() {
                    out.push(A[((n >> (18 - 6 * k)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }
    let png = smooth_png_bytes(32, 32);
    let uri = format!("data:image/png;base64,{}", b64(&png));
    let gltf = json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": uri}],
        "textures": [{"source": 0}],
        "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}]
    });
    let bytes = serde_json::to_vec(&gltf).unwrap();
    let o = transform_model_with(&bytes, ".gltf", None, TexOptions::default()).unwrap();
    let v = o.variant.unwrap();
    assert_eq!(&v.glb[..4], b"glTF");
    let (doc, bin) = split(&v.glb);
    assert!(doc["images"][0].get("uri").is_none());
    let t = ungated(&png, Slot::Srgb);
    image_ktx(&doc, &bin, 0, t.format.bc());
    assert_eq!(o.images.len(), 1);
}

/// Many `images` with the same bytes and slot are encoded once and share one KTX2 view.
#[test]
fn identical_embedded_images_are_encoded_and_stored_once() {
    let png = smooth_png_bytes(32, 32);
    let n = 50;
    let glb = mk_glb(
        json!({
            "images": (0..n).map(|_| json!({"bufferView": 0, "mimeType": "image/png"})).collect::<Vec<_>>(),
            "textures": (0..n).map(|i| json!({"source": i})).collect::<Vec<_>>(),
            "materials": (0..n).map(|i| json!({"pbrMetallicRoughness": {"baseColorTexture": {"index": i}}})).collect::<Vec<_>>(),
            "bufferViews": [{"buffer": 0, "byteOffset": 0, "byteLength": png.len()}]
        }),
        &png,
    );
    let o = transform_model_with(&glb, ".glb", None, TexOptions::default()).unwrap();
    assert_eq!(o.images.len(), 1);
    let v = o.variant.unwrap();
    assert_eq!(v.images_replaced, (0..n).collect::<Vec<_>>());
    let (doc, _) = split(&v.glb);
    assert_eq!(doc["bufferViews"].as_array().unwrap().len(), 1);
    assert!(doc["images"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["bufferView"] == 0));
}

/// A model over the distinct-image limit gets no variant, before anything is encoded.
#[test]
fn model_over_the_embedded_image_limit_gets_no_variant() {
    use super::model::MAX_EMBEDDED_SOURCES;
    let n = MAX_EMBEDDED_SOURCES + 1;
    let mut bin = Vec::new();
    let mut views = Vec::new();
    for i in 0..n {
        emit::pad4(&mut bin, 0);
        let png = png_bytes(4, 4, [(i % 251) as u8, (i / 251) as u8, 7, 255]);
        views.push(json!({"buffer": 0, "byteOffset": bin.len(), "byteLength": png.len()}));
        bin.extend_from_slice(&png);
    }
    let glb = mk_glb(
        json!({
            "images": (0..n).map(|i| json!({"bufferView": i, "mimeType": "image/png"})).collect::<Vec<_>>(),
            "textures": (0..n).map(|i| json!({"source": i})).collect::<Vec<_>>(),
            "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
            "bufferViews": views
        }),
        &bin,
    );
    let o = transform_model_with(&glb, ".glb", None, TexOptions::default()).unwrap();
    assert!(o.variant.is_none());
    assert_eq!(o.reason, Some(NoGlbVariant::OverLimits));
    assert_eq!(o.times.total_ns(), 0);
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
/// big enough that its Brotli sidecar is smaller than the original.
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
fn entity_build_adds_glb_br_model_variants() {
    use super::glbbr;
    let grid = grid_glb(24);
    let (quad, _) = textured_quad_glb();
    // A `.gltf` gets no model variant: only a `.glb` is one file.
    let gltf = serde_json::to_vec(&json!({"asset": {"version": "2.0"}})).unwrap();
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkgrid", grid.clone());
    blobs.insert("bafkquad", quad);
    blobs.insert("bafkgltf", gltf);
    blobs.insert("bafkgridpng", smooth_png_bytes(64, 64));
    let scene = scene_of(
        "bafkentitym",
        &[
            ("models/grid.glb", "bafkgrid"),
            ("models/grid.png", "bafkgridpng"),
            ("models/quad.glb", "bafkquad"),
            ("models/plain.gltf", "bafkgltf"),
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
    let out = std::env::temp_dir().join(format!("abgen-bevy-models-{}", std::process::id()));
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
        ("model", "glb-br")
    );
    assert!(entry.width.is_none() && entry.mips.is_none());
    assert_eq!(entry.path, format!("bevy/m{MESH_GEN}/bafkgrid"));
    // The variant is the original, byte for byte.
    let glb = std::fs::read(key_path(&out, &entry.path).unwrap()).unwrap();
    assert_eq!(glb, grid);
    assert_eq!(cid_v1_raw_sha256(&glb), entry.cid);
    assert_eq!(glb.len() as u64, entry.size);
    // The sidecar holds the Brotli of the plain bytes and is smaller; bytes, sidecar and record
    // are listed for upload in that order, and the record says the sidecar exists and how the
    // model samples its image.
    let br_path = glbbr::br_sidecar(&key_path(&out, &entry.path).unwrap());
    let br = std::fs::read(&br_path).unwrap();
    assert!(br.len() < grid.len());
    assert_eq!(glbbr::brotli_decompress(&br).unwrap(), grid);
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
    // The `.gltf` has a final `not-glb` record and no entry.
    assert!(!built.entries.contains_key("bafkgltf"));
    let gltf_row = built
        .stats
        .model_files
        .iter()
        .find(|r| r.hash == "bafkgltf")
        .unwrap();
    assert_eq!(gltf_row.reason, Some(glbbr::NoModelVariant::NotGlb));
    let gltf_rec = key_path(&out, &record_key(&format!("bevy/m{MESH_GEN}/bafkgltf"))).unwrap();
    assert_eq!(
        Record::parse(&std::fs::read(&gltf_rec).unwrap())
            .unwrap()
            .reason
            .as_deref(),
        Some("not-glb")
    );
    // Stats: one row per model, totals, stage times.
    let st = &built.stats;
    let mv = &st.model_variants;
    assert_eq!(mv.considered, 3);
    assert_eq!(mv.variants + mv.size_gated.count, 2);
    assert_eq!(mv.no_variant.get("not-glb"), Some(&1));
    assert_eq!(mv.variants_original_bytes, mv.variant_bytes);
    assert!(mv.variant_bytes >= grid.len() as u64);
    assert!(mv.br_bytes >= br.len() as u64);
    assert!(st.stages.brotli_ns > 0);
    let row = st
        .model_files
        .iter()
        .find(|r| r.hash == "bafkgrid")
        .unwrap();
    assert_eq!(
        (row.format, row.br_size),
        (Some("glb-br"), Some(br.len() as u64))
    );
    assert!(st.summary_json().get("model_files").is_none());
    assert_eq!((st.reused_models, st.reused_textures), (0, 0));

    // A second build into the same root reuses everything from the records: no model is
    // fetched, nothing is encoded, nothing is listed for upload, the manifest is identical.
    let fetches = std::sync::Mutex::new(Vec::<String>::new());
    let counting = |h: &str| {
        fetches.lock().unwrap().push(h.to_string());
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
    // Only the `.gltf` is read again: its GLB variant's name depends on the buffers its JSON
    // names. A `.glb` is named by its CID alone.
    assert_eq!(*fetches.lock().unwrap(), vec!["bafkgltf".to_string()]);
    assert!(again.uploads.is_empty());
    assert_eq!(again.stats.reused_models, 3);
    assert_eq!(again.stats.reused_textures, 1);
    // Every GLB outcome is reused too (grid and `.gltf` negative, quad with a variant).
    let g = &again.stats.glb_variants;
    assert_eq!((g.considered, g.reused, g.variants), (3, 3, 1));
    assert_eq!(g.br_count, built.stats.glb_variants.br_count);
    assert_eq!(g.br_bytes, 0);
    assert_eq!(again.stats.unique_variants, 0);
    assert!(again.stats.stages.brotli_ns == 0 && again.stats.stages.probe_ns > 0);
    let row = again
        .stats
        .model_files
        .iter()
        .find(|r| r.hash == "bafkgrid")
        .unwrap();
    assert!(row.reused && row.variant_path.as_deref() == Some(entry.path.as_str()));
    // The `.gltf`'s negative record is reused too: its reason survives the round trip.
    let gated = again
        .stats
        .model_files
        .iter()
        .find(|r| r.hash == "bafkgltf")
        .unwrap();
    assert!(gated.reused);
    assert_eq!(gated.reason, Some(glbbr::NoModelVariant::NotGlb));

    // The lane's probe answers for keys that are not on disk: a record from elsewhere fills
    // the row without a fetch, and a negative record stops the encode.
    let out5 = std::env::temp_dir().join(format!("abgen-bevy-models-probe-{}", std::process::id()));
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
                reason: Some("tiny".into()),
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
    fetches.lock().unwrap().clear();
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
    assert!(png.reused && png.reason == Some(NoVariant::Tiny));
    // Only the models with a key the probe has no record for were fetched: the quad and the
    // `.gltf` (no record anywhere), and the grid for its GLB variant (the remote answers only
    // its `glb-br` key). Not the image.
    let mut fetched = fetches.lock().unwrap().clone();
    fetched.sort();
    assert_eq!(fetched, ["bafkgltf", "bafkgrid", "bafkquad"]);
    let grid_glb_key = format!("bevy/g{GLB_GEN}/bafkgrid");
    assert!(probed.uploads.iter().all(|u| u.key.contains("/bafkquad")
        || u.key.contains("/bafkgltf")
        || u.owner == grid_glb_key));
    let _ = std::fs::remove_dir_all(&out5);

    // Off: no `glb-br` entries at all (GLB variants are their own switch).
    let out2 = std::env::temp_dir().join(format!("abgen-bevy-models-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out2);
    let built2 = build_entity_variants(
        &scene,
        &lane,
        &out2,
        BuildOptions {
            models: false,
            ..BuildOptions::default()
        },
    )
    .unwrap();
    assert!(built2
        .entries
        .values()
        .flatten()
        .all(|e| e.format != glbbr::GLB_BR_FORMAT));
    assert!(built2.stats.model_files.is_empty());
    // Without `.br` sidecars there are no model variants either.
    let _ = std::fs::remove_dir_all(&out2);
    let built3 = build_entity_variants(
        &scene,
        &lane,
        &out2,
        BuildOptions {
            precompress_br: false,
            ..BuildOptions::default()
        },
    )
    .unwrap();
    assert!(built3.stats.model_files.is_empty());
    assert!(!built3.manifest.lanes.contains_key("mesh"));
    // GLB variants are still built, without sidecars.
    let quad_glb = &built3.entries["bafkquad"][0];
    assert_eq!(quad_glb.format, "glb");
    assert!(!key_path(&out2, &format!("{}.br", quad_glb.path))
        .unwrap()
        .exists());
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_dir_all(&out2);
}

/// A reused model variant whose record says it has no `.br` sidecar (the sidecar was lost, or
/// the record predates it) gets one: the sidecar is written and listed for upload before the
/// replaced record (`br: true`), and nothing is fetched.
#[test]
fn reused_model_variant_without_br_gets_a_sidecar_without_a_fetch() {
    use super::glbbr;
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
    let first = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    let entry = first.entries["bafkgrid"][0].clone();
    let vpath = key_path(&out, &entry.path).unwrap();
    let br_path = glbbr::br_sidecar(&vpath);
    let rec_path = key_path(&out, &record_key(&entry.path)).unwrap();
    // Drop the sidecar and say so in the record.
    std::fs::remove_file(&br_path).unwrap();
    let mut rec = Record::parse(&std::fs::read(&rec_path).unwrap()).unwrap();
    assert!(rec.br);
    rec.br = false;
    std::fs::write(&rec_path, serde_json::to_vec(&rec).unwrap()).unwrap();

    fetches.store(0, std::sync::atomic::Ordering::Relaxed);
    let again = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert_eq!(fetches.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(again.entries["bafkgrid"][0], entry);
    assert_eq!(again.stats.unique_variants, 0);
    let plain_bytes = std::fs::read(&vpath).unwrap();
    assert_eq!(
        glbbr::brotli_decompress(&std::fs::read(&br_path).unwrap()).unwrap(),
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
    // Default: texture, `glb-br` model and GLB variants.
    assert_eq!(
        BuildOptions::default(),
        BuildOptions {
            glb_variants: true,
            models: true,
            precompress_br: true,
            zstd_level: crate::ktx2_write::DEFAULT_ZSTD_LEVEL,
            epoch: None,
        }
    );
    // These runs look at textures only.
    let textures_only_opts = BuildOptions {
        models: false,
        glb_variants: false,
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
    let keys: Vec<&str> = m.keys().map(String::as_str).collect();
    let expected = vec!["bafkcolor", "bafkktx", "bafknormal"];
    assert_eq!(m["bafkktx"][0].format, "ktx2-bc7");
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

    // The missing image leaves the build incomplete: everything else is written, the manifest
    // is not (a rebuild once it can be fetched writes it).
    assert_eq!(built.incomplete, vec!["missing.png".to_string()]);
    assert!(!manifest_path(&out, "bafkentity").unwrap().exists());
    // The manifest bytes parse back to the same value.
    let parsed: Manifest = serde_json::from_slice(&built.manifest_bytes).unwrap();
    assert_eq!(&parsed, manifest);
    let raw: J = serde_json::from_slice(&built.manifest_bytes).unwrap();
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
    assert_eq!(reason("bafkktx"), None);
    assert_eq!(st.variants["ktx2-bc1 from png"].count, 1);
    assert_eq!(st.variants["ktx2-bc5 from png"].count, 1);
    assert_eq!(st.bc1_rejected, 0);
    let color = st.files.iter().find(|f| f.hash == "bafkcolor").unwrap();
    // Linear: BC1 without the PSNR check.
    assert!(color.bc1_rejected.is_none() && color.bc1_psnr_db.is_none());
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

    // GLB variants: the GLB with embedded images gets one (no size fields); the GLB whose only
    // image is external gets none; the texture entries are unchanged.
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
    assert_eq!(model.path, format!("bevy/g{GLB_GEN}/bafkembed"));
    let glb = std::fs::read(key_path(&out3, &model.path).unwrap()).unwrap();
    assert_eq!(cid_v1_raw_sha256(&glb), model.cid);
    // Its `.br` sidecar exists exactly when the record says so, and decodes to the variant.
    let rec3 =
        Record::parse(&std::fs::read(key_path(&out3, &record_key(&model.path)).unwrap()).unwrap())
            .unwrap();
    let br_path = key_path(&out3, &format!("{}.br", model.path)).unwrap();
    assert_eq!(rec3.br, br_path.is_file());
    if rec3.br {
        let br = std::fs::read(&br_path).unwrap();
        assert!(br.len() < glb.len());
        assert_eq!(super::glbbr::brotli_decompress(&br).unwrap(), glb);
    }
    // Stats: one GLB row per model, the embedded images' outcomes on the built one.
    let gs = &built3.stats.glb_variants;
    assert_eq!((gs.considered, gs.variants), (2, 1));
    assert_eq!(gs.no_variant["nothing-to-change"], 1);
    assert_eq!(
        gs.images_converted.values().map(|c| c.count).sum::<u64>(),
        2
    );
    assert!(gs.images_kept.is_empty());
    assert!(gs.gpu_variant_bytes < gs.gpu_original_bytes);
    let embed_row = built3
        .stats
        .glb_files
        .iter()
        .find(|r| r.hash == "bafkembed")
        .unwrap();
    assert_eq!(embed_row.images.len(), 2);
    assert_eq!(embed_row.br_size.is_some(), rec3.br);
    assert!(built3.stats.summary_json().get("glb_files").is_none());
    // The GLB nothing changes in has a negative record; the `.gltf` with no external buffer
    // keeps its plain name.
    let ext_rec = Record::parse(
        &std::fs::read(key_path(&out3, &format!("bevy/g{GLB_GEN}/bafkext.json")).unwrap()).unwrap(),
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

/// A texture whose content cannot be fetched leaves the build incomplete: the other variants
/// and records are written, the manifest is not (and an older one is removed). Once the fetch
/// works, the rebuild encodes only that texture and writes the manifest.
#[test]
fn texture_fetch_failure_writes_no_manifest_until_a_rebuild_completes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkgridpng", noise_png_bytes(64, 64));
    blobs.insert("bafklate", noise_png_bytes(64, 64));
    let scene = scene_of(
        "bafkentityinc",
        &[
            ("tex/grid.png", "bafkgridpng"),
            ("tex/late.png", "bafklate"),
        ],
    );
    let late_ok = AtomicBool::new(false);
    let fetches = std::sync::Mutex::new(Vec::<String>::new());
    let fetch = |h: &str| {
        fetches.lock().unwrap().push(h.to_string());
        if h == "bafklate" && !late_ok.load(Ordering::Relaxed) {
            return None;
        }
        blobs.get(h).cloned()
    };
    let probe = |_: &str| None;
    let lane = Lane {
        fetch: &fetch,
        probe: &probe,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    let out = std::env::temp_dir().join(format!("abgen-bevy-incomplete-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let mpath = manifest_path(&out, "bafkentityinc").unwrap();
    std::fs::create_dir_all(mpath.parent().unwrap()).unwrap();
    std::fs::write(&mpath, b"{}").unwrap();

    let first = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert_eq!(first.incomplete, vec!["tex/late.png".to_string()]);
    assert!(!mpath.exists());
    // The grid's variant and record are written and listed for upload; the failed fetch has
    // no record.
    let grid = first.entries["bafkgridpng"][0].clone();
    assert!(key_path(&out, &grid.path).unwrap().is_file());
    assert!(key_path(&out, &record_key(&grid.path)).unwrap().is_file());
    assert!(first.uploads.iter().all(|u| u.owner == grid.path));
    assert!(!first.entries.contains_key("bafklate"));
    assert!(
        !key_path(&out, &format!("bevy/t{TEX_GEN}/bafklate_srgb.json"))
            .unwrap()
            .exists()
    );

    // The fetch works now: only the late texture is fetched and encoded, and the manifest is
    // written.
    late_ok.store(true, Ordering::Relaxed);
    fetches.lock().unwrap().clear();
    let again = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert!(again.incomplete.is_empty());
    assert_eq!(*fetches.lock().unwrap(), vec!["bafklate".to_string()]);
    assert_eq!(again.entries["bafkgridpng"][0], grid);
    let late = &again.entries["bafklate"][0];
    assert!(again.uploads.iter().all(|u| u.owner == late.path));
    assert_eq!(std::fs::read(&mpath).unwrap(), again.manifest_bytes);
    assert_eq!(
        again.manifest.files.keys().collect::<Vec<_>>(),
        vec!["bafkgridpng", "bafklate"]
    );
    let _ = std::fs::remove_dir_all(&out);
}

/// A model whose content cannot be fetched leaves its images' slots unknown: the build is
/// incomplete and writes no manifest, though the textures' variants and records (correct for
/// the keys they are under) are written. Once the model can be fetched, the rebuild reads its
/// uses, encodes the normal map in its slot and writes the manifest; the unrelated texture is
/// reused.
#[test]
fn model_fetch_failure_writes_no_manifest_until_a_rebuild_completes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let gltf = serde_json::to_vec(&json!({
        "asset": {"version": "2.0"},
        "images": [{"uri": "../tex/normal.png"}],
        "textures": [{"source": 0}],
        "materials": [{"normalTexture": {"index": 0}}]
    }))
    .unwrap();
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkmodel", gltf);
    blobs.insert("bafknormal", noise_png_bytes(64, 64));
    blobs.insert("bafkcolor", smooth_png_bytes(64, 64));
    let scene = scene_of(
        "bafkentitymodelinc",
        &[
            ("models/ext.gltf", "bafkmodel"),
            ("tex/normal.png", "bafknormal"),
            ("color.png", "bafkcolor"),
        ],
    );
    let model_ok = AtomicBool::new(false);
    let fetches = std::sync::Mutex::new(Vec::<String>::new());
    let fetch = |h: &str| {
        fetches.lock().unwrap().push(h.to_string());
        if h == "bafkmodel" && !model_ok.load(Ordering::Relaxed) {
            return None;
        }
        blobs.get(h).cloned()
    };
    let probe = |_: &str| None;
    let lane = Lane {
        fetch: &fetch,
        probe: &probe,
        stored: &|_: &str| -> Option<Vec<u8>> { None },
        local_records: true,
    };
    let out = std::env::temp_dir().join(format!(
        "abgen-bevy-incomplete-model-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&out);
    let mpath = manifest_path(&out, "bafkentitymodelinc").unwrap();

    let first = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert_eq!(first.incomplete, vec!["models/ext.gltf".to_string()]);
    assert!(!mpath.exists());
    // Without the model the normal map falls back to the sRGB slot; that key's record is still
    // written (it is what the image is in that slot).
    assert!(
        key_path(&out, &format!("bevy/t{TEX_GEN}/bafknormal_srgb.json"))
            .unwrap()
            .is_file()
    );
    let color = first.entries["bafkcolor"][0].clone();
    assert!(key_path(&out, &record_key(&color.path)).unwrap().is_file());
    // The failed fetch has no record.
    assert!(!key_path(&out, &format!("bevy/m{MESH_GEN}/bafkmodel.json"))
        .unwrap()
        .exists());

    model_ok.store(true, Ordering::Relaxed);
    fetches.lock().unwrap().clear();
    let again = build_entity_variants(&scene, &lane, &out, BuildOptions::default()).unwrap();
    assert!(again.incomplete.is_empty());
    let fetched: std::collections::BTreeSet<String> =
        fetches.lock().unwrap().iter().cloned().collect();
    assert!(fetched.contains("bafkmodel") && fetched.contains("bafknormal"));
    assert!(!fetched.contains("bafkcolor"));
    let normal = &again.entries["bafknormal"][0];
    assert_eq!(normal.path, format!("bevy/t{TEX_GEN}/bafknormal_normal"));
    assert_eq!(normal.format, "ktx2-bc5");
    assert_eq!(again.entries["bafkcolor"][0], color);
    assert!(again
        .uploads
        .iter()
        .all(|u| u.owner == normal.path || u.owner.ends_with("/bafkmodel")));
    assert_eq!(std::fs::read(&mpath).unwrap(), again.manifest_bytes);
    let _ = std::fs::remove_dir_all(&out);
}
