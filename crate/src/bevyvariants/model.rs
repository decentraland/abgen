//! GLB variants (the `g` class): a GLB with Draco decoded, `KHR_texture_basisu` resolved and
//! every embedded image replaced by a BC KTX2 (`image/ktx2`; BC format by material slot, or the
//! pure UASTC -> BC7 transcode for an embedded creator KTX2). External image URIs and everything
//! else in the JSON are left as they are; external buffers are inlined.
//!
//! The point of a GLB variant is GPU memory (a PNG/JPEG is held as RGBA8, 4 bytes per pixel;
//! BC1 is 0.5, BC5/BC7 1), so an image whose BC KTX2 is larger on the wire is still replaced,
//! as standalone textures are. A PNG/JPEG that gets no BC encode (tiny, not
//! an image, does not decode) keeps its original bytes and MIME type. An embedded KTX2 that
//! cannot be transcoded gets no GLB variant at all (`embedded-ktx2-kept`): every KTX2 in a GLB
//! variant is BC1, BC5 or BC7, which is all the explorer's reader takes.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, BTreeSet};

use super::stats::{NoVariant, SourceKind, StageTimes};
use super::texture::{encode_texture_with, source_kind, Slot, TexOptions, TextureVariant, Uses};
use crate::bvwebgpu::emit;

pub const KTX2_MIME: &str = "image/ktx2";
const DRACO: &str = "KHR_draco_mesh_compression";
const BASISU: &str = "KHR_texture_basisu";

#[derive(Debug)]
pub struct ModelVariant {
    pub glb: Vec<u8>,
    pub draco_decoded: bool,
    pub basisu_resolved: bool,
    /// Indices of the glTF images now carried as BC KTX2 variants.
    pub images_replaced: Vec<usize>,
}

/// Why a model got no GLB variant (its record's `reason`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoGlbVariant {
    /// No Draco, no `KHR_texture_basisu`, no embedded image that gets a BC encode: the original
    /// is already what the client should load.
    NothingToChange,
    /// An embedded KTX2 did not transcode to BC7, so the GLB would carry a KTX2 the explorer
    /// cannot read; the client loads the original.
    EmbeddedKtx2Kept,
}

/// One embedded image's outcome in a GLB variant build.
#[derive(Clone, Debug, serde::Serialize)]
pub struct EmbeddedImage {
    /// Index in the glTF's `images`.
    pub index: usize,
    pub source: SourceKind,
    pub original_size: u64,
    /// `ktx2-bc1` / `ktx2-bc5` / `ktx2-bc7`, or `None` with a `reason` (original bytes kept).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<NoVariant>,
    /// GPU bytes of the original as the explorer holds it: RGBA8 at the source size for a
    /// PNG/JPEG (no mips), the BC7 level the explorer transcodes for a UASTC KTX2. `None` when
    /// the size is not known (not an image, or a KTX2 that did not transcode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_gpu_bytes: Option<u64>,
    /// GPU bytes of what the variant carries: the BC level, or the original's when kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_gpu_bytes: Option<u64>,
}

/// What [`transform_model_with`] did: the variant, or why there is none, and every embedded
/// image's outcome either way.
#[derive(Debug)]
pub struct GlbOutcome {
    pub variant: Option<ModelVariant>,
    pub reason: Option<NoGlbVariant>,
    pub images: Vec<EmbeddedImage>,
    /// Encode stage times of the embedded images.
    pub times: StageTimes,
}

/// RGBA8 bytes of a PNG/JPEG at its own size, from its header alone.
fn rgba_bytes(bytes: &[u8]) -> Option<u64> {
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some(u64::from(w) * u64::from(h) * 4)
}

