//! Published-GLB writer: re-encodes a float LOD GLB into the layout gltfpack
//! 1.1 emits for production's `lods-unity/lods/{id}_1.glb` so validate-lod
//! style metrics (fileSize, triangleCount, meshCount, materialCount) compare
//! like with like. The UnityFS bundles keep being built from the float GLB.
//!
//! Layout: one mesh, one primitive per material; `POSITION` as unnormalized
//! `u16` (14-bit range) dequantized by the mesh node's `translation`/`scale`;
//! `NORMAL` as normalized `i8`; `TEXCOORD_0` as normalized `u16` dequantized
//! by a `KHR_texture_transform` offset/scale on every texture of the
//! primitive's material; images first in the buffer, then one interleaved
//! bufferView per vertex stream shared by all primitives; nodes
//! `LOD > MeshBaker-mesh-mesh > mesh`; `scenes[0].name` = scene id.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

use crate::lodgen::model::{self, LodPrimitive};

pub const GENERATOR: &str = "abgen-lod";
pub const MESH_NODE_NAME: &str = "MeshBaker-mesh-mesh";
pub const ROOT_NODE_NAME: &str = "LOD";
pub const KHR_MESH_QUANTIZATION: &str = "KHR_mesh_quantization";
pub const KHR_TEXTURE_TRANSFORM: &str = "KHR_texture_transform";

/// gltfpack default `-vp 14`: positions span `0..=16383` per axis, one uniform
/// scale (largest extent / 16383) so quanta are cubic.
pub const POSITION_BITS: u32 = 14;
/// gltfpack default `-vn 8`: normals are normalized `i8` (`-127..=127`).
pub const NORMAL_BITS: u32 = 8;
/// Texcoords use the whole normalized `u16` range; gltfpack (`-vt 12`) stores
/// 12 of the 16 bits and folds the ratio into the transform scale. Same byte
/// size, 16x finer UVs.
pub const TEXCOORD_BITS: u32 = 16;

const POSITION_MAX: f64 = ((1u32 << POSITION_BITS) - 1) as f64;
const NORMAL_MAX: f64 = ((1u32 << (NORMAL_BITS - 1)) - 1) as f64;
const TEXCOORD_MAX: f64 = ((1u32 << TEXCOORD_BITS) - 1) as f64;
const U16_VERTEX_LIMIT: usize = 65535;

const COMPONENT_BYTE: u32 = 5120;
const COMPONENT_UNSIGNED_SHORT: u32 = 5123;
const COMPONENT_UNSIGNED_INT: u32 = 5125;
const TARGET_ARRAY_BUFFER: u32 = 34962;
const TARGET_ELEMENT_ARRAY_BUFFER: u32 = 34963;
const POSITION_STRIDE: usize = 8;
const NORMAL_STRIDE: usize = 4;
const TEXCOORD_STRIDE: usize = 4;

const FILTER_LINEAR: i64 = 9729;
const FILTER_LINEAR_MIPMAP_LINEAR: i64 = 9987;
const WRAP_REPEAT: i64 = 10497;

const PBR_TEXTURE_SLOTS: [&str; 2] = ["baseColorTexture", "metallicRoughnessTexture"];
const MATERIAL_TEXTURE_SLOTS: [&str; 3] = ["normalTexture", "occlusionTexture", "emissiveTexture"];

/// Node-level dequantization of the shared position grid: `p = offset + q * scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionQuant {
    pub offset: [f32; 3],
    pub scale: f32,
}

impl PositionQuant {
    pub fn of(prims: &[LodPrimitive]) -> PositionQuant {
        let mut mn = [f32::INFINITY; 3];
        let mut mx = [f32::NEG_INFINITY; 3];
        for prim in prims {
            for p in &prim.positions {
                for i in 0..3 {
                    mn[i] = mn[i].min(p[i]);
                    mx[i] = mx[i].max(p[i]);
                }
            }
        }
        let range = (0..3).map(|i| mx[i] - mn[i]).fold(0.0f32, f32::max);
        let scale = if range > 0.0 {
            range / POSITION_MAX as f32
        } else {
            1.0
        };
        let offset = if mn.iter().all(|v| v.is_finite()) {
            mn
        } else {
            [0.0; 3]
        };
        PositionQuant { offset, scale }
    }

    fn quantize(&self, p: [f32; 3]) -> [u16; 3] {
        let mut q = [0u16; 3];
        for i in 0..3 {
            let v = ((p[i] as f64 - self.offset[i] as f64) / self.scale as f64).round();
            q[i] = v.clamp(0.0, POSITION_MAX) as u16;
        }
        q
    }
}

/// Per-material texcoord dequantization carried by `KHR_texture_transform`:
/// `uv = offset + (q / 65535) * scale`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TexcoordQuant {
    pub offset: [f32; 2],
    pub scale: [f32; 2],
}

