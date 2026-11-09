//! `glb-meshopt` model variants: the original glTF/GLB with its triangle meshes encoded as
//! `EXT_meshopt_compression` streams (`crate::bvwebgpu::meshcomp`: vertex and index codecs,
//! EXPONENTIAL position/UV and OCTAHEDRAL normal/tangent filters at
//! [`meshcomp::Precision::BEVY`]: positions within 0.49 mm, UVs within 2^-15, normals and
//! tangents 10-bit) and nothing else touched: images, their bufferViews and MIME types,
//! external image URIs, materials, animations, skins and extras stay as they are. Draco
//! primitives are left alone (meshcomp protects them); a file already using
//! `EXT_meshopt_compression` gets no variant.
//!
//! The output is always a GLB with one BIN: external `.bin` buffers of a `.gltf` have to be read
//! to compress their meshes, so they are inlined (only buffers; image URIs are never resolved).
//!
//! Also here: the Brotli precompression of a variant (`ABGEN_BEVY_PRECOMPRESS=br`), stored beside
//! it as `<cid>.br` and served with `Content-Encoding: br`, and the stream decoder the geometry
//! check (`examples/bevy_geometry_check.rs`) uses.

use anyhow::{anyhow, bail, Context, Result};
use gltf_json::accessor::{ComponentType, GenericComponentType, Type};
use gltf_json::validation::{Checked, USize64};
use gltf_json::{buffer, Index, Root};
use serde_json::Value;

use crate::bvwebgpu::emit;
use crate::bvwebgpu::meshcomp;

/// The manifest `format` of these variants.
pub const FORMAT: &str = "glb-meshopt";
pub const EXT_MESHOPT: &str = "EXT_meshopt_compression";
/// Brotli quality and window of the `.br` sidecar. Quality 9: on Genesis Plaza's 376 mesh
/// variants (29.9 MB) it gives 14.6 MB in 1.1 s of CPU, where 11 gives 13.8 MB in 57 s.
pub const BROTLI_QUALITY: u32 = 9;
pub const BROTLI_LGWIN: u32 = 22;

#[derive(Debug)]
pub struct MeshoptVariant {
    pub glb: Vec<u8>,
    /// Compressed streams written (attribute or index accessors).
    pub streams: usize,
}

/// Why a model got no `glb-meshopt` variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoModelVariant {
    /// Content not available.
    Missing,
    /// The file already uses `EXT_meshopt_compression`.
    AlreadyMeshopt,
    /// No triangle primitive meshcomp can encode (or the glTF has no meshes).
    NothingToCompress,
    /// The variant is not smaller than the original.
    SizeGate,
    /// Parse or rewrite failed.
    Error,
}

/// Read every buffer into one 4-aligned byte space and point the views at it (buffer 0).
fn merge_buffers(
    root: &mut Root,
    glb_bin: Vec<u8>,
    resolve: crate::gltf::Resolve,
) -> Result<Vec<u8>> {
    let mut merged: Vec<u8> = Vec::new();
    let mut bases: Vec<u64> = Vec::with_capacity(root.buffers.len());
    for (bi, buf) in root.buffers.iter().enumerate() {
        let chunk: Vec<u8> = match &buf.uri {
            None => glb_bin.clone(),
            Some(uri) if uri.starts_with("data:") => crate::gltf::decode_data_uri(uri)
                .ok_or_else(|| anyhow!("buffer[{bi}] bad data uri"))?,
            Some(uri) => resolve
                .and_then(|f| f(uri))
                .ok_or_else(|| anyhow!("buffer[{bi}] external uri {uri:?} unresolved"))?,
        };
        emit::pad4(&mut merged, 0);
        bases.push(merged.len() as u64);
        merged.extend_from_slice(&chunk);
    }
    for (vi, view) in root.buffer_views.iter_mut().enumerate() {
        let base = *bases.get(view.buffer.value()).ok_or_else(|| {
            anyhow!(
                "bufferView[{vi}] buffer {} out of range",
                view.buffer.value()
            )
        })?;
        view.byte_offset = Some(USize64(base + view.byte_offset.map_or(0, |o| o.0)));
        view.buffer = Index::new(0);
    }
    Ok(merged)
}

