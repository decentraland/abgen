//! Model variants: a GLB with Draco decoded, `KHR_texture_basisu` resolved and every embedded
//! image replaced by its DDS texture variant (format by material slot). External image URIs and
//! everything else in the JSON are left as they are; external buffers are inlined.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, BTreeSet};

use super::texture::{encode_texture, Slot, TextureVariant, Uses};
use crate::bvwebgpu::emit;

pub const DDS_MIME: &str = "image/vnd-ms.dds";
const DRACO: &str = "KHR_draco_mesh_compression";
const BASISU: &str = "KHR_texture_basisu";

#[derive(Debug)]
pub struct ModelVariant {
    pub glb: Vec<u8>,
    pub draco_decoded: bool,
    pub basisu_resolved: bool,
    /// Indices of the glTF images now carried as DDS.
    pub images_replaced: Vec<usize>,
}

/// Per-image material uses of a glTF, indexed like its `images`, plus each image's external
/// URI. `None` when the glTF does not classify (every image is then treated as colour).
pub fn image_uses(
    bytes: &[u8],
    ext: &str,
    resolve: crate::gltf::Resolve,
) -> Option<(Vec<Uses>, Vec<Option<String>>)> {
    let u = emit::image_uses(bytes, ext, resolve)?;
    let n = u.uris.len();
    let uses = (0..n)
        .map(|i| Uses {
            srgb: u.srgb.contains(&i),
            normal: u.normal.contains(&i),
            linear: u.linear.contains(&i),
        })
        .collect();
    Some((uses, u.uris))
}

fn uses_extension(doc: &J, name: &str) -> bool {
    ["extensionsUsed", "extensionsRequired"].iter().any(|k| {
        doc.get(*k)
            .and_then(J::as_array)
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(name)))
    })
}

fn draco_source_views(doc: &J) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for m in doc.get("meshes").and_then(J::as_array).into_iter().flatten() {
        for p in m.get("primitives").and_then(J::as_array).into_iter().flatten() {
            if let Some(v) = p
                .get("extensions")
                .and_then(|e| e.get(DRACO))
                .and_then(|d| d.get("bufferView"))
                .and_then(J::as_u64)
            {
                out.insert(v as usize);
            }
        }
    }
    out
}

/// Drop the loader's internal `_abgen_from_draco` primitive marker; true if any primitive had
/// been decoded.
fn strip_draco_markers(gltf: &mut J) -> bool {
    let mut any = false;
    for m in gltf
        .get_mut("meshes")
        .and_then(J::as_array_mut)
        .into_iter()
        .flatten()
    {
        for p in m
            .get_mut("primitives")
            .and_then(J::as_array_mut)
            .into_iter()
            .flatten()
        {
            if let Some(o) = p.as_object_mut() {
                any |= o.remove("_abgen_from_draco").is_some();
            }
        }
    }
    any
}

/// Route every `KHR_texture_basisu` texture to a core `source` and drop the extension. A
/// texture that already has a `source` (a PNG/JPEG fallback) keeps it, the same precedence the
/// bundle lanes apply; one that has only the extension gets the KTX2 image as its `source`.
pub(crate) fn resolve_texture_basisu(gltf: &mut J) -> bool {
    let mut changed = false;
    for t in gltf
        .get_mut("textures")
        .and_then(J::as_array_mut)
        .into_iter()
        .flatten()
    {
        let Some(obj) = t.as_object_mut() else {
            continue;
        };
        let Some(src) = obj
            .get("extensions")
            .and_then(|e| e.get(BASISU))
            .and_then(|b| b.get("source"))
            .cloned()
        else {
            continue;
        };
        if !obj.contains_key("source") {
            obj.insert("source".into(), src);
        }
        let now_empty = match obj.get_mut("extensions").and_then(J::as_object_mut) {
            Some(e) => {
                e.remove(BASISU);
                e.is_empty()
            }
            None => false,
        };
        if now_empty {
            obj.remove("extensions");
        }
        changed = true;
    }
    if let Some(root) = gltf.as_object_mut() {
        for key in ["extensionsUsed", "extensionsRequired"] {
            let empty = match root.get_mut(key).and_then(J::as_array_mut) {
                Some(a) => {
                    let before = a.len();
                    a.retain(|v| v.as_str() != Some(BASISU));
                    changed |= a.len() != before;
                    a.is_empty()
                }
                None => false,
            };
            if empty {
                root.remove(key);
            }
        }
    }
    changed
}