impl TexcoordQuant {
    pub fn of(uvs: &[[f32; 2]]) -> TexcoordQuant {
        let mut mn = [f32::INFINITY; 2];
        let mut mx = [f32::NEG_INFINITY; 2];
        for uv in uvs {
            for i in 0..2 {
                mn[i] = mn[i].min(uv[i]);
                mx[i] = mx[i].max(uv[i]);
            }
        }
        let mut offset = [0.0f32; 2];
        let mut scale = [1.0f32; 2];
        for i in 0..2 {
            if mn[i].is_finite() && mx[i].is_finite() {
                offset[i] = mn[i];
                let ext = mx[i] - mn[i];
                if ext > 0.0 {
                    scale[i] = ext;
                }
            }
        }
        TexcoordQuant { offset, scale }
    }

    fn quantize(&self, uv: [f32; 2]) -> [u16; 2] {
        let mut q = [0u16; 2];
        for i in 0..2 {
            let t = (uv[i] as f64 - self.offset[i] as f64) / self.scale[i] as f64;
            q[i] = (t * TEXCOORD_MAX).round().clamp(0.0, TEXCOORD_MAX) as u16;
        }
        q
    }
}

fn quantize_normal(n: [f32; 3]) -> [i8; 3] {
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    let n = if len > 1e-12 {
        [n[0] / len, n[1] / len, n[2] / len]
    } else {
        [0.0, 0.0, 1.0]
    };
    let mut q = [0i8; 3];
    for i in 0..3 {
        q[i] = (n[i] as f64 * NORMAL_MAX)
            .round()
            .clamp(-NORMAL_MAX, NORMAL_MAX) as i8;
    }
    q
}

/// One primitive per material, materials in ascending index order; vertices
/// of merged primitives keep their input order.
pub fn merge_by_material(prims: &[LodPrimitive]) -> Result<Vec<LodPrimitive>> {
    let mut merged: BTreeMap<usize, LodPrimitive> = BTreeMap::new();
    for (pi, prim) in prims.iter().enumerate() {
        if prim.positions.is_empty() || prim.indices.len() < 3 {
            continue;
        }
        if prim.indices.len() % 3 != 0 {
            bail!(
                "primitive {pi}: index count {} is not a multiple of 3",
                prim.indices.len()
            );
        }
        let n = prim.positions.len();
        if let Some(&bad) = prim.indices.iter().find(|&&i| i as usize >= n) {
            bail!("primitive {pi}: index {bad} out of range for {n} vertices");
        }
        let dst = merged.entry(prim.material).or_insert_with(|| LodPrimitive {
            material: prim.material,
            ..Default::default()
        });
        let base = dst.positions.len() as u32;
        dst.positions.extend_from_slice(&prim.positions);
        dst.normals.extend(prim.normals.iter().copied().take(n));
        dst.normals.resize(dst.positions.len(), [0.0, 0.0, 1.0]);
        dst.uvs.extend(prim.uvs.iter().copied().take(n));
        dst.uvs.resize(dst.positions.len(), [0.0, 0.0]);
        dst.indices.extend(prim.indices.iter().map(|&i| i + base));
    }
    Ok(merged.into_values().collect())
}

pub(super) fn align4(bin: &mut Vec<u8>) {
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
}

pub(super) fn add_view(
    views: &mut Vec<Value>,
    offset: usize,
    len: usize,
    stride: Option<usize>,
    target: Option<u32>,
) -> usize {
    let mut v = Map::new();
    v.insert("buffer".to_string(), json!(0));
    v.insert("byteOffset".to_string(), json!(offset));
    v.insert("byteLength".to_string(), json!(len));
    if let Some(s) = stride {
        v.insert("byteStride".to_string(), json!(s));
    }
    if let Some(t) = target {
        v.insert("target".to_string(), json!(t));
    }
    views.push(Value::Object(v));
    views.len() - 1
}

/// Shortest decimal that round-trips the `f32`, so the JSON reads like
/// gltfpack's output instead of a widened `f64` expansion.
fn f32_json(v: f32) -> Value {
    v.to_string()
        .parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or_else(|| json!(v))
}

fn image_bytes<'a>(img: &Value, views: &[Value], buffers: &'a [Vec<u8>]) -> Result<&'a [u8]> {
    let Some(bv) = img.get("bufferView").and_then(Value::as_u64) else {
        bail!("image is not embedded in the GLB buffer: {img}");
    };
    let view = views
        .get(bv as usize)
        .with_context(|| format!("bufferView {bv} missing"))?;
    let buffer = view.get("buffer").and_then(Value::as_u64).unwrap_or(0) as usize;
    let offset = view.get("byteOffset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let len = view
        .get("byteLength")
        .and_then(Value::as_u64)
        .with_context(|| format!("bufferView {bv} has no byteLength"))? as usize;
    let buf = buffers
        .get(buffer)
        .with_context(|| format!("buffer {buffer} missing"))?;
    buf.get(offset..offset + len)
        .with_context(|| format!("bufferView {bv} exceeds buffer {buffer}"))
}