/// Parse a GLB or glTF and read every buffer into one byte space (buffer 0, views rebased):
/// the form `compress_model` and the readers below work on. External buffer URIs go through
/// `resolve`; image URIs are never touched.
pub fn load_merged(bytes: &[u8], resolve: crate::gltf::Resolve) -> Result<(Root, Vec<u8>)> {
    let (json_bytes, glb_bin) = emit::split_container(bytes)?;
    let mut root: Root =
        gltf_json::deserialize::from_slice(&json_bytes).context("gltf json parse")?;
    let merged = merge_buffers(&mut root, glb_bin, resolve)?;
    Ok((root, merged))
}

/// Build the `glb-meshopt` variant of `bytes`. `Ok(Err(reason))` means no variant.
pub fn compress_model(
    bytes: &[u8],
    ext: &str,
    resolve: crate::gltf::Resolve,
) -> Result<std::result::Result<MeshoptVariant, NoModelVariant>> {
    let _ = ext;
    let (mut root, merged) = load_merged(bytes, resolve)?;
    if root.extensions_used.iter().any(|e| e == EXT_MESHOPT)
        || root.extensions_required.iter().any(|e| e == EXT_MESHOPT)
    {
        return Ok(Err(NoModelVariant::AlreadyMeshopt));
    }
    if root.meshes.is_empty() {
        return Ok(Err(NoModelVariant::NothingToCompress));
    }
    let Some(plan) = meshcomp::plan(&root, &merged, meshcomp::Precision::BEVY) else {
        return Ok(Err(NoModelVariant::NothingToCompress));
    };

    // Copy every view that stays into the new BIN, keeping its offset modulo 4 so accessor
    // alignment is unchanged. Views only compressed accessors used become empty (their index
    // stays, so nothing else needs renumbering).
    let mut zombie = vec![false; root.buffer_views.len()];
    for &vi in &plan.zombies {
        zombie[vi] = true;
    }
    let mut new_bin: Vec<u8> = Vec::new();
    for (vi, view) in root.buffer_views.iter_mut().enumerate() {
        let start = view.byte_offset.map_or(0, |o| o.0) as usize;
        let len = view.byte_length.0 as usize;
        if zombie[vi] {
            view.byte_length = USize64(0);
            view.byte_offset = Some(USize64(0));
            continue;
        }
        if start.checked_add(len).is_none_or(|e| e > merged.len()) {
            bail!("bufferView[{vi}] out of its buffer");
        }
        while new_bin.len() % 4 != start % 4 {
            new_bin.push(0);
        }
        let off = new_bin.len() as u64;
        new_bin.extend_from_slice(&merged[start..start + len]);
        view.byte_offset = Some(USize64(off));
    }
    let before: Vec<ComponentType> = root
        .accessors
        .iter()
        .map(|a| match a.component_type {
            Checked::Valid(GenericComponentType(ct)) => ct,
            Checked::Invalid => ComponentType::F32,
        })
        .collect();
    let streams = plan.streams.len();
    meshcomp::apply(&mut root, &mut new_bin, &plan.streams);
    // A filtered accessor changes component type (normals and tangents become normalized
    // I8); its float `min`/`max` would no longer describe the stored values.
    for (a, was) in root.accessors.iter_mut().zip(before) {
        let now = match a.component_type {
            Checked::Valid(GenericComponentType(ct)) => Some(ct),
            Checked::Invalid => None,
        };
        if now != Some(was) {
            a.min = None;
            a.max = None;
        }
    }
    emit::pad4(&mut new_bin, 0);
    root.buffers = vec![buffer::Buffer {
        byte_length: USize64(new_bin.len() as u64),
        name: None,
        uri: None,
        extensions: None,
        extras: Default::default(),
    }];
    let json =
        gltf_json::serialize::to_string(&root).map_err(|e| anyhow!("gltf serialize: {e}"))?;
    Ok(Ok(MeshoptVariant {
        glb: emit::glb_frame(json.into_bytes(), &new_bin),
        streams,
    }))
}

