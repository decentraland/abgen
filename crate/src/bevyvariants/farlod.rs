//! The Bevy copy of a scene's far LOD: the published `{sceneId}_1.glb` (gltfpack layout, see
//! `crate::lodgen::quantize`) run through the GLB rewrite ([`super::model`]), so every embedded
//! atlas is a BC1 / BC5 / BC7 KTX2 (Zstandard) instead of a JPEG/PNG the explorer holds as
//! RGBA8, plus a Brotli `.br` copy. Geometry, nodes, materials and extensions
//! (`KHR_mesh_quantization`, `KHR_texture_transform`) are copied as they are.
//!
//! It is derived from the plain GLB only, after the Unity bundles are built from that same
//! file, and written beside it under its own directory; the plain GLB and the bundles are never
//! touched. Differences from the scene lane's GLB variants, all because a far LOD is seen from
//! 80-320 m:
//!
//! - **Mips.** The full chain down to 1x1 (+33% memory). Level 0 alone shimmers at that
//!   distance and samples more texture memory than the screen needs; the published GLB's
//!   sampler already asks for `LINEAR_MIPMAP_LINEAR`.
//! - **Unused alpha.** An atlas drawn only by `OPAQUE` materials is encoded as opaque, so it
//!   can take BC1 (behind the same PSNR check). The LOD bake gives most atlases an alpha
//!   channel whether or not any material reads it.
//! - **Always written.** A far LOD with nothing to rewrite (no embedded image) gets the plain
//!   bytes as its Bevy copy, so the explorer finds one at the key it derives from the scene id.
//!   Only a GLB whose rewrite fails (or whose embedded KTX2 will not transcode) gets none, and
//!   the explorer falls back to the plain file.
//!
//! On disk: `{scene_dir}/`[`GLB_DIR`]`/{sid}_{level}.glb`, its `.br` (when smaller) and
//! `{sid}_{level}.stats.json` ([`FarLodStats`]: download and GPU sizes). Published under
//! [`KEY_DIR`] with the same file names, unversioned like the plain GLB.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value as J;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use super::glbbr::brotli_compress;
use super::model::{transform_model_with, EmbeddedImage, NoGlbVariant};
use super::texture::TexOptions;

/// Generation of the far LOD copy's encode rules, stamped into its stats. Bump when its bytes
/// change for the same plain GLB.
pub const FAR_LOD_GEN: u32 = 1;
pub use crate::lods::{BEVY_GLB_DIR as GLB_DIR, BEVY_GLB_KEY_DIR as KEY_DIR};

/// The far LOD copy's texture rules: mips, unused alpha dropped, the scene lane's size cap.
pub fn options() -> TexOptions {
    TexOptions {
        mips: true,
        drop_unused_alpha: true,
        ..TexOptions::default()
    }
}

/// File name of the stats beside a far LOD copy.
pub fn stats_name(scene_id: &str, level: u32) -> String {
    format!("{}_{}.stats.json", scene_id.to_lowercase(), level)
}

/// Vertex and index sizes of a GLB's mesh primitives (each accessor counted once).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MeshBytes {
    pub vertices: u64,
    pub indices: u64,
    /// Vertex attributes as stored (quantized), each element padded to 4 bytes as a GPU vertex
    /// format requires.
    pub vertex_bytes: u64,
    /// Vertex attributes widened to `f32` per component, as Bevy's glTF loader holds them.
    pub vertex_bytes_f32: u64,
    pub index_bytes: u64,
}

/// Download and GPU sizes of one far LOD's Bevy copy against its plain GLB.
#[derive(Clone, Debug, Serialize)]
pub struct FarLodStats {
    pub v: u32,
    pub generator: String,
    pub scene_id: String,
    pub level: u32,
    /// The rewrite ran and changed nothing (no embedded image): the Bevy copy is the plain GLB.
    pub copied: bool,
    pub plain_bytes: u64,
    pub bevy_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub br_bytes: Option<u64>,
    /// Texture memory of the plain GLB (RGBA8 level 0, as the explorer loads it today).
    pub plain_texture_gpu_bytes: u64,
    /// Texture memory of the Bevy copy, every level.
    pub texture_gpu_bytes: u64,
    /// `texture_gpu_bytes` by format (`ktx2-bc1` / `ktx2-bc5` / `ktx2-bc7`, `rgba8` for an
    /// image that kept its original bytes).
    pub texture_gpu_by_format: BTreeMap<String, u64>,
    pub mesh: MeshBytes,
    /// `texture_gpu_bytes` + the widened vertex bytes + index bytes: what the explorer budgets.
    pub gpu_bytes: u64,
    pub images: Vec<EmbeddedImage>,
    pub encode_ms: u64,
}

/// A built far LOD copy.
#[derive(Debug)]
pub struct FarLod {
    pub glb: Vec<u8>,
    /// The Brotli copy, when smaller than `glb`.
    pub br: Option<Vec<u8>>,
    pub stats: FarLodStats,
}