fn copy_images(
    gltf: &Value,
    buffers: &[Vec<u8>],
    bin: &mut Vec<u8>,
    views: &mut Vec<Value>,
) -> Result<Vec<Value>> {
    let Some(images) = gltf.get("images").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let src_views = gltf
        .get("bufferViews")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(images.len());
    for (i, img) in images.iter().enumerate() {
        let bytes = image_bytes(img, &src_views, buffers).with_context(|| format!("image {i}"))?;
        align4(bin);
        let off = bin.len();
        bin.extend_from_slice(bytes);
        let view = add_view(views, off, bytes.len(), None, None);
        let mut o = Map::new();
        if let Some(name) = img.get("name") {
            o.insert("name".to_string(), name.clone());
        }
        o.insert("bufferView".to_string(), json!(view));
        if let Some(mime) = img.get("mimeType") {
            o.insert("mimeType".to_string(), mime.clone());
        }
        out.push(Value::Object(o));
    }
    Ok(out)
}

fn vec2_of(v: Option<&Value>, default: [f64; 2]) -> [f64; 2] {
    match v.and_then(Value::as_array) {
        Some(a) if a.len() == 2 => [
            a[0].as_f64().unwrap_or(default[0]),
            a[1].as_f64().unwrap_or(default[1]),
        ],
        _ => default,
    }
}

/// Folds the texcoord dequantization into the texture's transform. A prior
/// offset/scale composes (`uv' = o + s * (q_off + q_scale * t)`); a prior
/// rotation cannot be expressed as offset/scale and is refused.
fn apply_texture_transform(info: &mut Value, uv: &TexcoordQuant) -> Result<()> {
    let Some(obj) = info.as_object_mut() else {
        return Ok(());
    };
    let mut offset = [uv.offset[0] as f64, uv.offset[1] as f64];
    let mut scale = [uv.scale[0] as f64, uv.scale[1] as f64];
    let exts = obj
        .entry("extensions".to_string())
        .or_insert_with(|| json!({}));
    if let Some(prev) = exts.get(KHR_TEXTURE_TRANSFORM) {
        if prev.get("rotation").and_then(Value::as_f64).unwrap_or(0.0) != 0.0 {
            bail!("texture transform with rotation cannot carry texcoord dequantization");
        }
        let o = vec2_of(prev.get("offset"), [0.0, 0.0]);
        let s = vec2_of(prev.get("scale"), [1.0, 1.0]);
        for i in 0..2 {
            offset[i] = o[i] + s[i] * offset[i];
            scale[i] *= s[i];
        }
    }
    let mut tt = Map::new();
    tt.insert(
        "offset".to_string(),
        json!([f32_json(offset[0] as f32), f32_json(offset[1] as f32)]),
    );
    tt.insert(
        "scale".to_string(),
        json!([f32_json(scale[0] as f32), f32_json(scale[1] as f32)]),
    );
    if let Some(e) = exts.as_object_mut() {
        e.insert(KHR_TEXTURE_TRANSFORM.to_string(), Value::Object(tt));
    }
    Ok(())
}

/// Production materials carry `metallicFactor 0` and the default roughness;
/// every texture slot of a used material receives the dequantization
/// transform. Returns whether a transform was written.
fn quantize_material(mat: &mut Value, uv: Option<&TexcoordQuant>) -> Result<bool> {
    let Some(obj) = mat.as_object_mut() else {
        bail!("material is not an object: {mat}");
    };
    let mut wrote = false;
    let pbr = obj
        .entry("pbrMetallicRoughness".to_string())
        .or_insert_with(|| json!({}));
    if let Some(pbr) = pbr.as_object_mut() {
        pbr.insert("metallicFactor".to_string(), json!(0));
        pbr.remove("roughnessFactor");
        if let Some(uv) = uv {
            for slot in PBR_TEXTURE_SLOTS {
                if let Some(info) = pbr.get_mut(slot) {
                    apply_texture_transform(info, uv)?;
                    wrote = true;
                }
            }
        }
    }
    if let Some(uv) = uv {
        for slot in MATERIAL_TEXTURE_SLOTS {
            if let Some(info) = obj.get_mut(slot) {
                apply_texture_transform(info, uv)?;
                wrote = true;
            }
        }
    }
    Ok(wrote)
}

/// Textures keep their sources; every sampler gets trilinear filtering and
/// drops the default REPEAT wrap, and textures without a sampler use the first.
fn samplers_and_textures(gltf: &Value) -> (Vec<Value>, Vec<Value>) {
    let mut textures = gltf
        .get("textures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if textures.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let mut samplers = gltf
        .get("samplers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if samplers.is_empty() {
        samplers.push(json!({}));
    }
    for t in &mut textures {
        if let Some(o) = t.as_object_mut() {
            let s = o
                .get("sampler")
                .and_then(Value::as_u64)
                .filter(|&s| (s as usize) < samplers.len())
                .unwrap_or(0);
            o.insert("sampler".to_string(), json!(s));
        }
    }
    for s in &mut samplers {
        if let Some(o) = s.as_object_mut() {
            o.insert("magFilter".to_string(), json!(FILTER_LINEAR));
            o.insert("minFilter".to_string(), json!(FILTER_LINEAR_MIPMAP_LINEAR));
            for wrap in ["wrapS", "wrapT"] {
                if o.get(wrap).and_then(Value::as_i64) == Some(WRAP_REPEAT) {
                    o.remove(wrap);
                }
            }
        }
    }
    (samplers, textures)
}

fn pack_glb(root: Value, bin: &[u8]) -> Result<Vec<u8>> {
    let mut json_bytes = serde_json::to_vec(&root)?;
    while !json_bytes.len().is_multiple_of(4) {
        json_bytes.push(b' ');
    }
    let total = 12 + 8 + json_bytes.len() + 8 + bin.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json_bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_bytes);
    out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
    out.extend_from_slice(&[0x42, 0x49, 0x4E, 0x00]);
    out.extend_from_slice(bin);
    Ok(out)
}

