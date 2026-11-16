//! GLB variants (the `g` class): a GLB with Draco decoded, `KHR_texture_basisu` resolved and
//! every embedded image replaced by a BC KTX2 (`image/ktx2`; BC format by material slot, or the
//! pure UASTC -> BC7 transcode for an embedded creator KTX2). External image URIs and everything
//! else in the JSON are left as they are; external buffers are inlined.
//!
//! The point of a GLB variant is GPU memory (a PNG/JPEG is held as RGBA8, 4 bytes per pixel;
//! BC1 is 0.5, BC5/BC7 1), so an image whose BC KTX2 is larger on the wire is still replaced,
//! as standalone textures are. A PNG/JPEG that gets no BC encode (tiny, not
//! an image, does not decode) keeps its original bytes and MIME type. An embedded KTX2 that
//! cannot be transcoded gets no GLB variant at all (`embedded-ktx2-kept`), nor does one whose
//! textures sample an external KTX2 (`external-ktx2`): every KTX2 in a GLB variant is BC1, BC5
//! or BC7, which is all the explorer's reader takes.
//!
//! Images no texture references (a basisu KTX2 whose PNG/JPEG fallback won) are dropped,
//! unless the model uses an extension that may reference images elsewhere. Each
//! distinct embedded image and slot is encoded and stored once, however many `images` point at
//! it. A model over [`MAX_EMBEDDED_SOURCES`] distinct images or [`MAX_EMBEDDED_PIXELS`] source
//! pixels gets no variant (`over-limits`), nor does one whose materials do not classify
//! (`unclassified`: a normal map would be encoded as colour). All of these are final.
//!
//! With `TexOptions::drop_unused_alpha` (the far LOD copy), an embedded image whose alpha no
//! material reads ([`alpha_unused_images`]) is encoded as opaque.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, BTreeSet};

use super::stats::{NoVariant, SourceKind, StageTimes};
use super::texture::{encode_image, source_kind, Slot, TexOptions, TextureVariant, Uses};
use crate::bvwebgpu::emit;

pub const KTX2_MIME: &str = "image/ktx2";
const DRACO: &str = "KHR_draco_mesh_compression";
const BASISU: &str = "KHR_texture_basisu";

#[derive(Debug)]
pub struct ModelVariant {
    pub glb: Vec<u8>,
    pub draco_decoded: bool,
    pub basisu_resolved: bool,
    /// Indices (in the variant's `images`) of the images now carried as BC KTX2 variants.
    /// Images no texture referenced are dropped, so indices can differ from the original's.
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
    /// A texture samples an external KTX2: with `KHR_texture_basisu` resolved it would be a
    /// plain core image the explorer may not read.
    ExternalKtx2,
    /// The materials could not be classified, so a normal map would be encoded as colour.
    Unclassified,
    /// More than [`MAX_EMBEDDED_SOURCES`] distinct embedded images, or more than
    /// [`MAX_EMBEDDED_PIXELS`] source pixels in all.
    OverLimits,
}

/// Most distinct embedded images (by bytes and slot) one GLB variant encodes.
pub const MAX_EMBEDDED_SOURCES: usize = 512;
/// Most source pixels (summed over the distinct embedded images, from their headers) one GLB
/// variant decodes: 2^28, 1 GiB as RGBA8.
pub const MAX_EMBEDDED_PIXELS: u64 = 1 << 28;

/// Where an embedded image's bytes come from: a byte range of the merged buffers, or a data
/// URI's decoded bytes (by digest).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum SrcKey {
    Range(usize, usize),
    Digest([u8; 32]),
}

/// Source pixels from an image's header (0 when unknown). A KTX2 counts its level 0 up to
/// 1024 x 1024: only one level of at most 1024 on a side is ever transcoded.
fn source_pixels(bytes: &[u8]) -> u64 {
    if crate::ktx2::is_ktx2(bytes) {
        return crate::ktx2::read_header(bytes).map_or(0, |h| {
            (u64::from(h.width) * u64::from(h.height.max(1))).min(1024 * 1024)
        });
    }
    rgba_bytes(bytes).map_or(0, |b| b / 4)
}