fn view_index(v: &J) -> Option<usize> {
    v.get("bufferView").and_then(J::as_u64).map(|x| x as usize)
}

/// Build the model variant. `Ok(None)` when nothing would change (no Draco, no
/// `KHR_texture_basisu`, no embedded image that gets a texture variant): the original is
/// already what the client should load. `Err` for files the prototype cannot rewrite
/// (unparseable, an unresolved external buffer, a failed Draco decode, `EXT_meshopt_compression`).
pub fn transform_model(
    bytes: &[u8],
    ext: &str,
    resolve: crate::gltf::Resolve,
) -> Result<Option<ModelVariant>> {
    let pre = crate::naming::parse_gltf_doc(bytes, ext).context("gltf json parse")?;
    if uses_extension(&pre, "EXT_meshopt_compression") {
        bail!("EXT_meshopt_compression is not handled by the prototype");
    }
    let draco_views = draco_source_views(&pre);
    drop(pre);
    let uses = image_uses(bytes, ext, resolve).map(|(u, _)| u);

    let (mut gltf, buffers) = crate::gltf::load_gltf_inputs(bytes, ext, resolve)?;
    let draco_decoded = strip_draco_markers(&mut gltf);
    let basisu_resolved = resolve_texture_basisu(&mut gltf);
    if let Some(root) = gltf.as_object_mut() {
        // The Draco decode drops its name from these lists but can leave them empty.
        for key in ["extensionsUsed", "extensionsRequired"] {
            if root
                .get(key)
                .and_then(J::as_array)
                .is_some_and(|a| a.is_empty())
            {
                root.remove(key);
            }
        }
    }

    // One merged byte space over every buffer, 4-aligned per buffer.
    let mut merged: Vec<u8> = Vec::new();
    let mut bases: Vec<usize> = Vec::with_capacity(buffers.len());
    for b in &buffers {
        emit::pad4(&mut merged, 0);
        bases.push(merged.len());
        merged.extend_from_slice(b);
    }
    let views: Vec<J> = match gltf.get_mut("bufferViews").map(std::mem::take) {
        Some(J::Array(a)) => a,
        _ => Vec::new(),
    };
    let mut ranges: Vec<(usize, usize, usize)> = Vec::with_capacity(views.len());
    for (vi, v) in views.iter().enumerate() {
        let b = v.get("buffer").and_then(J::as_u64).unwrap_or(0) as usize;
        let off = v.get("byteOffset").and_then(J::as_u64).unwrap_or(0) as usize;
        let len = v
            .get("byteLength")
            .and_then(J::as_u64)
            .ok_or_else(|| anyhow!("bufferView[{vi}] has no byteLength"))? as usize;
        let base = *bases
            .get(b)
            .ok_or_else(|| anyhow!("bufferView[{vi}] buffer {b} out of range"))?;
        if off.checked_add(len).is_none_or(|e| e > buffers[b].len()) {
            bail!("bufferView[{vi}] out of buffer {b}");
        }
        ranges.push((base + off, len, off % 4));
    }

    // Embedded images (bufferView or data: URI) -> texture variants.
    let images: Vec<J> = gltf
        .get("images")
        .and_then(J::as_array)
        .cloned()
        .unwrap_or_default();
    let mut replaced: BTreeMap<usize, TextureVariant> = BTreeMap::new();
    for (i, img) in images.iter().enumerate() {
        let uri = img.get("uri").and_then(J::as_str);
        let src: Option<Vec<u8>> = match (uri, view_index(img)) {
            (None, Some(vi)) => ranges
                .get(vi)
                .map(|&(start, len, _)| merged[start..start + len].to_vec()),
            (Some(u), _) if u.starts_with("data:") => crate::gltf::decode_data_uri(u),
            _ => None,
        };
        let Some(src) = src else { continue };
        let slot = uses
            .as_ref()
            .and_then(|u| u.get(i).copied())
            .map_or(Slot::Srgb, Uses::slot);
        match encode_texture(&src, slot) {
            Ok(Some(v)) => {
                replaced.insert(i, v);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(image = i, error = %format!("{e:#}"), "bevy model variant: embedded image kept as is"),
        }
    }

    if !draco_decoded && !basisu_resolved && replaced.is_empty() {
        return Ok(None);
    }

    // Views still needed: every accessor's, and every image's that keeps its bytes.
    let mut accessors: Vec<J> = match gltf.get_mut("accessors").map(std::mem::take) {
        Some(J::Array(a)) => a,
        _ => Vec::new(),
    };
    let mut live: BTreeSet<usize> = BTreeSet::new();
    for a in &accessors {
        live.extend(view_index(a));
        if let Some(s) = a.get("sparse") {
            live.extend(s.get("indices").and_then(view_index));
            live.extend(s.get("values").and_then(view_index));
        }
    }
    for (i, img) in images.iter().enumerate() {
        if !replaced.contains_key(&i) {
            live.extend(view_index(img));
        }
    }
    let mut dropped: BTreeSet<usize> = draco_views;
    for (i, img) in images.iter().enumerate() {
        if replaced.contains_key(&i) {
            dropped.extend(view_index(img));
        }
    }
    dropped.retain(|v| !live.contains(v));

    // Compact: copy kept views into one BIN, keeping each view's offset modulo 4 so accessor
    // alignment is unchanged.
    let mut bin: Vec<u8> = Vec::new();
    let mut remap: Vec<Option<usize>> = vec![None; views.len()];
    let mut new_views: Vec<J> = Vec::with_capacity(views.len() + replaced.len());
    for (vi, mut v) in views.into_iter().enumerate() {
        if dropped.contains(&vi) {
            continue;
        }
        let (start, len, phase) = ranges[vi];
        while bin.len() % 4 != phase {
            bin.push(0);
        }
        let off = bin.len();
        bin.extend_from_slice(&merged[start..start + len]);
        if let Some(o) = v.as_object_mut() {
            o.insert("buffer".into(), json!(0));
            o.insert("byteOffset".into(), json!(off));
        }
        remap[vi] = Some(new_views.len());
        new_views.push(v);
    }
    let fix = |holder: &mut J, what: &str| -> Result<()> {
        if let Some(vi) = view_index(holder) {
            let to = remap
                .get(vi)
                .copied()
                .flatten()
                .ok_or_else(|| anyhow!("{what} references dropped bufferView {vi}"))?;
            holder["bufferView"] = json!(to);
        }
        Ok(())
    };
    for (ai, a) in accessors.iter_mut().enumerate() {
        fix(a, &format!("accessor[{ai}]"))?;
        if let Some(s) = a.get_mut("sparse") {
            if let Some(ix) = s.get_mut("indices") {
                fix(ix, &format!("accessor[{ai}].sparse.indices"))?;
            }
            if let Some(vs) = s.get_mut("values") {
                fix(vs, &format!("accessor[{ai}].sparse.values"))?;
            }
        }
    }
    let mut images = images;
    for (i, img) in images.iter_mut().enumerate() {
        match replaced.get(&i) {
            None => fix(img, &format!("image[{i}]"))?,
            Some(v) => {
                emit::pad4(&mut bin, 0);
                let off = bin.len();
                bin.extend_from_slice(&v.dds);
                let vidx = new_views.len();
                new_views.push(json!({
                    "buffer": 0,
                    "byteOffset": off,
                    "byteLength": v.dds.len(),
                    "name": v.format.manifest_name(),
                }));
                if let Some(o) = img.as_object_mut() {
                    o.remove("uri");
                    o.insert("bufferView".into(), json!(vidx));
                    o.insert("mimeType".into(), json!(DDS_MIME));
                }
            }
        }
    }
    emit::pad4(&mut bin, 0);

    let root = gltf
        .as_object_mut()
        .ok_or_else(|| anyhow!("glTF root is not an object"))?;
    if !accessors.is_empty() {
        root.insert("accessors".into(), J::Array(accessors));
    } else {
        root.remove("accessors");
    }
    if !images.is_empty() {
        root.insert("images".into(), J::Array(images));
    }
    if new_views.is_empty() {
        root.remove("bufferViews");
        root.remove("buffers");
    } else {
        root.insert("bufferViews".into(), J::Array(new_views));
        root.insert("buffers".into(), json!([{ "byteLength": bin.len() }]));
    }
    let json_bytes = serde_json::to_vec(&gltf).context("gltf serialize")?;
    Ok(Some(ModelVariant {
        glb: emit::glb_frame(json_bytes, &bin),
        draco_decoded,
        basisu_resolved,
        images_replaced: replaced.into_keys().collect(),
    }))
}