/// Brotli (quality [`BROTLI_QUALITY`], window [`BROTLI_LGWIN`]) of `data`: the `.br` sidecar.
pub fn brotli_compress(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Write;
    let mut out = Vec::with_capacity(data.len() / 2);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 1 << 16, BROTLI_QUALITY, BROTLI_LGWIN);
        w.write_all(data).context("brotli")?;
        w.flush().context("brotli flush")?;
    }
    Ok(out)
}

/// Brotli decode (for checks and tests).
pub fn brotli_decompress(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    brotli::Decompressor::new(data, 1 << 16)
        .read_to_end(&mut out)
        .context("brotli decode")?;
    Ok(out)
}

/// Path of the `.br` sidecar of a stored variant.
pub fn br_sidecar(path: &std::path::Path) -> std::path::PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".br");
    std::path::PathBuf::from(s)
}

/// The `EXT_meshopt_compression` object of an accessor's bufferView, if it has one.
pub fn meshopt_ext(root: &Root, view: usize) -> Option<&serde_json::Map<String, Value>> {
    root.buffer_views
        .get(view)?
        .extensions
        .as_ref()?
        .others
        .get(EXT_MESHOPT)?
        .as_object()
}

/// Decode a bufferView carrying `EXT_meshopt_compression` to its plain bytes (`count *
/// byteStride`), filters applied. `None` when the view has no such extension or the stream is
/// malformed.
pub fn decode_meshopt_view(root: &Root, bin: &[u8], view: usize) -> Option<Vec<u8>> {
    let e = meshopt_ext(root, view)?;
    let off = e.get("byteOffset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let len = e.get("byteLength")?.as_u64()? as usize;
    let stride = e.get("byteStride")?.as_u64()? as usize;
    let count = e.get("count")?.as_u64()? as usize;
    let mode = e.get("mode")?.as_str()?;
    let filter = e.get("filter").and_then(Value::as_str);
    let src = bin.get(off..off.checked_add(len)?)?;
    meshcomp::decode_stream(src, count, stride, mode, filter)
}

/// Every element of accessor `ai` as `f32` components, through a meshopt view when its
/// bufferView has one, with normalized integers mapped to [-1, 1] / [0, 1] and plain integers
/// converted as is. `None` for sparse accessors, out-of-range reads or invalid enums.
pub fn read_accessor_f32(root: &Root, bin: &[u8], ai: usize) -> Option<Vec<Vec<f32>>> {
    let a = root.accessors.get(ai)?;
    if a.sparse.is_some() {
        return None;
    }
    let Checked::Valid(GenericComponentType(ct)) = a.component_type else {
        return None;
    };
    let Checked::Valid(ty) = a.type_ else {
        return None;
    };
    let comp = match ct {
        ComponentType::I8 | ComponentType::U8 => 1,
        ComponentType::I16 | ComponentType::U16 => 2,
        ComponentType::U32 | ComponentType::F32 => 4,
    };
    let ncomp = match ty {
        Type::Scalar => 1,
        Type::Vec2 => 2,
        Type::Vec3 => 3,
        Type::Vec4 => 4,
        Type::Mat2 => 4,
        Type::Mat3 => 9,
        Type::Mat4 => 16,
    };
    let elem = comp * ncomp;
    let vi = a.buffer_view?.value();
    let view = root.buffer_views.get(vi)?;
    let decoded;
    let (data, stride, base): (&[u8], usize, usize) = if meshopt_ext(root, vi).is_some() {
        decoded = decode_meshopt_view(root, bin, vi)?;
        let stride = view.byte_stride.map_or(elem, |s| s.0);
        (&decoded, stride, a.byte_offset.map_or(0, |o| o.0) as usize)
    } else {
        let start = view.byte_offset.map_or(0, |o| o.0) as usize;
        let len = view.byte_length.0 as usize;
        let stride = view.byte_stride.map_or(elem, |s| s.0);
        (
            bin.get(start..start.checked_add(len)?)?,
            stride,
            a.byte_offset.map_or(0, |o| o.0) as usize,
        )
    };
    let count = a.count.0 as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let s = base + i * stride;
        let row = data.get(s..s + elem)?;
        let mut v = Vec::with_capacity(ncomp);
        for c in 0..ncomp {
            let p = &row[c * comp..(c + 1) * comp];
            v.push(match ct {
                ComponentType::F32 => f32::from_le_bytes(p.try_into().ok()?),
                ComponentType::U32 => u32::from_le_bytes(p.try_into().ok()?) as f32,
                ComponentType::U16 => {
                    let x = u16::from_le_bytes(p.try_into().ok()?) as f32;
                    if a.normalized {
                        x / 65535.0
                    } else {
                        x
                    }
                }
                ComponentType::I16 => {
                    let x = i16::from_le_bytes(p.try_into().ok()?) as f32;
                    if a.normalized {
                        (x / 32767.0).max(-1.0)
                    } else {
                        x
                    }
                }
                ComponentType::U8 => {
                    let x = p[0] as f32;
                    if a.normalized {
                        x / 255.0
                    } else {
                        x
                    }
                }
                ComponentType::I8 => {
                    let x = p[0] as i8 as f32;
                    if a.normalized {
                        (x / 127.0).max(-1.0)
                    } else {
                        x
                    }
                }
            });
        }
        out.push(v);
    }
    Some(out)
}