/// Extensions that reference images only through `textures` (or not at all), so dropping
/// images no texture samples cannot leave a dangling index.
fn only_known_extensions(gltf: &J) -> bool {
    const KNOWN: &[&str] = &[
        "KHR_draco_mesh_compression",
        "KHR_texture_basisu",
        "KHR_texture_transform",
        "KHR_mesh_quantization",
        "KHR_lights_punctual",
        "KHR_animation_pointer",
        "EXT_texture_webp",
        "EXT_mesh_gpu_instancing",
    ];
    ["extensionsUsed", "extensionsRequired"]
        .iter()
        .flat_map(|k| gltf.get(*k).and_then(J::as_array).into_iter().flatten())
        .all(|e| {
            e.as_str()
                .is_some_and(|e| e.starts_with("KHR_materials_") || KNOWN.contains(&e))
        })
}

/// Images that `KHR_texture_basisu` is the only source of for some texture (no core
/// `source`): resolving the extension makes them that texture's core `source`.
fn basisu_promoted_images(gltf: &J) -> BTreeSet<usize> {
    gltf.get("textures")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
        .filter(|t| t.get("source").is_none())
        .filter_map(|t| {
            t.get("extensions")
                .and_then(|e| e.get(BASISU))
                .and_then(|b| b.get("source"))
                .and_then(J::as_u64)
                .map(|i| i as usize)
        })
        .collect()
}

/// An image with an external (not `data:`) URI.
fn is_external(img: &J) -> bool {
    img.get("uri")
        .and_then(J::as_str)
        .is_some_and(|u| !u.starts_with("data:"))
}

/// The image indices the textures sample: `source` and any extension's `source`
/// (`EXT_texture_webp`, ...).
fn texture_image_refs(gltf: &J) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    for t in gltf
        .get("textures")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
    {
        out.extend(t.get("source").and_then(J::as_u64).map(|i| i as usize));
        for e in t
            .get("extensions")
            .and_then(J::as_object)
            .into_iter()
            .flat_map(|o| o.values())
        {
            out.extend(e.get("source").and_then(J::as_u64).map(|i| i as usize));
        }
    }
    out
}

/// Point every texture `source` (and extension `source`) at the image's new index.
fn remap_texture_images(gltf: &mut J, remap: &[Option<usize>]) -> Result<()> {
    let fix = |holder: &mut J| -> Result<()> {
        if let Some(i) = holder.get("source").and_then(J::as_u64) {
            let to = remap
                .get(i as usize)
                .copied()
                .flatten()
                .ok_or_else(|| anyhow!("texture source {i} out of range"))?;
            holder["source"] = json!(to);
        }
        Ok(())
    };
    for t in gltf
        .get_mut("textures")
        .and_then(J::as_array_mut)
        .into_iter()
        .flatten()
    {
        fix(t)?;
        if let Some(exts) = t.get_mut("extensions").and_then(J::as_object_mut) {
            for e in exts.values_mut() {
                fix(e)?;
            }
        }
    }
    Ok(())
}

/// An image with an external (not `data:`) URI that is a KTX2, by MIME type or extension.
fn is_external_ktx2(img: &J) -> bool {
    let Some(uri) = img.get("uri").and_then(J::as_str) else {
        return false;
    };
    if uri.starts_with("data:") {
        return false;
    }
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    img.get("mimeType").and_then(J::as_str) == Some(KTX2_MIME)
        || path.to_ascii_lowercase().ends_with(".ktx2")
}