/// One embedded image: its BC KTX2 and its outcome row.
fn encode_embedded(
    index: usize,
    src: &[u8],
    slot: Slot,
    opts: TexOptions,
    times: &mut StageTimes,
) -> (Option<TextureVariant>, EmbeddedImage) {
    let source = source_kind(src);
    let mut row = EmbeddedImage {
        index,
        source,
        original_size: src.len() as u64,
        format: None,
        variant_size: None,
        width: None,
        height: None,
        reason: None,
        original_gpu_bytes: match source {
            SourceKind::Png | SourceKind::Jpeg => rgba_bytes(src),
            _ => None,
        },
        variant_gpu_bytes: None,
    };
    let v = match encode_texture_with(src, slot, opts, times) {
        Ok(Ok(v)) => v,
        Ok(Err(miss)) => {
            row.reason = Some(miss.reason);
            row.variant_gpu_bytes = row.original_gpu_bytes;
            return (None, row);
        }
        Err(e) => {
            tracing::warn!(image = index, error = %format!("{e:#}"), "bevy glb variant: embedded image kept as is");
            row.reason = Some(NoVariant::DecodeError);
            row.variant_gpu_bytes = row.original_gpu_bytes;
            return (None, row);
        }
    };
    let gpu = crate::ktx2_write::level_bytes(v.format.bc(), v.width, v.height) as u64;
    row.format = Some(v.format.manifest_name());
    row.variant_size = Some(v.bytes.len() as u64);
    row.width = Some(v.width);
    row.height = Some(v.height);
    row.variant_gpu_bytes = Some(gpu);
    if source == SourceKind::Ktx2 {
        // The explorer transcodes the same level to the same BC7 itself.
        row.original_gpu_bytes = Some(gpu);
    }
    (Some(v), row)
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

/// Build the GLB variant. `Ok(None)` when there is none ([`NoGlbVariant`]): the client loads
/// the original. `Err` for files that cannot be rewritten (unparseable, an unresolved external
/// buffer, a failed Draco decode, `EXT_meshopt_compression`).
pub fn transform_model(
    bytes: &[u8],
    ext: &str,
    resolve: crate::gltf::Resolve,
) -> Result<Option<ModelVariant>> {
    transform_model_with(bytes, ext, resolve, TexOptions::default()).map(|o| o.variant)
}

/// [`transform_model`] with explicit texture settings for the embedded images, saying why
/// there is no variant and what happened to each embedded image.
pub fn transform_model_with(
    bytes: &[u8],
    ext: &str,
    resolve: crate::gltf::Resolve,
    opts: TexOptions,
) -> Result<GlbOutcome> {
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
        let len =
            v.get("byteLength")
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

    // Embedded images (bufferView or data: URI) -> BC KTX2.
    let images: Vec<J> = gltf
        .get("images")
        .and_then(J::as_array)
        .cloned()
        .unwrap_or_default();
    let mut replaced: BTreeMap<usize, TextureVariant> = BTreeMap::new();
    let mut rows: Vec<EmbeddedImage> = Vec::new();
    let mut times = StageTimes::default();
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
        let (v, row) = encode_embedded(i, &src, slot, opts, &mut times);
        if let Some(v) = v {
            replaced.insert(i, v);
        }
        rows.push(row);
    }
    let none = |reason, images, times| GlbOutcome {
        variant: None,
        reason: Some(reason),
        images,
        times,
    };
    if rows
        .iter()
        .any(|r| r.source == SourceKind::Ktx2 && r.format.is_none())
    {
        return Ok(none(NoGlbVariant::EmbeddedKtx2Kept, rows, times));
    }
    if !draco_decoded && !basisu_resolved && replaced.is_empty() {
        return Ok(none(NoGlbVariant::NothingToChange, rows, times));
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
                bin.extend_from_slice(&v.bytes);
                let vidx = new_views.len();
                new_views.push(json!({
                    "buffer": 0,
                    "byteOffset": off,
                    "byteLength": v.bytes.len(),
                    "name": v.format.manifest_name(),
                }));
                if let Some(o) = img.as_object_mut() {
                    o.remove("uri");
                    o.insert("bufferView".into(), json!(vidx));
                    o.insert("mimeType".into(), json!(KTX2_MIME));
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
    Ok(GlbOutcome {
        variant: Some(ModelVariant {
            glb: emit::glb_frame(json_bytes, &bin),
            draco_decoded,
            basisu_resolved,
            images_replaced: replaced.into_keys().collect(),
        }),
        reason: None,
        images: rows,
        times,
    })
}
