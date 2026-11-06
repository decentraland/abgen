use super::model::{transform_model, KTX2_MIME};
use super::texture::{
    choose_format, encode_texture, transcode_uastc_bc7, EncodePath, Slot, TexFormat, Uses,
};
use crate::ktx2::Ktx2Header;
use crate::ktx2_write::{read_levels, BcFormat};
use super::*;
use crate::bvwebgpu::emit;
use crate::bvwebgpu::emit::tests::{mk_glb, noise_png_bytes, png_bytes, quad_bin_and_views};
use crate::catalyst::{ContentEntry, Scene};
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
        assert_eq!(l.len(), crate::ktx2_write::level_bytes(format, w, hh), "level {m}");
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
    t.transcode(level, basisu::TargetFormat::Bc7Rgba, basisu::DecodeFlags::NONE)
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
        content_path(root, "bafkreiabc").unwrap(),
        std::path::Path::new("/out/bevy/contents/bafkreiabc")
    );
    for bad in ["", "..", "a/b", "b64-abc", "x.json", "a b"] {
        assert!(manifest_path(root, bad).is_none(), "{bad}");
        assert!(content_path(root, bad).is_none(), "{bad}");
    }
    assert_eq!(manifest_key("e"), "bevy/manifest/e.json");
    assert_eq!(content_key("c"), "bevy/contents/c");
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
    let opaque = noise_png_bytes(16, 16);
    let v = encode_texture(&opaque, Slot::Srgb).unwrap().unwrap();
    assert_eq!((v.format, v.width, v.height, v.mips), (TexFormat::Bc1, 16, 16, 5));
    let (h, levels) = ktx(&v.bytes, BcFormat::Bc1Rgb);
    assert_eq!((h.width, h.height, h.levels), (16, 16, 5));
    assert_eq!(levels[0].len(), 16 * 8);

    let alpha = noise_alpha_png(16, 8);
    let v = encode_texture(&alpha, Slot::Srgb).unwrap().unwrap();
    assert_eq!(v.format, TexFormat::Bc7);
    let (h, _) = ktx(&v.bytes, BcFormat::Bc7);
    assert_eq!((h.width, h.height, h.levels), (16, 8, 5));

    let v = encode_texture(&opaque, Slot::Normal).unwrap().unwrap();
    assert_eq!(v.format, TexFormat::Bc5);
    let (h, _) = ktx(&v.bytes, BcFormat::Bc5);
    assert_eq!(h.levels, 5);
}

#[test]
fn encoded_levels_equal_the_encoders_chain() {
    // The container is a lossless wrapper: its levels concatenate to the encoder's output.
    let png = noise_alpha_png(16, 16);
    let v = encode_texture(&png, Slot::Linear).unwrap().unwrap();
    assert_eq!(v.format, TexFormat::Bc7);
    let rgba = image::load_from_memory(&png).unwrap().to_rgba8().into_raw();
    let mut bled = rgba;
    crate::alpha_bleed::alpha_bleed_inplace(&mut bled, 16, 16);
    let (chain, mips) = crate::bc7_pure::encode_bc7_mip_chain_with_profile(
        &bled,
        16,
        16,
        None,
        false,
        false,
        false,
        crate::bc7_pure::Bc7Profile::Basic,
    );
    assert_eq!(mips, 5);
    let (_, levels) = ktx(&v.bytes, BcFormat::Bc7);
    assert_eq!(levels.concat(), chain);
}

#[test]
fn tiny_unknown_and_oversize_inputs() {
    assert!(encode_texture(&png_bytes(2, 2, [1, 2, 3, 255]), Slot::Srgb)
        .unwrap()
        .is_none());
    assert!(encode_texture(&png_bytes(64, 1, [1, 2, 3, 255]), Slot::Srgb)
        .unwrap()
        .is_none());
    assert!(encode_texture(b"ID3 not an image", Slot::Srgb)
        .unwrap()
        .is_none());
    // Sniffed by bytes: PNG magic with a broken body is an error, not "no variant".
    assert!(encode_texture(b"\x89PNG\r\n\x1a\nbroken", Slot::Srgb).is_err());
    let v = encode_texture(&noise_png_bytes(2048, 16), Slot::Srgb)
        .unwrap()
        .unwrap();
    assert_eq!((v.width, v.height, v.mips), (1024, 8, 11));
    // NPOT snaps to powers of two (whole blocks for wgpu).
    let v = encode_texture(&noise_png_bytes(24, 12), Slot::Srgb)
        .unwrap()
        .unwrap();
    assert_eq!((v.width, v.height), (32, 16));
}

#[test]
fn jpeg_is_sniffed_and_encoded() {
    let jpeg = include_bytes!("../../testdata/gradient-16x16.jpg");
    let v = encode_texture(jpeg, Slot::Srgb).unwrap().unwrap();
    assert_eq!((v.format, v.width, v.height), (TexFormat::Bc1, 16, 16));
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
    let v = encode_texture(&png, Slot::Linear).unwrap().unwrap();
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
    assert_eq!((v.format, v.width, v.height, v.mips), (TexFormat::Bc7, 8, 8, 1));
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
    assert_eq!(gated.is_some(), v.bytes.len() <= UASTC_8.len());
}

#[test]
fn uastc_that_needs_more_than_a_transcode_gets_no_variant() {
    // 6x6 is not whole 4x4 blocks: the explorer would decode and re-encode it, so no variant.
    assert!(transcode_uastc_bc7(UASTC_6).is_none());
    assert!(encode_texture(UASTC_6, Slot::Srgb).unwrap().is_none());
    // Not a UASTC KTX2 the gate takes (here: a BC7 KTX2 of our own).
    let ours = crate::ktx2_write::write_ktx2(BcFormat::Bc7, 4, 4, &[&[0u8; 16][..]]).unwrap();
    assert!(transcode_uastc_bc7(&ours).is_none());
}