/// One embedded image's outcome in a GLB variant build.
#[derive(Clone, Debug, serde::Serialize)]
pub struct EmbeddedImage {
    /// Index in the variant's `images` of the first image with these bytes and slot (each
    /// distinct one is encoded once).
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
    /// Levels the KTX2 carries (1 unless `TexOptions::mips`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mips: Option<u32>,
    /// The source had non-opaque texels no material reads, and was encoded as opaque.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub alpha_dropped: bool,
    /// An opaque colour image whose BC1 fell below the PSNR floor and took BC7.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bc1_rejected: bool,
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
    alpha_unused: bool,
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
        mips: None,
        alpha_dropped: false,
        bc1_rejected: false,
        reason: None,
        original_gpu_bytes: match source {
            SourceKind::Png | SourceKind::Jpeg => rgba_bytes(src),
            _ => None,
        },
        variant_gpu_bytes: None,
    };
    let v = match encode_image(src, slot, alpha_unused, opts, times) {
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
    let gpu = v.gpu_bytes();
    row.format = Some(v.format.manifest_name());
    row.variant_size = Some(v.bytes.len() as u64);
    row.width = Some(v.width);
    row.height = Some(v.height);
    row.mips = Some(v.mips);
    row.alpha_dropped = v.alpha_dropped;
    row.bc1_rejected = v.bc1_rejected;
    row.variant_gpu_bytes = Some(gpu);
    if source == SourceKind::Ktx2 {
        // The explorer transcodes the same level to the same BC7 itself.
        row.original_gpu_bytes = Some(gpu);
    }
    (Some(v), row)
}

/// Per-image material uses of a glTF, indexed like its `images`, plus each image's external
/// URI. `None` when the glTF does not classify.
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

/// Material texture slots that never read a texel's alpha in core glTF. A base colour reads
/// alpha unless its material is `OPAQUE`; every other slot (extension textures included) is
/// taken to read it.
const ALPHA_FREE_SLOTS: [&str; 4] = [
    "emissiveTexture",
    "normalTexture",
    "occlusionTexture",
    "pbrMetallicRoughness.metallicRoughnessTexture",
];

/// Images whose alpha channel no material reads: referenced at least once, and only as the
/// base colour of `OPAQUE` materials or from [`ALPHA_FREE_SLOTS`]. A texture's image is its
/// `source` and any extension's `source` (`KHR_texture_basisu`, `EXT_texture_webp`, ...). An
/// image no material references is not in the set (its use is unknown).
pub fn alpha_unused_images(doc: &J) -> BTreeSet<usize> {
    fn refs(v: &J, path: &str, out: &mut Vec<(String, usize)>) {
        match v {
            J::Object(o) => {
                for (k, v) in o {
                    let here = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    match v.get("index").and_then(J::as_u64) {
                        Some(t) if k.ends_with("Texture") => out.push((here, t as usize)),
                        _ => refs(v, &here, out),
                    }
                }
            }
            J::Array(a) => a.iter().for_each(|v| refs(v, path, out)),
            _ => {}
        }
    }
    let textures = doc.get("textures").and_then(J::as_array);
    let tex_images = |t: usize| -> Vec<usize> {
        let Some(t) = textures.and_then(|a| a.get(t)) else {
            return Vec::new();
        };
        let mut out: Vec<usize> = texture_source(t).into_iter().collect();
        if let Some(e) = t.get("extensions").and_then(J::as_object) {
            out.extend(e.values().filter_map(texture_source));
        }
        out
    };
    let mut used = BTreeSet::new();
    let mut read = BTreeSet::new();
    for m in doc
        .get("materials")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
    {
        let opaque = m
            .get("alphaMode")
            .and_then(J::as_str)
            .is_none_or(|a| a == "OPAQUE");
        let mut found = Vec::new();
        refs(m, "", &mut found);
        for (path, t) in found {
            let alpha_free = if path == "pbrMetallicRoughness.baseColorTexture" {
                opaque
            } else {
                ALPHA_FREE_SLOTS.contains(&path.as_str())
            };
            for i in tex_images(t) {
                used.insert(i);
                if !alpha_free {
                    read.insert(i);
                }
            }
        }
    }
    used.difference(&read).copied().collect()
}

