use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicU64, Ordering};

use super::model::{self, AlphaClass, LodMaterial, LodModel, LodPrimitive};
use super::placements::Placement;
use crate::scene::TexTransform;

#[cfg(not(target_arch = "wasm32"))]
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_cached(
    client: &crate::catalyst::CatalystClient,
    cache_dir: Option<&Path>,
    hash: &str,
) -> Result<Vec<u8>> {
    if let Some(dir) = cache_dir {
        if let Ok(b) = std::fs::read(dir.join(hash)) {
            return Ok(b);
        }
    }
    let bytes = client
        .fetch_content(hash)
        .with_context(|| format!("fetch content {hash}"))?;
    if let Some(dir) = cache_dir {
        let _ = std::fs::create_dir_all(dir);
        let tmp = dir.join(format!(
            ".{hash}.{}.{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join(hash));
        }
    }
    Ok(bytes)
}

pub fn sanitize_glb_json_padding(bytes: &mut [u8]) {
    if bytes.len() < 12 || &bytes[0..4] != b"glTF" {
        return;
    }
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let clen = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        let ctype = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]);
        let data_start = pos + 8;
        let Some(data_end) = data_start.checked_add(clen).filter(|&e| e <= bytes.len()) else {
            return;
        };
        if ctype == 0x4E4F_534A {
            let mut i = data_end;
            while i > data_start && bytes[i - 1] == 0 {
                bytes[i - 1] = b' ';
                i -= 1;
            }
            return;
        }
        pos = data_end;
    }
}

