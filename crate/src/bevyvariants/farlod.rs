//! The Bevy copy of a scene's far LOD: the published `{sceneId}_1.glb` (gltfpack layout, see
//! `crate::lodgen::quantize`) run through the GLB rewrite ([`super::model`]), so every embedded
//! atlas is a BC1 / BC5 / BC7 KTX2 (Zstandard, with mips) instead of a JPEG/PNG the explorer
//! holds as RGBA8, plus a Brotli `.br` copy. Geometry, nodes, materials
//! and extensions (`KHR_mesh_quantization`, `KHR_texture_transform`) are copied as they are.
//!
//! It is derived from the plain GLB only, after the Unity bundles are built from that same
//! file, and written beside it under its own directory; the plain GLB and the bundles are never
//! touched. Differences from the scene lane's GLB variants, because a far LOD is seen from
//! 80-320 m:
//!
//! - **Mips.** The full chain down to 1x1 (+33% texture memory). At that distance many atlases
//!   are minified, and level 0 alone would shimmer as the camera moves; the published GLB's
//!   sampler already asks for `LINEAR_MIPMAP_LINEAR`.
//! - **Unused alpha.** An atlas drawn only by `OPAQUE` materials is encoded as opaque, so it
//!   can take BC1 (behind the same PSNR check). The LOD bake gives most atlases an alpha
//!   channel whether or not any material reads it. An atlas a `BLEND` or `MASK` material
//!   draws keeps its alpha (BC7).
//! - **Always written.** A far LOD with nothing to rewrite (no embedded image) gets the plain
//!   bytes as its Bevy copy, so the explorer finds one at the key it derives from the scene id.
//!   Only a GLB whose rewrite fails, or that gets no GLB variant for another reason
//!   ([`NoGlbVariant`]), gets none, and the explorer falls back to the plain file.
//! - **Always a `.br`.** The explorer fetches `{key}.br` by name, so it is written even in the
//!   unlikely case it is not smaller: a rebuild then always overwrites the previous one, and no
//!   stale `.br` can outlive the GLB it was made from (objects are only ever uploaded, never
//!   deleted).
//!
//! On disk: `{scene_dir}/`[`GLB_DIR`]`/{sid}_{level}.glb` and its `.br`. Published under
//! [`KEY_DIR`] with the same file names, unversioned like the plain GLB.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use super::glbbr::brotli_compress;
use super::model::{transform_model_with, EmbeddedImage, NoGlbVariant};
use super::texture::TexOptions;

pub use crate::lods::{BEVY_GLB_DIR as GLB_DIR, BEVY_GLB_KEY_DIR as KEY_DIR};

/// The far LOD copy's texture rules: the scene lane's, plus mips and unused alpha dropped.
pub fn options() -> TexOptions {
    TexOptions {
        mips: true,
        drop_unused_alpha: true,
        ..TexOptions::default()
    }
}

/// A built far LOD copy.
#[derive(Debug)]
pub struct FarLod {
    pub glb: Vec<u8>,
    /// The Brotli copy of `glb`, always (see the module docs).
    pub br: Vec<u8>,
    /// The rewrite ran and changed nothing (no embedded image): `glb` is the plain GLB.
    pub copied: bool,
    /// Each embedded atlas's outcome (format, size, whether its alpha was dropped).
    pub images: Vec<EmbeddedImage>,
}

/// Build the Bevy copy of the far LOD `plain` (a published `{sid}_{level}.glb`). `Ok(Err)`
/// with the reason when it gets none (any [`NoGlbVariant`] but `NothingToChange`, which copies
/// the plain bytes). `Err` when the GLB cannot be rewritten.
pub fn build(plain: &[u8], opts: TexOptions) -> Result<std::result::Result<FarLod, NoGlbVariant>> {
    let outcome = transform_model_with(plain, ".glb", None, opts)?;
    let (glb, copied) = match (outcome.variant, outcome.reason) {
        (Some(v), _) => (v.glb, false),
        (None, Some(NoGlbVariant::NothingToChange)) => (plain.to_vec(), true),
        (None, reason) => return Ok(Err(reason.unwrap_or(NoGlbVariant::NothingToChange))),
    };
    let br = brotli_compress(&glb)?;
    Ok(Ok(FarLod {
        glb,
        br,
        copied,
        images: outcome.images,
    }))
}