fn component_bytes(component_type: u64) -> u64 {
    match component_type {
        5120 | 5121 => 1,
        5122 | 5123 => 2,
        _ => 4,
    }
}

fn type_components(ty: &str) -> u64 {
    match ty {
        "SCALAR" => 1,
        "VEC2" => 2,
        "VEC3" => 3,
        "VEC4" | "MAT2" => 4,
        "MAT3" => 9,
        "MAT4" => 16,
        _ => 1,
    }
}

/// [`MeshBytes`] of a parsed glTF.
pub fn mesh_bytes(doc: &J) -> MeshBytes {
    let accessors = doc.get("accessors").and_then(J::as_array);
    let accessor = |i: usize| accessors.and_then(|a| a.get(i));
    let count = |a: &J| a.get("count").and_then(J::as_u64).unwrap_or(0);
    let mut attrs: BTreeSet<usize> = BTreeSet::new();
    let mut indices: BTreeSet<usize> = BTreeSet::new();
    let mut out = MeshBytes::default();
    for m in doc
        .get("meshes")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
    {
        for p in m
            .get("primitives")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(a) = p.get("attributes").and_then(J::as_object) {
                attrs.extend(a.values().filter_map(J::as_u64).map(|x| x as usize));
                if let Some(pos) = a.get("POSITION").and_then(J::as_u64) {
                    out.vertices += accessor(pos as usize).map_or(0, count);
                }
            }
            if let Some(i) = p.get("indices").and_then(J::as_u64) {
                indices.insert(i as usize);
            }
        }
    }
    for a in attrs.iter().filter_map(|&i| accessor(i)) {
        let comps = type_components(a.get("type").and_then(J::as_str).unwrap_or("SCALAR"));
        let size = component_bytes(a.get("componentType").and_then(J::as_u64).unwrap_or(5126));
        out.vertex_bytes += count(a) * (comps * size).next_multiple_of(4);
        out.vertex_bytes_f32 += count(a) * comps * 4;
    }
    for a in indices.iter().filter_map(|&i| accessor(i)) {
        let size = component_bytes(a.get("componentType").and_then(J::as_u64).unwrap_or(5125));
        out.indices += count(a);
        out.index_bytes += count(a) * size;
    }
    out
}

/// Build the Bevy copy of the far LOD `plain` (a published `{sid}_{level}.glb`). `Ok(None)`
/// when it gets none: an embedded KTX2 that does not transcode. `Err` when the GLB cannot be
/// rewritten.
pub fn build(plain: &[u8], scene_id: &str, level: u32, opts: TexOptions) -> Result<Option<FarLod>> {
    let started = std::time::Instant::now();
    let outcome = transform_model_with(plain, ".glb", None, opts)?;
    let (glb, copied) = match (outcome.variant, outcome.reason) {
        (Some(v), _) => (v.glb, false),
        (None, Some(NoGlbVariant::NothingToChange)) => (plain.to_vec(), true),
        (None, _) => return Ok(None),
    };
    let br = brotli_compress(&glb)?;
    let br = (br.len() < glb.len()).then_some(br);
    let doc = crate::naming::parse_gltf_doc(&glb, ".glb").context("far lod json parse")?;
    let mesh = mesh_bytes(&doc);
    let mut by_format: BTreeMap<String, u64> = BTreeMap::new();
    let mut plain_tex = 0u64;
    for img in &outcome.images {
        plain_tex += img.original_gpu_bytes.unwrap_or(0);
        *by_format
            .entry(img.format.unwrap_or("rgba8").to_string())
            .or_default() += img.variant_gpu_bytes.unwrap_or(0);
    }
    let tex: u64 = by_format.values().sum();
    let stats = FarLodStats {
        v: FAR_LOD_GEN,
        generator: super::generator(),
        scene_id: scene_id.to_lowercase(),
        level,
        copied,
        plain_bytes: plain.len() as u64,
        bevy_bytes: glb.len() as u64,
        br_bytes: br.as_ref().map(|b| b.len() as u64),
        plain_texture_gpu_bytes: plain_tex,
        texture_gpu_bytes: tex,
        texture_gpu_by_format: by_format,
        gpu_bytes: tex + mesh.vertex_bytes_f32 + mesh.index_bytes,
        mesh,
        images: outcome.images,
        encode_ms: started.elapsed().as_millis() as u64,
    };
    Ok(Some(FarLod { glb, br, stats }))
}

/// Write a built copy under `{scene_dir}/`[`GLB_DIR`]: the GLB, its `.br` and its stats.
/// Returns the GLB's path.
pub fn write(scene_dir: &Path, far: &FarLod) -> Result<PathBuf> {
    let dir = scene_dir.join(GLB_DIR);
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let (sid, level) = (&far.stats.scene_id, far.stats.level);
    let path = dir.join(crate::lods::published_glb_name(sid, level));
    crate::lods::write_atomic(&path, &far.glb)?;
    let br_path = super::glbbr::br_sidecar(&path);
    match &far.br {
        Some(br) => crate::lods::write_atomic(&br_path, br)?,
        None => {
            let _ = std::fs::remove_file(&br_path);
        }
    }
    let stats = serde_json::to_vec_pretty(&far.stats)?;
    crate::lods::write_atomic(&dir.join(stats_name(sid, level)), &stats)?;
    Ok(path)
}