fn texture_source(t: &J) -> Option<usize> {
    t.get("source").and_then(J::as_u64).map(|x| x as usize)
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
    let alpha_unused = if opts.drop_unused_alpha {
        alpha_unused_images(&pre)
    } else {
        BTreeSet::new()
    };
    drop(pre);
    let uses = image_uses(bytes, ext, resolve).map(|(u, _)| u);

    let (mut gltf, buffers) = crate::gltf::load_gltf_inputs(bytes, ext, resolve)?;
    let draco_decoded = strip_draco_markers(&mut gltf);
    // Images a basisu texture is about to take as its core `source`: KTX2 by the extension's
    // definition, whatever their URI says.
    let basisu_promoted = basisu_promoted_images(&gltf);
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

    // Images no texture references any more (a basisu KTX2 whose PNG/JPEG fallback won) are
    // dropped, so nothing the explorer never samples can block the variant or ship in it.
    let all_images: Vec<J> = match gltf.get_mut("images").map(std::mem::take) {
        Some(J::Array(a)) => a,
        _ => Vec::new(),
    };
    // An extension this code does not know may reference images outside `textures`
    // (`EXT_lights_image_based`, `VRMC_vrm` thumbnails): then nothing is dropped, so no index
    // it holds moves.
    let referenced: BTreeSet<usize> = if only_known_extensions(&gltf) {
        texture_image_refs(&gltf)
    } else {
        (0..all_images.len()).collect()
    };
    let mut image_remap: Vec<Option<usize>> = vec![None; all_images.len()];
    let mut images: Vec<J> = Vec::with_capacity(all_images.len());
    // Kept image (new index) -> its index in the original, which `uses` is indexed by.
    let mut original_index: Vec<usize> = Vec::with_capacity(all_images.len());
    let mut removed_views: BTreeSet<usize> = BTreeSet::new();
    for (i, img) in all_images.into_iter().enumerate() {
        if referenced.contains(&i) {
            image_remap[i] = Some(images.len());
            original_index.push(i);
            images.push(img);
        } else {
            removed_views.extend(view_index(&img));
        }
    }
    remap_texture_images(&mut gltf, &image_remap)?;

    let none = |reason, images, times| GlbOutcome {
        variant: None,
        reason: Some(reason),
        images,
        times,
    };
    // An external KTX2 a texture samples stays a KTX2 the explorer may not read (it has no
    // `KHR_texture_basisu` any more): no variant.
    if images.iter().enumerate().any(|(i, img)| {
        is_external_ktx2(img) || (basisu_promoted.contains(&original_index[i]) && is_external(img))
    }) {
        return Ok(none(
            NoGlbVariant::ExternalKtx2,
            Vec::new(),
            StageTimes::default(),
        ));
    }

    // Embedded images (bufferView or data: URI): each distinct source and slot is encoded
    // once, within the per-model limits, which are enforced as sources are found. A source is
    // a key into `merged` or into the decoded data URIs (one copy per digest), never a copy:
    // overlapping views cost nothing until they are encoded, one at a time.
    // An image whose alpha no material reads (`TexOptions::drop_unused_alpha`) is a distinct
    // source from the same bytes with their alpha read: it encodes differently.
    // (key, slot, alpha unused, first image with it).
    let mut sources: Vec<(SrcKey, Slot, bool, usize)> = Vec::new();
    let mut data_uris: std::collections::HashMap<[u8; 32], Vec<u8>> =
        std::collections::HashMap::new();
    let mut source_of: BTreeMap<usize, usize> = BTreeMap::new();
    {
        let mut seen: std::collections::HashMap<(SrcKey, Slot, bool), usize> =
            std::collections::HashMap::new();
        let mut pixels: u64 = 0;
        for (i, img) in images.iter().enumerate() {
            let uri = img.get("uri").and_then(J::as_str);
            let key = match (uri, view_index(img)) {
                (None, Some(vi)) => match ranges.get(vi) {
                    Some(&(start, len, _)) => SrcKey::Range(start, len),
                    None => continue,
                },
                (Some(u), _) if u.starts_with("data:") => match crate::gltf::decode_data_uri(u) {
                    Some(b) => {
                        let d = crate::hashes::sha256(&b);
                        data_uris.entry(d).or_insert(b);
                        SrcKey::Digest(d)
                    }
                    None => continue,
                },
                _ => continue,
            };
            let slot = uses
                .as_ref()
                .and_then(|u| u.get(original_index[i]).copied())
                .map_or(Slot::Srgb, Uses::slot);
            let no_alpha = alpha_unused.contains(&original_index[i]);
            let idx = match seen.get(&(key, slot, no_alpha)) {
                Some(&idx) => idx,
                None => {
                    // A new source: count it against the limits before keeping it.
                    if seen.len() >= MAX_EMBEDDED_SOURCES {
                        return Ok(none(
                            NoGlbVariant::OverLimits,
                            Vec::new(),
                            StageTimes::default(),
                        ));
                    }
                    // The same bytes under another slot decode once more: count them again.
                    let src = match key {
                        SrcKey::Range(start, len) => Some(&merged[start..start + len]),
                        SrcKey::Digest(d) => data_uris.get(&d).map(Vec::as_slice),
                    };
                    pixels = pixels.saturating_add(src.map_or(0, source_pixels));
                    if pixels > MAX_EMBEDDED_PIXELS {
                        return Ok(none(
                            NoGlbVariant::OverLimits,
                            Vec::new(),
                            StageTimes::default(),
                        ));
                    }
                    sources.push((key, slot, no_alpha, i));
                    seen.insert((key, slot, no_alpha), sources.len() - 1);
                    sources.len() - 1
                }
            };
            source_of.insert(i, idx);
        }
    }
    if !sources.is_empty() && uses.is_none() {
        // Without the materials' uses a normal map would be encoded as colour data.
        return Ok(none(
            NoGlbVariant::Unclassified,
            Vec::new(),
            StageTimes::default(),
        ));
    }
    let mut times = StageTimes::default();
    let mut variants: Vec<Option<TextureVariant>> = Vec::with_capacity(sources.len());
    let mut rows: Vec<EmbeddedImage> = Vec::with_capacity(sources.len());
    for &(key, slot, no_alpha, first) in &sources {
        let src: &[u8] = match key {
            SrcKey::Range(start, len) => &merged[start..start + len],
            SrcKey::Digest(d) => data_uris.get(&d).map_or(&[][..], Vec::as_slice),
        };
        let (v, row) = encode_embedded(first, src, slot, no_alpha, opts, &mut times);
        variants.push(v);
        rows.push(row);
    }
    drop(data_uris);
    let replaced: BTreeMap<usize, usize> = source_of
        .iter()
        .filter(|(_, s)| variants[**s].is_some())
        .map(|(i, s)| (*i, *s))
        .collect();
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
    dropped.extend(removed_views);
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
    // Each distinct variant is written once; every image that encodes to it points at it.
    let mut variant_view: BTreeMap<usize, usize> = BTreeMap::new();
    let mut images = images;
    for (i, img) in images.iter_mut().enumerate() {
        match replaced.get(&i) {
            None => fix(img, &format!("image[{i}]"))?,
            Some(&si) => {
                let v = variants[si]
                    .as_ref()
                    .expect("replaced images have a variant");
                let vidx = *variant_view.entry(si).or_insert_with(|| {
                    emit::pad4(&mut bin, 0);
                    let off = bin.len();
                    bin.extend_from_slice(&v.bytes);
                    new_views.push(json!({
                        "buffer": 0,
                        "byteOffset": off,
                        "byteLength": v.bytes.len(),
                        "name": v.format.manifest_name(),
                    }));
                    new_views.len() - 1
                });
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
    } else {
        // Every image was dropped: no `images` at all (`null` is not valid glTF).
        root.remove("images");
    }
    if new_views.is_empty() {
        root.remove("bufferViews");
        root.remove("buffers");
    } else {
        root.insert("bufferViews".into(), J::Array(new_views));
        root.insert("buffers".into(), json!([{ "byteLength": bin.len() }]));
    }
    let json_bytes = serde_json::to_vec(&gltf).context("gltf serialize")?;
    // The GLB header and chunk lengths are u32.
    if u32::try_from(json_bytes.len() + bin.len() + 64).is_err() {
        bail!("GLB variant would exceed 4 GiB");
    }
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