/// Re-encodes `float_glb` (an `emit::emit_glb` output or any single-scene GLB
/// with embedded images) into the production `_1.glb` layout. Materials,
/// textures and images are copied from the float GLB's JSON; geometry comes
/// from `model::from_glb_bytes_with`, merged to one primitive per material.
pub fn write_gltfpack_layout(float_glb: &[u8], scene_id: &str) -> Result<Vec<u8>> {
    let (gltf, buffers) = crate::gltf::load_gltf_inputs(float_glb, ".glb", None)
        .context("parse float GLB container")?;
    let source = model::from_glb_bytes_with(float_glb, scene_id, true)
        .context("parse float GLB geometry")?;
    let prims = merge_by_material(&source.primitives)?;
    if prims.is_empty() {
        bail!("float GLB has no triangles; nothing to publish");
    }
    let quant = PositionQuant::of(&prims);

    let mut bin: Vec<u8> = Vec::new();
    let mut views: Vec<Value> = Vec::new();
    let images = copy_images(&gltf, &buffers, &mut bin, &mut views)?;

    align4(&mut bin);
    let pos_start = bin.len();
    let mut pos_meta: Vec<(usize, [u16; 3], [u16; 3])> = Vec::with_capacity(prims.len());
    for prim in &prims {
        let byte_offset = bin.len() - pos_start;
        let mut mn = [u16::MAX; 3];
        let mut mx = [0u16; 3];
        for p in &prim.positions {
            let q = quant.quantize(*p);
            for i in 0..3 {
                mn[i] = mn[i].min(q[i]);
                mx[i] = mx[i].max(q[i]);
                bin.extend_from_slice(&q[i].to_le_bytes());
            }
            bin.extend_from_slice(&0u16.to_le_bytes());
        }
        pos_meta.push((byte_offset, mn, mx));
    }
    let pos_view = add_view(
        &mut views,
        pos_start,
        bin.len() - pos_start,
        Some(POSITION_STRIDE),
        Some(TARGET_ARRAY_BUFFER),
    );

    let nrm_start = bin.len();
    let mut nrm_offsets: Vec<usize> = Vec::with_capacity(prims.len());
    for prim in &prims {
        nrm_offsets.push(bin.len() - nrm_start);
        for n in &prim.normals {
            let q = quantize_normal(*n);
            bin.extend_from_slice(&[q[0] as u8, q[1] as u8, q[2] as u8, 0]);
        }
    }
    let nrm_view = add_view(
        &mut views,
        nrm_start,
        bin.len() - nrm_start,
        Some(NORMAL_STRIDE),
        Some(TARGET_ARRAY_BUFFER),
    );

    let uv_start = bin.len();
    let mut uv_offsets: Vec<usize> = Vec::with_capacity(prims.len());
    let mut uv_quants: BTreeMap<usize, TexcoordQuant> = BTreeMap::new();
    for prim in &prims {
        uv_offsets.push(bin.len() - uv_start);
        let tq = TexcoordQuant::of(&prim.uvs);
        for uv in &prim.uvs {
            let q = tq.quantize(*uv);
            bin.extend_from_slice(&q[0].to_le_bytes());
            bin.extend_from_slice(&q[1].to_le_bytes());
        }
        uv_quants.insert(prim.material, tq);
    }
    let uv_view = add_view(
        &mut views,
        uv_start,
        bin.len() - uv_start,
        Some(TEXCOORD_STRIDE),
        Some(TARGET_ARRAY_BUFFER),
    );

    let idx_start = bin.len();
    let mut idx_meta: Vec<(usize, u32)> = Vec::with_capacity(prims.len());
    for prim in &prims {
        align4(&mut bin);
        let byte_offset = bin.len() - idx_start;
        let ctype = if prim.positions.len() <= U16_VERTEX_LIMIT {
            for &i in &prim.indices {
                bin.extend_from_slice(&(i as u16).to_le_bytes());
            }
            COMPONENT_UNSIGNED_SHORT
        } else {
            for &i in &prim.indices {
                bin.extend_from_slice(&i.to_le_bytes());
            }
            COMPONENT_UNSIGNED_INT
        };
        idx_meta.push((byte_offset, ctype));
    }
    let idx_view = add_view(
        &mut views,
        idx_start,
        bin.len() - idx_start,
        None,
        Some(TARGET_ELEMENT_ARRAY_BUFFER),
    );
    align4(&mut bin);

    let mut accessors: Vec<Value> = Vec::with_capacity(prims.len() * 4);
    let mut primitives: Vec<Value> = Vec::with_capacity(prims.len());
    for (pi, prim) in prims.iter().enumerate() {
        let count = prim.positions.len();
        let (pos_off, mn, mx) = pos_meta[pi];
        let pos_acc = accessors.len();
        accessors.push(json!({
            "bufferView": pos_view,
            "byteOffset": pos_off,
            "componentType": COMPONENT_UNSIGNED_SHORT,
            "count": count,
            "type": "VEC3",
            "min": mn,
            "max": mx,
        }));
        let nrm_acc = accessors.len();
        accessors.push(json!({
            "bufferView": nrm_view,
            "byteOffset": nrm_offsets[pi],
            "componentType": COMPONENT_BYTE,
            "count": count,
            "type": "VEC3",
            "normalized": true,
        }));
        let uv_acc = accessors.len();
        accessors.push(json!({
            "bufferView": uv_view,
            "byteOffset": uv_offsets[pi],
            "componentType": COMPONENT_UNSIGNED_SHORT,
            "count": count,
            "type": "VEC2",
            "normalized": true,
        }));
        let (idx_off, idx_ctype) = idx_meta[pi];
        let idx_acc = accessors.len();
        accessors.push(json!({
            "bufferView": idx_view,
            "byteOffset": idx_off,
            "componentType": idx_ctype,
            "count": prim.indices.len(),
            "type": "SCALAR",
        }));
        primitives.push(json!({
            "attributes": {"POSITION": pos_acc, "NORMAL": nrm_acc, "TEXCOORD_0": uv_acc},
            "indices": idx_acc,
            "material": prim.material,
        }));
    }

    let mut materials = gltf
        .get("materials")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let max_material = prims.iter().map(|p| p.material).max().unwrap_or(0);
    while materials.len() <= max_material {
        materials.push(json!({"name": "default"}));
    }
    let mut uses_texture_transform = false;
    for (mi, mat) in materials.iter_mut().enumerate() {
        if quantize_material(mat, uv_quants.get(&mi)).with_context(|| format!("material {mi}"))? {
            uses_texture_transform = true;
        }
    }
    let (samplers, textures) = samplers_and_textures(&gltf);

    let mut extensions_used: Vec<String> = vec![KHR_MESH_QUANTIZATION.to_string()];
    if uses_texture_transform {
        extensions_used.push(KHR_TEXTURE_TRANSFORM.to_string());
    }
    for e in gltf
        .get("extensionsUsed")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !extensions_used.iter().any(|x| x == e) {
            extensions_used.push(e.to_string());
        }
    }
    let mut extensions_required: Vec<String> = vec![KHR_MESH_QUANTIZATION.to_string()];
    for e in gltf
        .get("extensionsRequired")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !extensions_required.iter().any(|x| x == e) {
            extensions_required.push(e.to_string());
        }
    }

    let mut root = Map::new();
    root.insert(
        "asset".to_string(),
        json!({"version": "2.0", "generator": GENERATOR}),
    );
    root.insert("extensionsUsed".to_string(), json!(extensions_used));
    root.insert("extensionsRequired".to_string(), json!(extensions_required));
    root.insert("scene".to_string(), json!(0));
    root.insert(
        "scenes".to_string(),
        json!([{"name": scene_id, "nodes": [2]}]),
    );
    let scale = f32_json(quant.scale);
    root.insert(
        "nodes".to_string(),
        json!([
            {
                "mesh": 0,
                "translation": [
                    f32_json(quant.offset[0]),
                    f32_json(quant.offset[1]),
                    f32_json(quant.offset[2]),
                ],
                "scale": [scale.clone(), scale.clone(), scale],
            },
            {"name": MESH_NODE_NAME, "children": [0]},
            {"name": ROOT_NODE_NAME, "children": [1]},
        ]),
    );
    root.insert("meshes".to_string(), json!([{"primitives": primitives}]));
    root.insert("accessors".to_string(), Value::Array(accessors));
    root.insert("bufferViews".to_string(), Value::Array(views));
    root.insert("buffers".to_string(), json!([{"byteLength": bin.len()}]));
    root.insert("materials".to_string(), Value::Array(materials));
    if !textures.is_empty() {
        root.insert("textures".to_string(), Value::Array(textures));
    }
    if !images.is_empty() {
        root.insert("images".to_string(), Value::Array(images));
    }
    if !samplers.is_empty() {
        root.insert("samplers".to_string(), Value::Array(samplers));
    }
    pack_glb(Value::Object(root), &bin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lodgen::emit;
    use crate::lodgen::model::{AlphaClass, LodImage, LodMaterial, LodModel};

    fn tiny_png(seed: u8) -> Vec<u8> {
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([seed, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, seed, 0, 255]));
        img.put_pixel(0, 1, image::Rgba([0, 0, seed, 255]));
        img.put_pixel(1, 1, image::Rgba([seed, seed, 0, 128]));
        let mut cur = std::io::Cursor::new(Vec::new());
        img.write_to(&mut cur, image::ImageFormat::Png).unwrap();
        cur.into_inner()
    }

    fn quad(origin: [f32; 3], material: usize) -> LodPrimitive {
        let [x, y, z] = origin;
        LodPrimitive {
            positions: vec![
                [x, y, z],
                [x + 3.0, y + 0.25, z - 1.5],
                [x + 2.5, y + 4.0, z + 0.75],
                [x - 0.25, y + 2.0, z + 1.5],
            ],
            normals: vec![
                [0.0, 0.0, 1.0],
                [0.0, 1.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.6, 0.0, -0.8],
            ],
            uvs: vec![[0.1, 0.2], [0.9, 0.2], [0.9, 0.7], [0.3, 0.7]],
            indices: vec![0, 1, 2, 0, 2, 3],
            material,
            ..Default::default()
        }
    }

    fn tri(origin: [f32; 3], material: usize) -> LodPrimitive {
        let [x, y, z] = origin;
        LodPrimitive {
            positions: vec![[x, y, z], [x + 1.0, y, z], [x, y + 1.0, z]],
            normals: vec![[0.0, 0.0, -1.0]; 3],
            uvs: vec![[0.5, 0.5], [0.75, 0.5], [0.5, 0.25]],
            indices: vec![0, 1, 2],
            material,
            ..Default::default()
        }
    }

    fn sample_model() -> LodModel {
        LodModel {
            root_name: "sample_1".to_string(),
            primitives: vec![
                quad([-10.0, 0.0, 5.0], 0),
                tri([4.0, 1.0, -8.0], 1),
                tri([12.0, -3.0, 20.0], 0),
            ],
            materials: vec![
                LodMaterial {
                    name: "TextureBakeResult-mat".to_string(),
                    class: AlphaClass::Opaque,
                    image: Some(0),
                    double_sided: true,
                    metallic: 0.3,
                    roughness: 0.4,
                    ..Default::default()
                },
                LodMaterial {
                    name: "TextureBakeResult-mat-cutout".to_string(),
                    class: AlphaClass::Mask,
                    cutoff: 0.5,
                    image: Some(1),
                    ..Default::default()
                },
            ],
            images: vec![
                LodImage {
                    bytes: tiny_png(200),
                    mime: "image/png".to_string(),
                },
                LodImage {
                    bytes: tiny_png(90),
                    mime: "image/png".to_string(),
                },
            ],
            log: Vec::new(),
        }
    }

    fn split(glb: &[u8]) -> (Value, Vec<u8>) {
        let (json, buffers) = crate::gltf::load_gltf_inputs(glb, ".glb", None).unwrap();
        (json, buffers.into_iter().next().unwrap_or_default())
    }

    fn read_accessor(json: &Value, bin: &[u8], idx: usize) -> Vec<Vec<f64>> {
        let acc = &json["accessors"][idx];
        let view = &json["bufferViews"][acc["bufferView"].as_u64().unwrap() as usize];
        let comps = match acc["type"].as_str().unwrap() {
            "SCALAR" => 1,
            "VEC2" => 2,
            "VEC3" => 3,
            other => panic!("type {other}"),
        };
        let ctype = acc["componentType"].as_u64().unwrap() as u32;
        let normalized = acc["normalized"].as_bool().unwrap_or(false);
        let csize = match ctype {
            COMPONENT_BYTE => 1,
            COMPONENT_UNSIGNED_SHORT => 2,
            COMPONENT_UNSIGNED_INT => 4,
            other => panic!("componentType {other}"),
        };
        let stride = view["byteStride"]
            .as_u64()
            .map(|s| s as usize)
            .unwrap_or(comps * csize);
        let base = view["byteOffset"].as_u64().unwrap_or(0) as usize
            + acc["byteOffset"].as_u64().unwrap_or(0) as usize;
        let count = acc["count"].as_u64().unwrap() as usize;
        (0..count)
            .map(|i| {
                (0..comps)
                    .map(|c| {
                        let at = base + i * stride + c * csize;
                        match ctype {
                            COMPONENT_BYTE => {
                                let v = bin[at] as i8 as f64;
                                if normalized {
                                    (v / 127.0).max(-1.0)
                                } else {
                                    v
                                }
                            }
                            COMPONENT_UNSIGNED_SHORT => {
                                let v = u16::from_le_bytes([bin[at], bin[at + 1]]) as f64;
                                if normalized {
                                    v / 65535.0
                                } else {
                                    v
                                }
                            }
                            _ => {
                                u32::from_le_bytes([bin[at], bin[at + 1], bin[at + 2], bin[at + 3]])
                                    as f64
                            }
                        }
                    })
                    .collect()
            })
            .collect()
    }

    fn texture_transform(json: &Value, material: usize) -> ([f64; 2], [f64; 2]) {
        let tt = &json["materials"][material]["pbrMetallicRoughness"]["baseColorTexture"]
            ["extensions"][KHR_TEXTURE_TRANSFORM];
        let v = |k: &str| {
            let a = tt[k].as_array().unwrap();
            [a[0].as_f64().unwrap(), a[1].as_f64().unwrap()]
        };
        (v("offset"), v("scale"))
    }

    #[test]
    fn quantize_roundtrip_positions_within_one_quantum() {
        let model = sample_model();
        let float = emit::emit_glb(&model).unwrap();
        let out = write_gltfpack_layout(&float, "bafkroundtrip").unwrap();
        let (json, bin) = split(&out);

        let expected = merge_by_material(&model.primitives).unwrap();
        assert_eq!(expected.len(), 2);
        assert_eq!(expected[0].positions.len(), 7);
        assert_eq!(expected[0].indices, vec![0, 1, 2, 0, 2, 3, 4, 5, 6]);

        let node = &json["nodes"][0];
        let t: Vec<f64> = node["translation"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        let s = node["scale"][0].as_f64().unwrap();
        assert_eq!(node["scale"][1].as_f64().unwrap(), s);
        assert_eq!(node["scale"][2].as_f64().unwrap(), s);
        // bbox: x -10.25..13 (23.25), y -3..4 (7), z -8..20 (28); the uniform
        // scale follows the largest extent.
        assert!((s - 28.0 / 16383.0).abs() < 1e-9, "scale {s}");
        assert_eq!(t, vec![-10.25, -3.0, -8.0]);

        let prims = json["meshes"][0]["primitives"].as_array().unwrap();
        for (pi, prim) in prims.iter().enumerate() {
            let want = &expected[pi];
            let pos = read_accessor(
                &json,
                &bin,
                prim["attributes"]["POSITION"].as_u64().unwrap() as usize,
            );
            assert_eq!(pos.len(), want.positions.len());
            for (q, p) in pos.iter().zip(&want.positions) {
                for i in 0..3 {
                    let deq = t[i] + q[i] * s;
                    assert!(
                        (deq - p[i] as f64).abs() <= s,
                        "prim {pi} axis {i}: {deq} vs {}",
                        p[i]
                    );
                }
            }
            let nrm = read_accessor(
                &json,
                &bin,
                prim["attributes"]["NORMAL"].as_u64().unwrap() as usize,
            );
            for (q, n) in nrm.iter().zip(&want.normals) {
                for i in 0..3 {
                    assert!(
                        (q[i] - n[i] as f64).abs() <= 1.0 / 127.0,
                        "normal {q:?} vs {n:?}"
                    );
                }
            }
            let uv = read_accessor(
                &json,
                &bin,
                prim["attributes"]["TEXCOORD_0"].as_u64().unwrap() as usize,
            );
            let material = prim["material"].as_u64().unwrap() as usize;
            let (off, sc) = texture_transform(&json, material);
            for (q, u) in uv.iter().zip(&want.uvs) {
                for i in 0..2 {
                    let deq = off[i] + q[i] * sc[i];
                    assert!(
                        (deq - u[i] as f64).abs() <= sc[i] / 65535.0 + 1e-6,
                        "uv {deq} vs {}",
                        u[i]
                    );
                }
            }
            let idx: Vec<u32> =
                read_accessor(&json, &bin, prim["indices"].as_u64().unwrap() as usize)
                    .iter()
                    .map(|v| v[0] as u32)
                    .collect();
            assert_eq!(idx, want.indices);
        }
    }

    #[test]
    fn quantize_layout_matches_gltfpack_shape() {
        let model = sample_model();
        let float = emit::emit_glb(&model).unwrap();
        let sid = "bafkreishape";
        let out = write_gltfpack_layout(&float, sid).unwrap();
        let (json, _bin) = split(&out);

        assert_eq!(json["asset"]["generator"], GENERATOR);
        assert_eq!(json["extensionsRequired"], json!([KHR_MESH_QUANTIZATION]));
        assert_eq!(
            json["extensionsUsed"],
            json!([KHR_MESH_QUANTIZATION, KHR_TEXTURE_TRANSFORM])
        );
        assert_eq!(json["scene"], 0);
        assert_eq!(json["scenes"], json!([{"name": sid, "nodes": [2]}]));
        let nodes = json["nodes"].as_array().unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[0]["mesh"], 0);
        assert!(nodes[0].get("name").is_none());
        assert_eq!(nodes[0]["translation"].as_array().unwrap().len(), 3);
        assert_eq!(nodes[0]["scale"].as_array().unwrap().len(), 3);
        assert_eq!(nodes[1], json!({"name": MESH_NODE_NAME, "children": [0]}));
        assert_eq!(nodes[2], json!({"name": ROOT_NODE_NAME, "children": [1]}));

        let meshes = json["meshes"].as_array().unwrap();
        assert_eq!(meshes.len(), 1);
        let prims = meshes[0]["primitives"].as_array().unwrap();
        assert_eq!(prims.len(), model.materials.len());
        let acc = json["accessors"].as_array().unwrap();
        let views = json["bufferViews"].as_array().unwrap();
        for (pi, prim) in prims.iter().enumerate() {
            assert_eq!(prim["material"], pi);
            let p = &acc[prim["attributes"]["POSITION"].as_u64().unwrap() as usize];
            assert_eq!(p["componentType"], COMPONENT_UNSIGNED_SHORT);
            assert!(p.get("normalized").is_none());
            assert_eq!(p["min"].as_array().unwrap().len(), 3);
            assert_eq!(p["max"].as_array().unwrap().len(), 3);
            assert_eq!(
                views[p["bufferView"].as_u64().unwrap() as usize]["byteStride"],
                8
            );
            let n = &acc[prim["attributes"]["NORMAL"].as_u64().unwrap() as usize];
            assert_eq!(n["componentType"], COMPONENT_BYTE);
            assert_eq!(n["normalized"], true);
            assert_eq!(
                views[n["bufferView"].as_u64().unwrap() as usize]["byteStride"],
                4
            );
            let u = &acc[prim["attributes"]["TEXCOORD_0"].as_u64().unwrap() as usize];
            assert_eq!(u["componentType"], COMPONENT_UNSIGNED_SHORT);
            assert_eq!(u["normalized"], true);
            assert_eq!(
                views[u["bufferView"].as_u64().unwrap() as usize]["byteStride"],
                4
            );
            let i = &acc[prim["indices"].as_u64().unwrap() as usize];
            assert_eq!(i["componentType"], COMPONENT_UNSIGNED_SHORT);
            assert_eq!(
                views[i["bufferView"].as_u64().unwrap() as usize]["target"],
                TARGET_ELEMENT_ARRAY_BUFFER
            );
        }
        // the position stream is one shared view: both accessors point at it
        assert_eq!(
            acc[prims[0]["attributes"]["POSITION"].as_u64().unwrap() as usize]["bufferView"],
            acc[prims[1]["attributes"]["POSITION"].as_u64().unwrap() as usize]["bufferView"]
        );
        // positions max hits the 14-bit ceiling on the largest axis
        let max0: Vec<u64> = acc[0]["max"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        let max4: Vec<u64> = acc[4]["max"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        assert!(
            max0.iter().chain(&max4).any(|&m| m == 16383),
            "{max0:?} {max4:?}"
        );

        let mats = json["materials"].as_array().unwrap();
        assert_eq!(mats[0]["name"], "TextureBakeResult-mat");
        assert_eq!(mats[0]["doubleSided"], true);
        assert_eq!(mats[0]["alphaMode"], "OPAQUE");
        assert_eq!(mats[0]["pbrMetallicRoughness"]["metallicFactor"], 0);
        assert!(mats[0]["pbrMetallicRoughness"]
            .get("roughnessFactor")
            .is_none());
        assert_eq!(mats[1]["name"], "TextureBakeResult-mat-cutout");
        assert_eq!(mats[1]["alphaMode"], "MASK");
        assert_eq!(mats[1]["alphaCutoff"], 0.5);
        for m in mats {
            let tt =
                &m["pbrMetallicRoughness"]["baseColorTexture"]["extensions"][KHR_TEXTURE_TRANSFORM];
            assert_eq!(tt["offset"].as_array().unwrap().len(), 2);
            assert_eq!(tt["scale"].as_array().unwrap().len(), 2);
        }
        assert_eq!(
            json["samplers"],
            json!([{"magFilter": 9729, "minFilter": 9987}])
        );
        assert_eq!(
            json["textures"],
            json!([{"sampler": 0, "source": 0}, {"sampler": 0, "source": 1}])
        );
        let images = json["images"].as_array().unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["mimeType"], "image/png");
        assert_eq!(images[0]["bufferView"], 0);
        assert_eq!(images[1]["bufferView"], 1);
        assert!(views[0].get("target").is_none());

        // abgen's own parser (the bundle lane) reads the artifact back intact
        let back = model::from_glb_bytes(&out, "back").unwrap();
        assert_eq!(back.total_tris(), model.total_tris());
        assert_eq!(back.materials.len(), 2);
        assert_eq!(back.images.len(), 2);
    }

    #[test]
    fn quantize_is_smaller_than_float_input() {
        let n = 40u32;
        let mut prim = LodPrimitive {
            material: 0,
            ..Default::default()
        };
        for y in 0..=n {
            for x in 0..=n {
                prim.positions
                    .push([x as f32 * 0.4, (x * y) as f32 * 0.01, y as f32 * 0.4]);
                prim.normals.push([0.0, 1.0, 0.0]);
                prim.uvs.push([x as f32 / n as f32, y as f32 / n as f32]);
            }
        }
        for y in 0..n {
            for x in 0..n {
                let a = y * (n + 1) + x;
                let b = a + 1;
                let c = a + n + 1;
                let d = c + 1;
                prim.indices.extend_from_slice(&[a, b, c, b, d, c]);
            }
        }
        let model = LodModel {
            root_name: "grid_1".to_string(),
            primitives: vec![prim],
            materials: vec![LodMaterial {
                name: "TextureBakeResult-mat".to_string(),
                image: Some(0),
                ..Default::default()
            }],
            images: vec![LodImage {
                bytes: tiny_png(7),
                mime: "image/png".to_string(),
            }],
            log: Vec::new(),
        };
        let float = emit::emit_glb(&model).unwrap();
        let out = write_gltfpack_layout(&float, "bafkreigrid").unwrap();
        assert!(
            out.len() < float.len(),
            "quantized {} >= float {}",
            out.len(),
            float.len()
        );
        // 16 bytes per vertex (8 pos + 4 nrm + 4 uv) against 32 in the float GLB
        let verts = ((n + 1) * (n + 1)) as usize;
        assert!(float.len() - out.len() >= verts * 16 - 1024);
        let back = model::from_glb_bytes(&out, "grid").unwrap();
        assert_eq!(back.total_tris(), (n * n * 2) as usize);
    }

    #[test]
    fn quantize_refuses_empty_input() {
        let empty = emit::emit_empty_glb("empty_1").unwrap();
        let err = write_gltfpack_layout(&empty, "bafkempty").unwrap_err();
        assert!(format!("{err:#}").contains("no triangles"), "{err:#}");
    }
}