/// Build and write the Bevy copy of the plain far LOD GLB at `plain_path`. A copy that cannot
/// be built is logged and skipped: the explorer falls back to the plain GLB, and the LOD
/// build it follows stands.
pub fn write_from_plain(
    scene_dir: &Path,
    scene_id: &str,
    level: u32,
    plain_path: &Path,
) -> Option<FarLodStats> {
    let result = std::fs::read(plain_path)
        .with_context(|| format!("read {}", plain_path.display()))
        .and_then(|plain| build(&plain, scene_id, level, options()));
    match result {
        Ok(Some(far)) => match write(scene_dir, &far) {
            Ok(_) => Some(far.stats),
            Err(e) => {
                tracing::warn!(scene = scene_id, level, error = %format!("{e:#}"), "bevy far lod: write failed");
                None
            }
        },
        Ok(None) => {
            tracing::warn!(
                scene = scene_id,
                level,
                "bevy far lod: an embedded KTX2 does not transcode; no Bevy copy"
            );
            None
        }
        Err(e) => {
            tracing::warn!(scene = scene_id, level, error = %format!("{e:#}"), "bevy far lod: build failed");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::model::{AlphaClass, LodImage, LodMaterial, LodModel, LodPrimitive};
    use serde_json::json;

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
        let far = build(&plain, "BafkScene", 1, options()).unwrap().unwrap();
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
        let rows = &far.stats.images;
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

        // Brotli round-trips; stats add up.
        let br = far.br.as_ref().expect("br smaller than the glb");
        assert_eq!(super::super::glbbr::brotli_decompress(br).unwrap(), far.glb);
        let s = &far.stats;
        assert_eq!(s.scene_id, "bafkscene");
        assert!(!s.copied);
        assert_eq!(s.plain_bytes, plain.len() as u64);
        assert_eq!(s.br_bytes, Some(br.len() as u64));
        assert_eq!(s.plain_texture_gpu_bytes, (64 * 64 + 32 * 32) * 4);
        assert_eq!(
            s.texture_gpu_by_format["ktx2-bc1"],
            super::super::texture::chain_gpu_bytes(crate::ktx2_write::BcFormat::Bc1Rgb, 64, 64, 7)
        );
        assert_eq!(
            s.texture_gpu_bytes,
            s.texture_gpu_by_format.values().sum::<u64>()
        );
        assert!(s.texture_gpu_bytes < s.plain_texture_gpu_bytes);
        assert_eq!(s.mesh, mesh_bytes(&p));
        assert_eq!(s.mesh.vertices, 8);
        assert_eq!(s.mesh.indices, 12);
        assert_eq!(
            s.gpu_bytes,
            s.texture_gpu_bytes + s.mesh.vertex_bytes_f32 + s.mesh.index_bytes
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
        let far = build(&plain, "bafkscene", 1, opts).unwrap().unwrap();
        assert_eq!(far.stats.images[0].format, Some("ktx2-bc7"));
        assert!(!far.stats.images[0].alpha_dropped);
        assert_eq!(far.stats.images[0].mips, Some(1));
    }

    #[test]
    fn far_lod_without_images_is_copied_and_published_beside_the_plain_glb() {
        let plain = published_far_lod(false);
        let far = build(&plain, "bafkscene", 1, options()).unwrap().unwrap();
        assert!(far.stats.copied);
        assert_eq!(far.glb, plain);
        assert_eq!(far.stats.texture_gpu_bytes, 0);

        let base = std::env::temp_dir().join(format!("farlod-pub-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let scene = base.join("bafkscene");
        let plain_dir = scene.join(crate::lods::PUBLISHED_GLB_DIR);
        std::fs::create_dir_all(&plain_dir).unwrap();
        std::fs::write(plain_dir.join("bafkscene_1.glb"), &plain).unwrap();
        let path = write(&scene, &far).unwrap();
        assert_eq!(path, scene.join("lods-bevy/lods/bafkscene_1.glb"));

        let objs = crate::lods::published_objects(&scene, &[1]);
        let keys: Vec<&str> = objs.iter().map(|o| o.key.as_str()).collect();
        let mut want = vec![
            "LOD/lods-unity/lods/bafkscene_1.glb",
            "LOD/lods-bevy/lods/bafkscene_1.glb",
        ];
        if far.br.is_some() {
            want.push("LOD/lods-bevy/lods/bafkscene_1.glb.br");
        }
        want.push("LOD/lods-bevy/lods/bafkscene_1.stats.json");
        assert_eq!(keys, want);
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