/// The files a copy of `level` writes under `{scene_dir}/`[`GLB_DIR`]: the `.br`, the GLB.
fn level_files(scene_dir: &Path, scene_id: &str, level: u32) -> [PathBuf; 2] {
    let glb = scene_dir
        .join(GLB_DIR)
        .join(crate::lods::published_glb_name(scene_id, level));
    [super::glbbr::br_sidecar(&glb), glb]
}

/// Write a built copy of `scene_id`'s `level` under `{scene_dir}/`[`GLB_DIR`]: the GLB and its
/// `.br`. Returns the GLB's path.
pub fn write(scene_dir: &Path, scene_id: &str, level: u32, far: &FarLod) -> Result<PathBuf> {
    let dir = scene_dir.join(GLB_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let [br_path, path] = level_files(scene_dir, scene_id, level);
    crate::lods::write_atomic(&path, &far.glb)?;
    crate::lods::write_atomic(&br_path, &far.br)?;
    Ok(path)
}

/// Build and write the Bevy copy of the plain far LOD GLB at `plain_path`. A copy that cannot
/// be built is logged and skipped, and any copy of that level already in `scene_dir` is
/// removed, so nothing older is published beside the new plain GLB: the explorer falls back to
/// the plain GLB, and the LOD build it follows stands.
pub fn write_from_plain(
    scene_dir: &Path,
    scene_id: &str,
    level: u32,
    plain_path: &Path,
) -> Option<FarLod> {
    let result = std::fs::read(plain_path)
        .with_context(|| format!("read {}", plain_path.display()))
        .and_then(|plain| build(&plain, options()));
    let failure = match result {
        Ok(Ok(far)) => match write(scene_dir, scene_id, level, &far) {
            Ok(_) => return Some(far),
            Err(e) => format!("write failed: {e:#}"),
        },
        Ok(Err(reason)) => format!("no GLB variant ({reason:?})"),
        Err(e) => format!("build failed: {e:#}"),
    };
    tracing::warn!(scene = scene_id, level, %failure, "bevy far lod: no Bevy copy");
    for f in level_files(scene_dir, scene_id, level) {
        let _ = std::fs::remove_file(f);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::model::{AlphaClass, LodImage, LodMaterial, LodModel, LodPrimitive};
    use serde_json::{json, Value as J};

    /// A smooth gradient (BC1 holds it above the PSNR floor) with an alpha channel that is not
    /// all 255, so only the unused-alpha rule can make it BC1.
    fn gradient_png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([
                (x * 255 / w) as u8,
                (y * 255 / h) as u8,
                128,
                if (x + y) % 2 == 0 { alpha } else { 255 },
            ])
        });
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn quad(material: usize, z: f32) -> LodPrimitive {
        LodPrimitive {
            positions: vec![[0.0, 0.0, z], [1.0, 0.0, z], [0.0, 1.0, z], [1.0, 1.0, z]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]],
            indices: vec![0, 1, 2, 2, 1, 3],
            material,
            ..Default::default()
        }
    }

    /// A far LOD in the published layout: an opaque atlas whose alpha no material reads, and a
    /// blended one.
    fn published_far_lod(images: bool) -> Vec<u8> {
        let mat = |name: &str, class, image| LodMaterial {
            name: name.to_string(),
            class,
            image,
            ..Default::default()
        };
        let model = LodModel {
            root_name: "bafkscene_1".to_string(),
            primitives: vec![quad(0, 0.0), quad(1, 1.0)],
            materials: vec![
                mat(
                    "TextureBakeResult-mat",
                    AlphaClass::Opaque,
                    images.then_some(0),
                ),
                mat(
                    "TextureBakeResult-mat-transparent",
                    AlphaClass::Blend,
                    images.then_some(1),
                ),
            ],
            images: if images {
                vec![
                    LodImage {
                        bytes: gradient_png(64, 64, 128),
                        mime: "image/png".to_string(),
                    },
                    LodImage {
                        bytes: gradient_png(32, 32, 64),
                        mime: "image/png".to_string(),
                    },
                ]
            } else {
                Vec::new()
            },
            log: Vec::new(),
        };
        let float = crate::lodgen::emit::emit_glb(&model).unwrap();
        crate::lodgen::quantize::write_gltfpack_layout(&float, "bafkscene").unwrap()
    }

    fn doc(glb: &[u8]) -> J {
        crate::naming::parse_gltf_doc(glb, ".glb").unwrap()
    }

    fn bin(glb: &[u8]) -> Vec<u8> {
        crate::bvwebgpu::emit::split_container(glb).unwrap().1
    }

    fn view_bytes<'a>(bin: &'a [u8], doc: &J, view: u64) -> &'a [u8] {
        let v = &doc["bufferViews"][view as usize];
        let off = v["byteOffset"].as_u64().unwrap_or(0) as usize;
        let len = v["byteLength"].as_u64().unwrap() as usize;
        &bin[off..off + len]
    }

    /// Accessor contents as `(count, type, componentType, normalized, bytes)`.
    fn accessors(glb: &[u8]) -> Vec<(J, J, J, J, Vec<u8>)> {
        let d = doc(glb);
        let bin = bin(glb);
        d["accessors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                let v = view_bytes(&bin, &d, a["bufferView"].as_u64().unwrap());
                let off = a["byteOffset"].as_u64().unwrap_or(0) as usize;
                (
                    a["count"].clone(),
                    a["type"].clone(),
                    a["componentType"].clone(),
                    a["normalized"].clone(),
                    v[off..].to_vec(),
                )
            })
            .collect()
    }

    #[test]
    fn far_lod_copy_keeps_geometry_and_replaces_atlases_with_mipped_bc_ktx2() {
        let plain = published_far_lod(true);
        let far = build(&plain, options()).unwrap().unwrap();
        let (p, b) = (doc(&plain), doc(&far.glb));

        // Geometry, nodes, materials and extensions are the plain file's.
        assert_eq!(accessors(&plain), accessors(&far.glb));
        for k in [
            "nodes",
            "meshes",
            "materials",
            "textures",
            "samplers",
            "extensionsUsed",
        ] {
            assert_eq!(p[k], b[k], "{k}");
        }
        assert_eq!(
            b["extensionsUsed"],
            json!(["KHR_mesh_quantization", "KHR_texture_transform"])
        );

        // Both atlases are KTX2 with a full mip chain; the opaque one dropped its alpha and
        // took BC1, the blended one kept it (BC7).
        let rows = &far.images;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].format, Some("ktx2-bc1"));
        assert!(rows[0].alpha_dropped);
        assert_eq!(rows[0].mips, Some(7));
        assert_eq!(rows[1].format, Some("ktx2-bc7"));
        assert!(!rows[1].alpha_dropped);
        assert_eq!(rows[1].mips, Some(6));
        let far_bin = bin(&far.glb);
        for (i, img) in b["images"].as_array().unwrap().iter().enumerate() {
            assert_eq!(img["mimeType"], "image/ktx2");
            let bytes = view_bytes(&far_bin, &b, img["bufferView"].as_u64().unwrap());
            let levels = crate::ktx2_write::read_levels(bytes).unwrap();
            assert_eq!(levels.len() as u32, rows[i].mips.unwrap());
        }
        // Texture memory counts every level.
        assert_eq!(
            rows[0].variant_gpu_bytes,
            Some(super::super::texture::chain_gpu_bytes(
                crate::ktx2_write::BcFormat::Bc1Rgb,
                64,
                64,
                7
            ))
        );
        assert!(!far.copied);

        // Brotli round-trips.
        assert!(far.br.len() < far.glb.len());
        assert_eq!(
            super::super::glbbr::brotli_decompress(&far.br).unwrap(),
            far.glb
        );
    }

    #[test]
    fn keep_alpha_and_no_mips_follow_the_options() {
        let plain = published_far_lod(true);
        let opts = TexOptions {
            mips: false,
            drop_unused_alpha: false,
            ..options()
        };
        let far = build(&plain, opts).unwrap().unwrap();
        assert_eq!(far.images[0].format, Some("ktx2-bc7"));
        assert!(!far.images[0].alpha_dropped);
        assert_eq!(far.images[0].mips, Some(1));
    }

    #[test]
    fn far_lod_without_images_is_copied_and_published_beside_the_plain_glb() {
        let plain = published_far_lod(false);
        let far = build(&plain, options()).unwrap().unwrap();
        assert!(far.copied);
        assert_eq!(far.glb, plain);

        let base = std::env::temp_dir().join(format!("farlod-pub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scene = base.join("bafkscene");
        let plain_dir = scene.join(crate::lods::PUBLISHED_GLB_DIR);
        std::fs::create_dir_all(&plain_dir).unwrap();
        std::fs::write(plain_dir.join("bafkscene_1.glb"), &plain).unwrap();
        let path = write(&scene, "bafkscene", 1, &far).unwrap();
        assert_eq!(path, scene.join("lods-bevy/lods/bafkscene_1.glb"));

        let objs = crate::lods::published_objects(&scene, &[1]);
        let keys: Vec<&str> = objs.iter().map(|o| o.key.as_str()).collect();
        let want = vec![
            "LOD/lods-unity/lods/bafkscene_1.glb",
            "LOD/lods-bevy/lods/bafkscene_1.glb",
            "LOD/lods-bevy/lods/bafkscene_1.glb.br",
        ];
        assert_eq!(keys, want);
        // Only the requested levels' Bevy files are listed.
        assert!(!crate::lods::published_objects(&scene, &[0])
            .iter()
            .any(|o| o.key.starts_with(KEY_DIR)));
        // Only the Unity objects get content-addressed twins.
        let twins = crate::lods::with_digest_twins(objs, "bafkscene", "d1");
        assert_eq!(twins.len(), want.len() + 1);
        assert_eq!(
            twins.last().unwrap().key,
            "LOD/lods-unity/lods/bafkscene_d1_1.glb"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_failed_copy_removes_the_previous_one_of_its_level() {
        let base = std::env::temp_dir().join(format!("farlod-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scene = base.join("bafkscene");
        let plain = scene.join("plain.glb");
        std::fs::create_dir_all(&scene).unwrap();
        std::fs::write(&plain, published_far_lod(true)).unwrap();
        assert!(write_from_plain(&scene, "bafkscene", 1, &plain).is_some());
        let files = level_files(&scene, "bafkscene", 1);
        assert!(files.iter().all(|f| f.exists()));

        // A rebuild whose plain GLB will not rewrite leaves no copy of the earlier build behind.
        std::fs::write(&plain, b"not a glb").unwrap();
        assert!(write_from_plain(&scene, "bafkscene", 1, &plain).is_none());
        assert!(files.iter().all(|f| !f.exists()));
        assert!(crate::lods::published_objects(&scene, &[1]).is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn alpha_is_unused_only_for_images_opaque_materials_draw() {
        let d = json!({
            "images": [{}, {}, {}, {}, {}, {}],
            "textures": [
                {"source": 0}, {"source": 1}, {"source": 2},
                {"extensions": {"KHR_texture_basisu": {"source": 3}}},
                {"source": 4}, {"source": 5}
            ],
            "materials": [
                {"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}},
                 "emissiveTexture": {"index": 2}},
                {"alphaMode": "OPAQUE", "pbrMetallicRoughness": {"baseColorTexture": {"index": 3}}},
                {"alphaMode": "MASK", "pbrMetallicRoughness": {"baseColorTexture": {"index": 1}}},
                {"alphaMode": "BLEND", "pbrMetallicRoughness": {"baseColorTexture": {"index": 2}}},
                {"extensions": {"KHR_materials_pbrSpecularGlossiness":
                    {"specularGlossinessTexture": {"index": 4}}}}
            ]
        });
        // 0: opaque base colour; 3: opaque base colour through basisu. 1 is masked, 2 is
        // also blended, 4 is a spec-gloss map (alpha is gloss), 5 is never referenced.
        let got: Vec<usize> = super::super::model::alpha_unused_images(&d)
            .into_iter()
            .collect();
        assert_eq!(got, vec![0, 3]);
    }
}