/// Index accessor `ai` as `u32`, through a meshopt view when it has one.
pub fn read_indices(root: &Root, bin: &[u8], ai: usize) -> Option<Vec<u32>> {
    let rows = read_accessor_f32(root, bin, ai)?;
    rows.iter().map(|r| r.first().map(|&x| x as u32)).collect()
}

/// Whether two index lists describe the same triangles in the same order with the same
/// winding. The meshopt index codec is lossless about that, but it may rotate the vertices
/// within a triangle (`[a, b, c]` comes back as `[b, c, a]` or `[c, a, b]`), so this is the
/// identity the check asks for. `Ok(rotated)` counts the rotated triangles; `Err` names the
/// first difference.
pub fn same_triangles(a: &[u32], b: &[u32]) -> std::result::Result<usize, String> {
    if a.len() != b.len() {
        return Err(format!("{} vs {} indices", a.len(), b.len()));
    }
    if !a.len().is_multiple_of(3) {
        return Err(format!("{} indices is not whole triangles", a.len()));
    }
    let mut rotated = 0;
    for (t, (x, y)) in a.chunks_exact(3).zip(b.chunks_exact(3)).enumerate() {
        if x == y {
            continue;
        }
        if y == [x[1], x[2], x[0]] || y == [x[2], x[0], x[1]] {
            rotated += 1;
        } else {
            return Err(format!("triangle {t}: {x:?} vs {y:?}"));
        }
    }
    Ok(rotated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bvwebgpu::emit::tests::{mk_glb, noise_png_bytes, quad_bin_and_views};
    use serde_json::json;

    fn quad_glb() -> Vec<u8> {
        let png = noise_png_bytes(8, 8);
        let (bin, views, accessors) = quad_bin_and_views(&png);
        mk_glb(
            json!({
                "scene": 0,
                "scenes": [{"nodes": [0]}],
                "nodes": [{"mesh": 0, "extras": {"keep": true}}],
                "meshes": [{"primitives": [{
                    "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2,
                                   "WEIGHTS_0": 3, "JOINTS_0": 4},
                    "indices": 5, "material": 0
                }]}],
                "materials": [{"pbrMetallicRoughness": {"baseColorTexture": {"index": 0}}}],
                "textures": [{"source": 0}, {"source": 1}],
                "images": [{"bufferView": 6, "mimeType": "image/png"}, {"uri": "ext/a.png"}],
                "bufferViews": views,
                "accessors": accessors
            }),
            &bin,
        )
    }

    fn parse(glb: &[u8]) -> (Root, Vec<u8>) {
        let (j, b) = emit::split_container(glb).unwrap();
        (gltf_json::deserialize::from_slice(&j).unwrap(), b)
    }

    #[test]
    fn meshes_are_compressed_and_everything_else_is_untouched() {
        let glb = quad_glb();
        let v = compress_model(&glb, ".glb", None).unwrap().unwrap();
        assert_eq!(v.streams, 6);
        let (orig, obin) = parse(&glb);
        let (root, bin) = parse(&v.glb);
        for name in [EXT_MESHOPT, "KHR_mesh_quantization"] {
            assert!(root.extensions_used.iter().any(|e| e == name));
            assert!(root.extensions_required.iter().any(|e| e == name));
        }
        // The embedded PNG keeps its bytes and MIME type; the external URI is as it was.
        assert_eq!(root.images[0].mime_type.as_ref().unwrap().0, "image/png");
        let iv = root.images[0].buffer_view.unwrap().value();
        let ov = orig.images[0].buffer_view.unwrap().value();
        let slice = |r: &Root, b: &[u8], vi: usize| -> Vec<u8> {
            let view = &r.buffer_views[vi];
            let s = view.byte_offset.map_or(0, |o| o.0) as usize;
            b[s..s + view.byte_length.0 as usize].to_vec()
        };
        assert_eq!(slice(&root, &bin, iv), slice(&orig, &obin, ov));
        assert_eq!(root.images[1].uri.as_deref(), Some("ext/a.png"));
        assert_eq!(
            serde_json::to_value(&root.materials).unwrap(),
            serde_json::to_value(&orig.materials).unwrap()
        );
        assert_eq!(
            root.nodes[0].extras.as_ref().map(|e| e.get()),
            orig.nodes[0].extras.as_ref().map(|e| e.get())
        );
        assert_eq!(
            orig.nodes[0].extras.as_ref().map(|e| e.get()),
            Some("{\"keep\":true}")
        );
        assert_eq!(root.buffers.len(), 1);
        assert!(root.buffers[0].uri.is_none());
        // Geometry round-trips through the decoder within the filters' precision.
        for (ai, tol) in [(0usize, 1e-3f32), (2, 1.0 / 16384.0), (3, 1e-5)] {
            let a = read_accessor_f32(&orig, &obin, ai).unwrap();
            let b = read_accessor_f32(&root, &bin, ai).unwrap();
            assert_eq!(a.len(), b.len());
            for (ra, rb) in a.iter().zip(&b) {
                for (x, y) in ra.iter().zip(rb) {
                    assert!((x - y).abs() <= tol, "accessor {ai}: {x} vs {y}");
                }
            }
        }
        let n = read_accessor_f32(&root, &bin, 1).unwrap();
        for r in &n {
            assert!(
                r[2] > 0.99 && r[0].abs() < 0.02 && r[1].abs() < 0.02,
                "{r:?}"
            );
        }
        assert!(root.accessors[1].min.is_none());
        let (ia, ib) = (
            read_indices(&orig, &obin, 5).unwrap(),
            read_indices(&root, &bin, 5).unwrap(),
        );
        assert_eq!(ia, vec![0, 1, 2, 0, 2, 3]);
        assert!(same_triangles(&ia, &ib).is_ok(), "{ia:?} vs {ib:?}");
        assert_eq!(same_triangles(&[0, 1, 2], &[1, 2, 0]), Ok(1));
        assert_eq!(same_triangles(&[0, 1, 2], &[0, 2, 1]).ok(), None);
        assert_eq!(same_triangles(&[0, 1, 2], &[0, 1, 2, 3, 4, 5]).ok(), None);
        // Running it again on the output is refused.
        assert_eq!(
            compress_model(&v.glb, ".glb", None).unwrap().unwrap_err(),
            NoModelVariant::AlreadyMeshopt
        );
    }

    #[test]
    fn a_model_without_meshes_gets_nothing() {
        let gltf = json!({"asset": {"version": "2.0"}, "images": [{"uri": "a.png"}]});
        let bytes = serde_json::to_vec(&gltf).unwrap();
        assert_eq!(
            compress_model(&bytes, ".gltf", None).unwrap().unwrap_err(),
            NoModelVariant::NothingToCompress
        );
    }

    #[test]
    fn brotli_round_trips() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 37) as u8).collect();
        let br = brotli_compress(&data).unwrap();
        assert!(br.len() < data.len() / 10);
        assert_eq!(brotli_decompress(&br).unwrap(), data);
        assert_eq!(
            br_sidecar(std::path::Path::new("/x/bevy/contents/bafk")),
            std::path::PathBuf::from("/x/bevy/contents/bafk.br")
        );
    }
}