fn textured_quad_glb() -> (Vec<u8>, Vec<u8>) {
    // Image 0 (normal map) is embedded in the quad's bin; image 1 is an opaque base colour
    // appended after it; image 2 is external.
    let normal_png = noise_png_bytes(16, 16);
    let (mut bin, mut views, accessors) = quad_bin_and_views(&normal_png);
    emit::pad4(&mut bin, 0);
    let color_png = noise_png_bytes(32, 32);
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
                "pbrMetallicRoughness": {"baseColorTexture": {"index": 1}},
                "occlusionTexture": {"index": 2}
            }],
            "textures": [{"source": 0}, {"source": 1}, {"source": 2}],
            "images": [
                {"bufferView": 6, "mimeType": "image/png"},
                {"bufferView": 7, "mimeType": "image/png"},
                {"uri": "textures/ao.png"}
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
    assert_eq!(doc["images"][2], json!({"uri": "textures/ao.png"}));
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
    // A model whose only image is external: no model variant, but its image is normal-only.
    let ext_glb = {
        let gltf = json!({
            "asset": {"version": "2.0"},
            "images": [{"uri": "../tex/normal.png"}],
            "textures": [{"source": 0}],
            "materials": [{"normalTexture": {"index": 0}}]
        });
        serde_json::to_vec(&gltf).unwrap()
    };
    let mut blobs: HashMap<&str, Vec<u8>> = HashMap::new();
    blobs.insert("bafkcolor", noise_png_bytes(16, 16));
    blobs.insert("bafknormal", noise_png_bytes(16, 16));
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
    let run = |tag: &str, opts: BuildOptions| {
        let out = std::env::temp_dir().join(format!(
            "abgen-bevy-variants-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&out);
        let built = build_entity_variants(&scene, &fetch, &out, opts).unwrap();
        (out, built)
    };
    // Default: texture variants only (GLB variants are opt-in).
    assert_eq!(BuildOptions::default(), BuildOptions { glb_variants: false });
    let (out, built) = run("a", BuildOptions::default());
    let m = &built.manifest;
    assert_eq!(m.v, 1);
    assert_eq!(m.entity, "bafkentity");
    assert!(m.generator.starts_with("abgen "));
    let ktx_variant = encode_texture(UASTC_8, Slot::Srgb).unwrap();
    let keys: Vec<&str> = m.files.keys().map(String::as_str).collect();
    let mut expected = vec!["bafkcolor", "bafknormal"];
    if ktx_variant.is_some() {
        expected.insert(1, "bafkktx");
        assert_eq!(m.files["bafkktx"][0].format, "ktx2-bc7");
    }
    assert_eq!(keys, expected);
    assert_eq!(m.files["bafkcolor"][0].format, "ktx2-bc1");
    assert_eq!(m.files["bafknormal"][0].format, "ktx2-bc5");
    // The images embedded in embed.glb get no texture variant of their own.
    assert!(m.files.values().flatten().all(|e| e.kind == "texture"));
    let tex = &m.files["bafkcolor"][0];
    assert_eq!(
        (tex.kind.as_str(), tex.width, tex.height, tex.mips),
        ("texture", Some(16), Some(16), Some(5))
    );

    // Every listed variant is stored under the CID of its bytes, with its size.
    for entries in m.files.values() {
        for e in entries {
            let bytes = std::fs::read(content_path(&out, &e.cid).unwrap()).unwrap();
            assert_eq!(cid_v1_raw_sha256(&bytes), e.cid);
            assert_eq!(bytes.len() as u64, e.size);
            assert!(e.cid.starts_with("bafkrei"));
        }
    }
    assert_eq!(built.variants.len(), expected.len());

    // The manifest on disk is the returned bytes and parses back to the same value.
    let on_disk = std::fs::read(manifest_path(&out, "bafkentity").unwrap()).unwrap();
    assert_eq!(on_disk, built.manifest_bytes);
    let parsed: Manifest = serde_json::from_slice(&on_disk).unwrap();
    assert_eq!(&parsed, m);
    let raw: J = serde_json::from_slice(&on_disk).unwrap();
    assert_eq!(raw["files"]["bafkcolor"][0]["width"], 16);

    // Same inputs, same bytes.
    let (out2, built2) = run("b", BuildOptions::default());
    assert_eq!(built2.manifest_bytes, built.manifest_bytes);

    // Opt-in: the GLB with embedded images gets a model variant (no size fields); the GLB whose
    // only image is external still gets none; the texture entries are unchanged.
    let (out3, built3) = run("g", BuildOptions { glb_variants: true });
    let m3 = &built3.manifest;
    let model = &m3.files["bafkembed"][0];
    assert_eq!((model.kind.as_str(), model.format.as_str()), ("model", "glb"));
    assert!(model.width.is_none() && model.mips.is_none());
    assert!(!m3.files.contains_key("bafkext"));
    let raw3: J = serde_json::from_slice(&built3.manifest_bytes).unwrap();
    assert!(raw3["files"]["bafkembed"][0].get("width").is_none());
    let mut textures_only = m3.files.clone();
    textures_only.remove("bafkembed");
    assert_eq!(&textures_only, &m.files);
    let glb = std::fs::read(content_path(&out3, &model.cid).unwrap()).unwrap();
    assert_eq!(cid_v1_raw_sha256(&glb), model.cid);

    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_dir_all(&out2);
    let _ = std::fs::remove_dir_all(&out3);
}