pub(crate) fn resolve_placement_hash(
    p: &Placement,
    by_file: &HashMap<String, String>,
) -> Result<String> {
    if let Some(h) = &p.glb_hash {
        return Ok(h.clone());
    }
    if let Some(f) = &p.glb_file {
        if let Some(h) = by_file.get(&f.to_lowercase()) {
            return Ok(h.clone());
        }
        bail!("placement src {f:?} not in entity content map");
    }
    bail!("placement has neither glb_hash nor glb_file");
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct MatKey {
    base_color: [u64; 4],
    class: AlphaClass,
    cutoff: u64,
    double_sided: bool,
    image: Option<usize>,
    emissive: [u64; 3],
    emissive_image: Option<usize>,
    metallic: u64,
    roughness: u64,
    mr_image: Option<usize>,
    normal_image: Option<usize>,
    emissive_strength: u64,
}

struct Prepared {
    scene: crate::scene::Scene,
    mats: Vec<usize>,
    bakes: Vec<Option<TexTransform>>,
    fallback: Option<usize>,
    name: String,
}

#[derive(Default)]
struct Counters {
    kept: usize,
    dropped_collider: usize,
    skipped_skin: usize,
}

fn unique_name(base: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let mut n = 1usize;
    loop {
        let cand = format!("{base}_{n}");
        if used.insert(cand.clone()) {
            return cand;
        }
        n += 1;
    }
}

fn intern_material(
    key: MatKey,
    name: &str,
    mat_by_key: &mut HashMap<MatKey, usize>,
    used_names: &mut HashSet<String>,
    model: &mut LodModel,
) -> usize {
    if let Some(&i) = mat_by_key.get(&key) {
        return i;
    }
    let i = model.materials.len();
    model.materials.push(LodMaterial {
        name: unique_name(name, used_names),
        class: key.class,
        base_color: [
            f64::from_bits(key.base_color[0]),
            f64::from_bits(key.base_color[1]),
            f64::from_bits(key.base_color[2]),
            f64::from_bits(key.base_color[3]),
        ],
        cutoff: f64::from_bits(key.cutoff),
        image: key.image,
        double_sided: key.double_sided,
        emissive: [
            f64::from_bits(key.emissive[0]),
            f64::from_bits(key.emissive[1]),
            f64::from_bits(key.emissive[2]),
        ],
        emissive_image: key.emissive_image,
        metallic: f64::from_bits(key.metallic),
        roughness: f64::from_bits(key.roughness),
        mr_image: key.mr_image,
        normal_image: key.normal_image,
        emissive_strength: f64::from_bits(key.emissive_strength),
    });
    mat_by_key.insert(key, i);
    i
}

fn default_mat_key() -> MatKey {
    MatKey {
        base_color: [1f64.to_bits(); 4],
        class: AlphaClass::Opaque,
        cutoff: 0.5f64.to_bits(),
        double_sided: false,
        image: None,
        emissive: [0f64.to_bits(); 3],
        emissive_image: None,
        metallic: 0f64.to_bits(),
        roughness: 1f64.to_bits(),
        mr_image: None,
        normal_image: None,
        emissive_strength: 1f64.to_bits(),
    }
}

fn subtree_prim_count(scene: &crate::scene::Scene, idx: usize) -> usize {
    if idx >= scene.nodes.len() {
        return 0;
    }
    let node = &scene.nodes[idx];
    let mut n = node.primitives.len();
    for &c in &node.children {
        n += subtree_prim_count(scene, c);
    }
    n
}

fn walk_instance(
    prep: &Prepared,
    idx: usize,
    parent: &[f64; 16],
    model: &mut LodModel,
    counters: &mut Counters,
) {
    let scene = &prep.scene;
    if idx >= scene.nodes.len() {
        return;
    }
    let node = &scene.nodes[idx];
    if node.is_collider || node.name_is_collider {
        counters.dropped_collider += subtree_prim_count(scene, idx);
        return;
    }
    let local = model::mat4_from_trs(node.translation, node.rotation, node.scale);
    let world = model::mat4_mul(parent, &local);
    let det = model::det3(&world);
    let nmat = model::inv_transpose3(&world);
    for prim in &node.primitives {
        if prim.positions.is_empty() || prim.indices.len() < 3 {
            continue;
        }
        if prim.skin_index.is_some() || prim.joints.is_some() {
            counters.skipped_skin += 1;
            continue;
        }
        let mut positions = Vec::with_capacity(prim.positions.len());
        for p in &prim.positions {
            let w = model::mul_point(&world, *p);
            positions.push([(-w[0]) as f32, w[1] as f32, w[2] as f32]);
        }
        let mut normals = Vec::with_capacity(prim.normals.len());
        for n in &prim.normals {
            let v = match &nmat {
                Some(m) => model::mul_normal(m, *n),
                None => *n,
            };
            let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
            let v = if len > 1e-12 {
                [v[0] / len, v[1] / len, v[2] / len]
            } else {
                v
            };
            normals.push([(-v[0]) as f32, v[1] as f32, v[2] as f32]);
        }
        if normals.len() != positions.len() {
            normals.resize(positions.len(), [0.0, 0.0, 1.0]);
        }
        let bake = prim
            .material_index
            .and_then(|mi| prep.bakes.get(mi))
            .and_then(|b| *b);
        let mut uvs: Vec<[f32; 2]> = match prim.uvs.as_ref() {
            Some(uv) => uv
                .iter()
                .map(|u| {
                    let b = match &bake {
                        Some(x) => [
                            u[0] * x.scale[0] + x.offset[0],
                            u[1] * x.scale[1] + x.offset[1],
                        ],
                        None => [u[0], u[1]],
                    };
                    [b[0] as f32, (1.0 - b[1]) as f32]
                })
                .collect(),
            None => vec![[0.0, 0.0]; positions.len()],
        };
        if uvs.len() != positions.len() {
            uvs.resize(positions.len(), [0.0, 0.0]);
        }
        let mut indices = Vec::with_capacity(prim.indices.len());
        for tri in prim.indices.chunks_exact(3) {
            if det >= 0.0 {
                indices.extend_from_slice(&[tri[0], tri[2], tri[1]]);
            } else {
                indices.extend_from_slice(&[tri[0], tri[1], tri[2]]);
            }
        }
        let material = match prim.material_index {
            Some(mi) if mi < prep.mats.len() => prep.mats[mi],
            _ => prep.fallback.expect("fallback material interned"),
        };
        counters.kept += 1;
        model.primitives.push(LodPrimitive {
            positions,
            normals,
            uvs,
            indices,
            material,
            ..Default::default()
        });
    }
    for &c in &node.children {
        walk_instance(prep, c, &world, model, counters);
    }
}

fn scene_needs_fallback(scene: &crate::scene::Scene) -> bool {
    scene.nodes.iter().any(|n| {
        n.primitives.iter().any(|p| {
            !p.positions.is_empty()
                && p.indices.len() >= 3
                && p.skin_index.is_none()
                && p.joints.is_none()
                && !matches!(p.material_index, Some(mi) if mi < scene.materials.len())
        })
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn assemble(
    client: &crate::catalyst::CatalystClient,
    scene: &crate::catalyst::Scene,
    placements: &[Placement],
    level: u32,
    cache_dir: Option<&Path>,
    lane: model::MatLane,
) -> Result<LodModel> {
    let by_file = scene.content_by_file();
    let mut file_by_hash: HashMap<&str, &str> = HashMap::new();
    for c in &scene.content {
        file_by_hash
            .entry(c.hash.as_str())
            .or_insert(c.file.as_str());
    }
    let fetch = |hash: &str| fetch_cached(client, cache_dir, hash);
    assemble_from(
        &format!("{}_{}", scene.entity_id.to_lowercase(), level),
        &by_file,
        &file_by_hash,
        placements,
        &fetch,
        lane,
    )
}

pub fn assemble_from(
    root_name: &str,
    by_file: &HashMap<String, String>,
    file_by_hash: &HashMap<&str, &str>,
    placements: &[Placement],
    fetch: &(dyn Fn(&str) -> Result<Vec<u8>> + Sync),
    lane: model::MatLane,
) -> Result<LodModel> {
    if placements.is_empty() {
        bail!("assemble: no placements");
    }

    let mut placed: Vec<(usize, &Placement, String)> = Vec::with_capacity(placements.len());
    let mut unresolved: Vec<String> = Vec::new();
    for (i, p) in placements.iter().enumerate() {
        match resolve_placement_hash(p, by_file) {
            Ok(h) => placed.push((i, p, h)),
            Err(e) => unresolved.push(format!("placement {i}: {e}")),
        }
    }
    if placed.is_empty() {
        bail!(
            "assemble: all {} placement(s) unresolvable:\n{}",
            unresolved.len(),
            unresolved.join("\n")
        );
    }
    for u in &unresolved {
        eprintln!("assemble: skipping unresolvable {u}");
    }

    let mut uniq: Vec<String> = Vec::new();
    {
        let mut seen: HashMap<&str, ()> = HashMap::new();
        for (_, _, h) in &placed {
            if seen.insert(h.as_str(), ()).is_none() {
                uniq.push(h.clone());
            }
        }
    }

    let parsed: Vec<(String, Result<(crate::scene::Scene, String)>)> = uniq
        .par_iter()
        .map(|hash| {
            let r = (|| -> Result<(crate::scene::Scene, String)> {
                let mut bytes = fetch(hash)?;
                sanitize_glb_json_padding(&mut bytes);
                let src_name = file_by_hash
                    .get(hash.as_str())
                    .map(|f| f.to_string())
                    .unwrap_or_else(|| hash.clone());
                let ext = if src_name.to_lowercase().ends_with(".gltf") {
                    ".gltf"
                } else {
                    ".glb"
                };
                let resolve_fn = |uri: &str| -> Option<Vec<u8>> {
                    let h = crate::naming::uri_content_hash(uri, &src_name, by_file)?;
                    fetch(h).ok()
                };
                let parsed = crate::gltf::parse(&bytes, ext, Some(&resolve_fn), false, true)
                    .with_context(|| format!("parse {src_name}"))?;
                Ok((parsed, src_name))
            })();
            (hash.clone(), r)
        })
        .collect();

    let mut model = LodModel {
        root_name: root_name.to_string(),
        ..Default::default()
    };
    let mut image_by_hash: HashMap<String, usize> = HashMap::new();
    let mut mat_by_key: HashMap<MatKey, usize> = HashMap::new();
    let mut used_names: HashSet<String> = HashSet::new();
    let mut prepared: HashMap<String, Prepared> = HashMap::new();
    let mut parse_errs: Vec<String> = Vec::new();

    for (hash, r) in parsed {
        let (mut src, name) = match r {
            Ok(x) => x,
            Err(e) => {
                parse_errs.push(format!("{hash}: {e:#}"));
                continue;
            }
        };
        let mut image_slot: Vec<Option<Option<usize>>> = vec![None; src.images.len()];
        let mut mats = Vec::with_capacity(src.materials.len());
        let mut bakes = Vec::with_capacity(src.materials.len());
        for m in &src.materials {
            let mut intern = |tr: Option<crate::scene::TexRef>| match tr {
                Some(tr) if tr.image < src.images.len() => {
                    if image_slot[tr.image].is_none() {
                        image_slot[tr.image] = Some(model::intern_image(
                            &src,
                            tr.image,
                            &mut image_by_hash,
                            &mut model,
                        ));
                    }
                    image_slot[tr.image].unwrap()
                }
                _ => None,
            };
            let image = intern(m.base_color_image);
            let mut key = default_mat_key();
            let (eff, emissive, emissive_image) = if lane.raw_materials {
                let eimg = if m.emissive != [0.0; 3] {
                    intern(m.emissive_image)
                } else {
                    None
                };
                key.metallic = m.metallic.to_bits();
                key.roughness = m.roughness.to_bits();
                key.mr_image = intern(m.metallic_roughness_image);
                key.normal_image = intern(m.normal_image);
                key.emissive_strength = m.emissive_strength.to_bits();
                (m.base_color, m.emissive, eimg)
            } else if lane.emissive_channel {
                let e = model::scaled_emissive(m);
                let eimg = if e != [0.0; 3] {
                    intern(m.emissive_image)
                } else {
                    None
                };
                (m.base_color, e, eimg)
            } else {
                (
                    model::fold_emissive(m.base_color, m.emissive),
                    [0.0; 3],
                    None,
                )
            };
            key.base_color = [
                eff[0].to_bits(),
                eff[1].to_bits(),
                eff[2].to_bits(),
                eff[3].to_bits(),
            ];
            key.class = AlphaClass::from_alpha_mode(&m.alpha_mode);
            key.cutoff = m.alpha_cutoff.to_bits();
            key.double_sided = m.double_sided;
            key.image = image;
            key.emissive = [
                emissive[0].to_bits(),
                emissive[1].to_bits(),
                emissive[2].to_bits(),
            ];
            key.emissive_image = emissive_image;
            mats.push(intern_material(
                key,
                &m.name,
                &mut mat_by_key,
                &mut used_names,
                &mut model,
            ));
            bakes.push(m.tex_transforms.get("_BaseMap").copied());
        }
        let fallback = if scene_needs_fallback(&src) {
            Some(intern_material(
                default_mat_key(),
                "default",
                &mut mat_by_key,
                &mut used_names,
                &mut model,
            ))
        } else {
            None
        };
        src.images = Vec::new();
        src.image_bytes = Vec::new();
        prepared.insert(
            hash,
            Prepared {
                scene: src,
                mats,
                bakes,
                fallback,
                name,
            },
        );
    }
    if !parse_errs.is_empty() {
        bail!(
            "assemble: {} asset(s) failed to fetch/parse:\n{}",
            parse_errs.len(),
            parse_errs.join("\n")
        );
    }

    let mut counters = Counters::default();
    for &(pi, p, ref hash) in &placed {
        let prep = &prepared[hash.as_str()];
        let parent = model::mat4_from_trs(p.position, p.rotation, p.scale);
        let prims_before = model.primitives.len();
        let tris_before = model.total_tris();
        for &r in &prep.scene.root_nodes {
            walk_instance(prep, r, &parent, &mut model, &mut counters);
        }
        model.log.push(format!(
            "placement {pi} {hash} ({}): prims={} tris={}",
            prep.name,
            model.primitives.len() - prims_before,
            model.total_tris() - tris_before
        ));
    }
    for u in &unresolved {
        model.log.push(format!("skipped unresolvable {u}"));
    }
    model.log.push(format!(
        "summary: instances={} unique_glbs={} prims_kept={} prims_collider_dropped={} prims_skinned_skipped={}",
        placed.len(),
        uniq.len(),
        counters.kept,
        counters.dropped_collider,
        counters.skipped_skin
    ));
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::emit::emit_glb;
    use crate::lodgen::model::LodImage;
    use serde_json::{json, Value};

    fn dummy_client() -> crate::catalyst::CatalystClient {
        crate::catalyst::CatalystClient::new("http://127.0.0.1:9")
    }

    fn entity(content: &[(&str, &str)]) -> crate::catalyst::Scene {
        crate::catalyst::Scene {
            entity_id: "BafTestEntity".to_string(),
            entity_type: "scene".to_string(),
            pointers: Vec::new(),
            content: content
                .iter()
                .map(|(f, h)| crate::catalyst::ContentEntry {
                    file: f.to_string(),
                    hash: h.to_string(),
                })
                .collect(),
            metadata: serde_json::json!({}),
        }
    }

    fn temp_cache(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "abgen-lod-assemble-test-{tag}-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn chunks(glb: &[u8]) -> (Value, Vec<u8>) {
        let jlen = u32::from_le_bytes(glb[12..16].try_into().unwrap()) as usize;
        let json: Value = serde_json::from_slice(&glb[20..20 + jlen]).unwrap();
        let bstart = 20 + jlen;
        let blen = u32::from_le_bytes(glb[bstart..bstart + 4].try_into().unwrap()) as usize;
        (json, glb[bstart + 8..bstart + 8 + blen].to_vec())
    }

    fn rebuild(json: &Value, bin: &[u8]) -> Vec<u8> {
        let mut jb = serde_json::to_vec(json).unwrap();
        while !jb.len().is_multiple_of(4) {
            jb.push(b' ');
        }
        let mut bb = bin.to_vec();
        while !bb.len().is_multiple_of(4) {
            bb.push(0);
        }
        let total = 12 + 8 + jb.len() + 8 + bb.len();
        let mut out = Vec::with_capacity(total);
        out.extend_from_slice(b"glTF");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(total as u32).to_le_bytes());
        out.extend_from_slice(&(jb.len() as u32).to_le_bytes());
        out.extend_from_slice(b"JSON");
        out.extend_from_slice(&jb);
        out.extend_from_slice(&(bb.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0x42, 0x49, 0x4E, 0x00]);
        out.extend_from_slice(&bb);
        out
    }

    fn base_material() -> LodMaterial {
        LodMaterial {
            name: "m".to_string(),
            class: AlphaClass::Opaque,
            base_color: [1.0, 1.0, 1.0, 1.0],
            cutoff: 0.5,
            image: None,
            double_sided: false,
            ..Default::default()
        }
    }

    fn tri_glb() -> Vec<u8> {
        emit_glb(&LodModel {
            root_name: "tri".to_string(),
            primitives: vec![LodPrimitive {
                positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]],
                indices: vec![0, 1, 2],
                material: 0,
                ..Default::default()
            }],
            materials: vec![base_material()],
            images: Vec::new(),
            log: Vec::new(),
        })
        .unwrap()
    }

    fn cube_model() -> LodModel {
        let axes: [([f32; 3], [f32; 3]); 6] = [
            ([0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            ([0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0]),
            ([1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),
            ([1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),
            ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0]),
        ];
        let cross = |a: [f32; 3], b: [f32; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let mut positions = Vec::new();
        let mut normals = Vec::new();
        let mut uvs = Vec::new();
        let mut indices = Vec::new();
        for (u, v) in axes {
            let n = cross(u, v);
            let base = positions.len() as u32;
            for (su, sv) in [(-0.5f32, -0.5f32), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)] {
                positions.push([
                    n[0] * 0.5 + u[0] * su + v[0] * sv,
                    n[1] * 0.5 + u[1] * su + v[1] * sv,
                    n[2] * 0.5 + u[2] * su + v[2] * sv,
                ]);
                normals.push(n);
                uvs.push([su + 0.5, sv + 0.5]);
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        LodModel {
            root_name: "cube".to_string(),
            primitives: vec![LodPrimitive {
                positions,
                normals,
                uvs,
                indices,
                material: 0,
                ..Default::default()
            }],
            materials: vec![base_material()],
            images: Vec::new(),
            log: Vec::new(),
        }
    }

    fn signed_volume(model: &LodModel) -> f64 {
        let mut v = 0.0f64;
        for prim in &model.primitives {
            for tri in prim.indices.chunks_exact(3) {
                let a = prim.positions[tri[0] as usize].map(|x| x as f64);
                let b = prim.positions[tri[1] as usize].map(|x| x as f64);
                let c = prim.positions[tri[2] as usize].map(|x| x as f64);
                v += a[0] * (b[1] * c[2] - b[2] * c[1])
                    + a[1] * (b[2] * c[0] - b[0] * c[2])
                    + a[2] * (b[0] * c[1] - b[1] * c[0]);
            }
        }
        v / 6.0
    }

    fn quat_rotate(q: [f64; 4], v: [f64; 3]) -> [f64; 3] {
        let u = [q[0], q[1], q[2]];
        let s = q[3];
        let cross = |a: [f64; 3], b: [f64; 3]| {
            [
                a[1] * b[2] - a[2] * b[1],
                a[2] * b[0] - a[0] * b[2],
                a[0] * b[1] - a[1] * b[0],
            ]
        };
        let uv = cross(u, v);
        let uuv = cross(u, uv);
        [
            v[0] + 2.0 * (s * uv[0] + uuv[0]),
            v[1] + 2.0 * (s * uv[1] + uuv[1]),
            v[2] + 2.0 * (s * uv[2] + uuv[2]),
        ]
    }

    fn stage(cache: &Path, hash: &str, bytes: &[u8]) {
        std::fs::write(cache.join(hash), bytes).unwrap();
    }

    fn summary_line(model: &LodModel) -> &str {
        model
            .log
            .iter()
            .rev()
            .find(|l| l.starts_with("summary:"))
            .unwrap()
    }

    #[test]
    fn collider_strip_sibling_survives() {
        let glb = tri_glb();
        let (mut json, bin) = chunks(&glb);
        json["nodes"] = json!([
            {"children": [1, 2], "name": "root"},
            {"mesh": 0, "name": "foo_collider"},
            {"mesh": 0, "name": "keeper"}
        ]);
        json["scenes"] = json!([{"nodes": [0]}]);
        let patched = rebuild(&json, &bin);
        let cache = temp_cache("collider");
        stage(&cache, "hcollider", &patched);
        let model = assemble(
            &dummy_client(),
            &entity(&[("m.glb", "hcollider")]),
            &[Placement {
                glb_hash: Some("hcollider".to_string()),
                ..Default::default()
            }],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert_eq!(model.total_tris(), 1);
        assert_eq!(model.primitives.len(), 1);
        let s = summary_line(&model);
        assert!(s.contains("prims_kept=1"), "{s}");
        assert!(s.contains("prims_collider_dropped=1"), "{s}");
    }

    #[test]
    fn trs_bake_lands_at_unity_coords() {
        let cube = cube_model();
        let glb = emit_glb(&cube).unwrap();
        let cache = temp_cache("trs");
        stage(&cache, "hcube", &glb);
        let s2 = std::f64::consts::FRAC_1_SQRT_2;
        let placement = Placement {
            glb_hash: Some("hcube".to_string()),
            glb_file: None,
            position: [3.0, 4.0, 5.0],
            rotation: [0.0, s2, 0.0, s2],
            scale: [1.0, 1.0, 1.0],
        };
        let model = assemble(
            &dummy_client(),
            &entity(&[("cube.glb", "hcube")]),
            std::slice::from_ref(&placement),
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert_eq!(model.primitives.len(), 1);
        let out = &model.primitives[0];
        assert_eq!(out.positions.len(), cube.primitives[0].positions.len());
        for (src, got) in cube.primitives[0].positions.iter().zip(&out.positions) {
            let lh = [-(src[0] as f64), src[1] as f64, src[2] as f64];
            let rot = quat_rotate(placement.rotation, lh);
            let unity = [
                rot[0] + placement.position[0],
                rot[1] + placement.position[1],
                rot[2] + placement.position[2],
            ];
            let builder_flipped = [-(got[0] as f64), got[1] as f64, got[2] as f64];
            for i in 0..3 {
                assert!(
                    (builder_flipped[i] - unity[i]).abs() < 1e-5,
                    "vertex {src:?}: unity expected {unity:?} got {builder_flipped:?}"
                );
            }
        }
    }

    #[test]
    fn placement_frame_is_local_no_mirror_no_offset() {
        let cube = cube_model();
        let glb = emit_glb(&cube).unwrap();
        let cache = temp_cache("frame");
        stage(&cache, "hcube", &glb);
        let model = assemble(
            &dummy_client(),
            &entity(&[("cube.glb", "hcube")]),
            &[Placement {
                glb_hash: Some("hcube".to_string()),
                position: [88.8968276977539, 0.2825070321559906, 13.414548873901368],
                ..Default::default()
            }],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        let (mn, mx) = model.bounds();
        let center = [
            (mn[0] + mx[0]) as f64 / 2.0,
            (mn[1] + mx[1]) as f64 / 2.0,
            (mn[2] + mx[2]) as f64 / 2.0,
        ];
        assert!((center[0] - -88.8968276977539).abs() < 1e-4, "{center:?}");
        assert!((center[1] - 0.2825070321559906).abs() < 1e-4, "{center:?}");
        assert!((center[2] - 13.414548873901368).abs() < 1e-4, "{center:?}");
    }

    #[test]
    fn mirror_scale_keeps_outward_winding() {
        let cube = cube_model();
        let glb = emit_glb(&cube).unwrap();
        let cache = temp_cache("mirror");
        stage(&cache, "hcube", &glb);
        let ent = entity(&[("cube.glb", "hcube")]);
        let plain = assemble(
            &dummy_client(),
            &ent,
            &[Placement {
                glb_hash: Some("hcube".to_string()),
                ..Default::default()
            }],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert!((signed_volume(&plain) - 1.0).abs() < 1e-4);
        let mirrored = assemble(
            &dummy_client(),
            &ent,
            &[Placement {
                glb_hash: Some("hcube".to_string()),
                scale: [-1.0, 1.0, 1.0],
                ..Default::default()
            }],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert!(
            (signed_volume(&mirrored) - 1.0).abs() < 1e-4,
            "signed volume {}",
            signed_volume(&mirrored)
        );
    }

    fn tiny_png(seed: u8) -> Vec<u8> {
        let mut img = image::RgbaImage::new(2, 2);
        for (i, p) in img.pixels_mut().enumerate() {
            *p = image::Rgba([seed, i as u8 * 40, 255 - seed, 255]);
        }
        let mut cur = std::io::Cursor::new(Vec::new());
        img.write_to(&mut cur, image::ImageFormat::Png).unwrap();
        cur.into_inner()
    }

    fn textured_tri_glb(mat_name: &str, marker: f32, png: &[u8]) -> Vec<u8> {
        emit_glb(&LodModel {
            root_name: "t".to_string(),
            primitives: vec![LodPrimitive {
                positions: vec![[marker, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.25, 0.5], [1.0, 0.0], [0.0, 1.0]],
                indices: vec![0, 1, 2],
                material: 0,
                ..Default::default()
            }],
            materials: vec![LodMaterial {
                name: mat_name.to_string(),
                image: Some(0),
                ..base_material()
            }],
            images: vec![LodImage {
                bytes: png.to_vec(),
                mime: "image/png".to_string(),
            }],
            log: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn uv_passthrough_and_tex_transform_bake() {
        let png = tiny_png(7);
        let glb = textured_tri_glb("m", 0.0, &png);
        let (mut json, bin) = chunks(&glb);
        json["materials"][0]["pbrMetallicRoughness"]["baseColorTexture"]["extensions"] =
            json!({"KHR_texture_transform": {"offset": [0.1, 0.2], "scale": [2.0, 3.0]}});
        let with_xform = rebuild(&json, &bin);
        let cache = temp_cache("uv");
        stage(&cache, "hplain", &glb);
        stage(&cache, "hxform", &with_xform);
        let ent = entity(&[("a.glb", "hplain"), ("b.glb", "hxform")]);
        let mk = |h: &str| Placement {
            glb_hash: Some(h.to_string()),
            ..Default::default()
        };
        let plain = assemble(
            &dummy_client(),
            &ent,
            &[mk("hplain")],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        let uv = plain.primitives[0].uvs[0];
        assert!(
            (uv[0] - 0.25).abs() < 1e-6 && (uv[1] - 0.5).abs() < 1e-6,
            "{uv:?}"
        );
        let baked = assemble(
            &dummy_client(),
            &ent,
            &[mk("hxform")],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        let uv = baked.primitives[0].uvs[0];
        assert!(
            (uv[0] - 0.6).abs() < 1e-5 && (uv[1] - 1.7).abs() < 1e-5,
            "expected baked (0.6, 1.7) got {uv:?}"
        );
    }

    #[test]
    fn material_image_dedupe_across_glbs() {
        let png = tiny_png(9);
        let a = textured_tri_glb("matA", 0.0, &png);
        let b = textured_tri_glb("matB", 5.0, &png);
        assert_ne!(a, b);
        let cache = temp_cache("dedupe");
        stage(&cache, "ha", &a);
        stage(&cache, "hb", &b);
        let ent = entity(&[("a.glb", "ha"), ("b.glb", "hb")]);
        let model = assemble(
            &dummy_client(),
            &ent,
            &[
                Placement {
                    glb_hash: Some("ha".to_string()),
                    ..Default::default()
                },
                Placement {
                    glb_hash: Some("hb".to_string()),
                    ..Default::default()
                },
            ],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert_eq!(model.images.len(), 1);
        assert_eq!(model.materials.len(), 1);
        assert_eq!(model.materials[0].name, "matA");
        assert_eq!(model.primitives.len(), 2);
        assert!(model.primitives.iter().all(|p| p.material == 0));
    }

    #[test]
    fn raw_materials_lane_keeps_source_truth_default_lane_normalizes() {
        let base_png = tiny_png(4);
        let mr_png = tiny_png(5);
        let nrm_png = tiny_png(6);
        let src = emit_glb(&LodModel {
            root_name: "truth".to_string(),
            primitives: vec![LodPrimitive {
                positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
                normals: vec![[0.0, 0.0, 1.0]; 3],
                uvs: vec![[0.25, 0.5], [1.0, 0.0], [0.0, 1.0]],
                indices: vec![0, 1, 2],
                material: 0,
                ..Default::default()
            }],
            materials: vec![LodMaterial {
                name: "Purple".to_string(),
                base_color: [0.1854, 0.0007, 0.6921, 1.0],
                image: Some(0),
                double_sided: true,
                roughness: 0.9,
                mr_image: Some(1),
                normal_image: Some(2),
                emissive: [0.25, 0.5, 0.75],
                emissive_strength: 2.0,
                ..base_material()
            }],
            images: [&base_png, &mr_png, &nrm_png]
                .iter()
                .map(|b| LodImage {
                    bytes: b.to_vec(),
                    mime: "image/png".to_string(),
                })
                .collect(),
            log: Vec::new(),
        })
        .unwrap();
        // drop the explicit metallicFactor: the spec-default-1.0 case the
        // normalized lane has been rewriting to 0
        let (mut json, bin) = chunks(&src);
        json["materials"][0]["pbrMetallicRoughness"]
            .as_object_mut()
            .unwrap()
            .remove("metallicFactor");
        let src = rebuild(&json, &bin);

        let cache = temp_cache("rawmat");
        stage(&cache, "htruth", &src);
        let ent = entity(&[("truth.glb", "htruth")]);
        let place = Placement {
            glb_hash: Some("htruth".to_string()),
            ..Default::default()
        };

        let raw = assemble(
            &dummy_client(),
            &ent,
            std::slice::from_ref(&place),
            1,
            Some(&cache),
            model::MatLane {
                raw_materials: true,
                ..Default::default()
            },
        )
        .unwrap();
        let m = &raw.materials[0];
        assert_eq!(m.metallic, 1.0);
        assert_eq!(m.roughness, 0.9);
        assert_eq!(m.base_color, [0.1854, 0.0007, 0.6921, 1.0]);
        assert_eq!(m.emissive, [0.25, 0.5, 0.75]);
        assert_eq!(m.emissive_strength, 2.0);
        assert!(m.double_sided);
        assert!(m.mr_image.is_some());
        assert!(m.normal_image.is_some());
        assert_eq!(raw.images.len(), 3);

        let glb = emit_glb(&raw).unwrap();
        let (json, _) = chunks(&glb);
        let mj = &json["materials"][0];
        assert_eq!(
            mj["pbrMetallicRoughness"]["metallicFactor"],
            serde_json::json!(1.0)
        );
        assert!(mj["pbrMetallicRoughness"]
            .get("metallicRoughnessTexture")
            .is_some());
        assert!(mj.get("normalTexture").is_some());
        assert_eq!(mj["doubleSided"], serde_json::json!(true));
        assert_eq!(
            mj["extensions"]["KHR_materials_emissive_strength"]["emissiveStrength"],
            serde_json::json!(2.0)
        );

        let plain = assemble(
            &dummy_client(),
            &ent,
            std::slice::from_ref(&place),
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        let m = &plain.materials[0];
        assert_eq!(m.metallic, 0.0);
        assert_eq!(m.roughness, 1.0);
        assert_eq!(m.mr_image, None);
        assert_eq!(m.normal_image, None);
        assert_eq!(m.emissive_strength, 1.0);
        assert_eq!(
            m.base_color,
            model::fold_emissive([0.1854, 0.0007, 0.6921, 1.0], [0.25, 0.5, 0.75])
        );
        assert_eq!(plain.images.len(), 1);
    }

    #[test]
    fn colliding_material_names_uniquified() {
        let png_a = tiny_png(1);
        let png_b = tiny_png(2);
        assert_ne!(png_a, png_b);
        let a = textured_tri_glb("PigeonBaked", 0.0, &png_a);
        let b = textured_tri_glb("PigeonBaked", 5.0, &png_b);
        let cache = temp_cache("names");
        stage(&cache, "ha", &a);
        stage(&cache, "hb", &b);
        let ent = entity(&[("a.glb", "ha"), ("b.glb", "hb")]);
        let mk = |h: &str| Placement {
            glb_hash: Some(h.to_string()),
            ..Default::default()
        };
        let model = assemble(
            &dummy_client(),
            &ent,
            &[mk("ha"), mk("hb")],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert_eq!(model.materials.len(), 2);
        assert_eq!(model.images.len(), 2);
        let names: HashSet<&str> = model.materials.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains("PigeonBaked"));
        assert!(names.contains("PigeonBaked_1"));
    }

    #[test]
    fn multi_placement_tri_count() {
        let glb = tri_glb();
        let cache = temp_cache("multi");
        stage(&cache, "htri", &glb);
        let ent = entity(&[("t.glb", "htri")]);
        let mk = |x: f64| Placement {
            glb_hash: Some("htri".to_string()),
            position: [x, 0.0, 0.0],
            ..Default::default()
        };
        let model = assemble(
            &dummy_client(),
            &ent,
            &[mk(0.0), mk(10.0), mk(20.0)],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert_eq!(model.total_tris(), 3);
        assert_eq!(model.primitives.len(), 3);
        let s = summary_line(&model);
        assert!(s.contains("instances=3"), "{s}");
        assert!(s.contains("unique_glbs=1"), "{s}");
        assert_eq!(model.root_name, "baftestentity_1");
    }

    #[test]
    fn missing_asset_hard_fails() {
        let err = assemble(
            &dummy_client(),
            &entity(&[]),
            &[Placement {
                glb_file: Some("missing.glb".to_string()),
                ..Default::default()
            }],
            1,
            None,
            Default::default(),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("missing.glb"), "{msg}");
        assert!(msg.contains("unresolvable"), "{msg}");
    }

    #[test]
    fn unresolvable_placement_skipped_rest_assembled() {
        let glb = tri_glb();
        let cache = temp_cache("skipunres");
        stage(&cache, "htri", &glb);
        let ent = entity(&[
            ("models/ok.glb", "htri"),
            (
                "models/wearables/second_floor/A/spy_suit_lower_body.glb",
                "hspy",
            ),
        ]);
        let model = assemble(
            &dummy_client(),
            &ent,
            &[
                Placement {
                    glb_file: Some(
                        "models/wearables/second_floor/A/spy_suit_lower_body".to_string(),
                    ),
                    ..Default::default()
                },
                Placement {
                    glb_file: Some("models/OK.glb".to_string()),
                    ..Default::default()
                },
            ],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert_eq!(model.total_tris(), 1);
        assert_eq!(model.primitives.len(), 1);
        let s = summary_line(&model);
        assert!(s.contains("instances=1"), "{s}");
        let skipped = model
            .log
            .iter()
            .any(|l| l.starts_with("skipped unresolvable") && l.contains("spy_suit_lower_body"));
        assert!(skipped, "{:?}", model.log);
    }

    #[test]
    fn nul_padded_json_chunk_is_tolerated() {
        let glb = tri_glb();
        let (json, bin) = chunks(&glb);
        let mut jb = serde_json::to_vec(&json).unwrap();
        while !jb.len().is_multiple_of(4) {
            jb.push(0);
        }
        jb.extend_from_slice(&[0, 0, 0, 0]);
        let total = 12 + 8 + jb.len() + 8 + bin.len();
        let mut padded = Vec::with_capacity(total);
        padded.extend_from_slice(b"glTF");
        padded.extend_from_slice(&2u32.to_le_bytes());
        padded.extend_from_slice(&(total as u32).to_le_bytes());
        padded.extend_from_slice(&(jb.len() as u32).to_le_bytes());
        padded.extend_from_slice(b"JSON");
        padded.extend_from_slice(&jb);
        padded.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        padded.extend_from_slice(&[0x42, 0x49, 0x4E, 0x00]);
        padded.extend_from_slice(&bin);
        assert!(crate::gltf::parse(&padded, ".glb", None, false, true).is_err());
        let cache = temp_cache("nulpad");
        stage(&cache, "hnul", &padded);
        let model = assemble(
            &dummy_client(),
            &entity(&[("t.glb", "hnul")]),
            &[Placement {
                glb_hash: Some("hnul".to_string()),
                ..Default::default()
            }],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        assert_eq!(model.total_tris(), 1);
    }

    #[test]
    fn emitted_glb_reparses() {
        let cube = cube_model();
        let glb = emit_glb(&cube).unwrap();
        let cache = temp_cache("reparse");
        stage(&cache, "hcube", &glb);
        let model = assemble(
            &dummy_client(),
            &entity(&[("cube.glb", "hcube")]),
            &[Placement {
                glb_hash: Some("hcube".to_string()),
                position: [1.0, 2.0, 3.0],
                ..Default::default()
            }],
            1,
            Some(&cache),
            Default::default(),
        )
        .unwrap();
        let out = emit_glb(&model).unwrap();
        let back = model::from_glb_bytes(&out, &model.root_name).unwrap();
        assert_eq!(back.total_tris(), model.total_tris());
        let (a_min, a_max) = model.bounds();
        let (b_min, b_max) = back.bounds();
        for i in 0..3 {
            assert!((a_min[i] - b_min[i]).abs() < 1e-5);
            assert!((a_max[i] - b_max[i]).abs() < 1e-5);
        }
    }
}
