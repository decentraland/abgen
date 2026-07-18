use gltf_json::accessor::{ComponentType, GenericComponentType, Type};
use gltf_json::validation::{Checked, USize64};
use gltf_json::{buffer, mesh, Index, Root};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

use super::emit::{acc_kind, is_draco, read_f32_rows, read_indices, read_tight};

pub(super) const EXT_NAME: &str = "EXT_meshopt_compression";
const POSITION_EXP_BITS: i32 = 16;
const TEXCOORD_EXP_BITS: i32 = 15;
const OCT_BITS: i32 = 8;

pub(super) struct Stream {
    pub accessor: usize,
    pub bytes: Vec<u8>,
    pub mode: &'static str,
    pub filter: Option<&'static str>,
    pub stride: usize,
    pub count: usize,
    pub component_type: ComponentType,
    pub type_: Type,
    pub normalized: bool,
}

pub(super) struct Plan {
    pub streams: Vec<Stream>,
    pub zombies: Vec<usize>,
    pub eligible: BTreeSet<(usize, usize)>,
}

fn codec_vertex(data: &[u8], count: usize, stride: usize) -> Option<Vec<u8>> {
    if count == 0
        || !(4..=256).contains(&stride)
        || !stride.is_multiple_of(4)
        || data.len() != count * stride
    {
        return None;
    }
    let bound = unsafe { meshopt::ffi::meshopt_encodeVertexBufferBound(count, stride) };
    let mut out = vec![0u8; bound];
    // version pinned to 0 so the artifact stays decodable and byte-stable
    // across meshoptimizer releases
    let n = unsafe {
        meshopt::ffi::meshopt_encodeVertexBufferLevel(
            out.as_mut_ptr(),
            out.len(),
            data.as_ptr().cast(),
            count,
            stride,
            2,
            0,
        )
    };
    (n > 0).then(|| {
        out.truncate(n);
        out
    })
}

fn codec_index(indices: &[u32]) -> Option<Vec<u8>> {
    if indices.is_empty() || !indices.len().is_multiple_of(3) {
        return None;
    }
    let max = *indices.iter().max().unwrap() as usize;
    unsafe { meshopt::ffi::meshopt_encodeIndexVersion(1) };
    let bound = unsafe { meshopt::ffi::meshopt_encodeIndexBufferBound(indices.len(), max + 1) };
    let mut out = vec![0u8; bound];
    let n = unsafe {
        meshopt::ffi::meshopt_encodeIndexBuffer(
            out.as_mut_ptr(),
            out.len(),
            indices.as_ptr(),
            indices.len(),
        )
    };
    (n > 0).then(|| {
        out.truncate(n);
        out
    })
}

fn filter_oct8(rows: &[[f32; 4]]) -> Vec<u8> {
    let mut out = vec![0u8; rows.len() * 4];
    unsafe {
        meshopt::ffi::meshopt_encodeFilterOct(
            out.as_mut_ptr().cast(),
            rows.len(),
            4,
            OCT_BITS,
            rows.as_ptr().cast(),
        );
    }
    out
}

fn filter_exp(
    flat: &[f32],
    count: usize,
    stride: usize,
    bits: i32,
    shared_vector: bool,
) -> Vec<u8> {
    let mode = if shared_vector {
        meshopt::ffi::meshopt_EncodeExpMode_meshopt_EncodeExpSharedVector
    } else {
        meshopt::ffi::meshopt_EncodeExpMode_meshopt_EncodeExpSeparate
    };
    let mut out = vec![0u8; count * stride];
    unsafe {
        meshopt::ffi::meshopt_encodeFilterExp(
            out.as_mut_ptr().cast(),
            count,
            stride,
            bits,
            flat.as_ptr(),
            mode,
        );
    }
    out
}

fn oct_encodable(x: f32, y: f32, z: f32) -> bool {
    x.is_finite() && y.is_finite() && z.is_finite() && x.abs() + y.abs() + z.abs() > 0.0
}

fn stream(
    ai: usize,
    bytes: Vec<u8>,
    filter: Option<&'static str>,
    stride: usize,
    count: usize,
    ct: ComponentType,
    ty: Type,
    normalized: bool,
) -> Stream {
    Stream {
        accessor: ai,
        bytes,
        mode: "ATTRIBUTES",
        filter,
        stride,
        count,
        component_type: ct,
        type_: ty,
        normalized,
    }
}

fn raw_stream(root: &Root, bin: &[u8], ai: usize) -> Option<Stream> {
    let (ct, ty, normalized) = acc_kind(root, ai)?;
    let (bytes, elem) = read_tight(root, bin, ai)?;
    let count = bytes.len() / elem;
    let encoded = codec_vertex(&bytes, count, elem)?;
    Some(stream(ai, encoded, None, elem, count, ct, ty, normalized))
}

pub(super) fn tangent_stream(rows: &[[f32; 4]]) -> Option<(Vec<u8>, usize)> {
    if rows.is_empty()
        || rows
            .iter()
            .any(|r| !oct_encodable(r[0], r[1], r[2]) || !r[3].is_finite())
    {
        return None;
    }
    Some((codec_vertex(&filter_oct8(rows), rows.len(), 4)?, rows.len()))
}

fn attr_stream(root: &Root, bin: &[u8], sem: &mesh::Semantic, ai: usize) -> Option<Stream> {
    let (ct, ty, _) = acc_kind(root, ai)?;
    let filtered = match sem {
        mesh::Semantic::Positions if ct == ComponentType::F32 && ty == Type::Vec3 => {
            let rows = read_f32_rows::<3>(root, bin, ai)?;
            let flat: Vec<f32> = rows.iter().flatten().copied().collect();
            if flat.iter().all(|v| v.is_finite()) {
                let f = filter_exp(&flat, rows.len(), 12, POSITION_EXP_BITS, true);
                codec_vertex(&f, rows.len(), 12).map(|b| {
                    stream(
                        ai,
                        b,
                        Some("EXPONENTIAL"),
                        12,
                        rows.len(),
                        ComponentType::F32,
                        Type::Vec3,
                        false,
                    )
                })
            } else {
                None
            }
        }
        mesh::Semantic::Normals if ct == ComponentType::F32 && ty == Type::Vec3 => {
            let rows = read_f32_rows::<3>(root, bin, ai)?;
            if rows.iter().all(|r| oct_encodable(r[0], r[1], r[2])) {
                let quads: Vec<[f32; 4]> = rows.iter().map(|r| [r[0], r[1], r[2], 1.0]).collect();
                codec_vertex(&filter_oct8(&quads), rows.len(), 4).map(|b| {
                    stream(
                        ai,
                        b,
                        Some("OCTAHEDRAL"),
                        4,
                        rows.len(),
                        ComponentType::I8,
                        Type::Vec3,
                        true,
                    )
                })
            } else {
                None
            }
        }
        mesh::Semantic::Tangents if ct == ComponentType::F32 && ty == Type::Vec4 => {
            let rows = read_f32_rows::<4>(root, bin, ai)?;
            tangent_stream(&rows).map(|(b, count)| {
                stream(
                    ai,
                    b,
                    Some("OCTAHEDRAL"),
                    4,
                    count,
                    ComponentType::I8,
                    Type::Vec4,
                    true,
                )
            })
        }
        mesh::Semantic::TexCoords(_) if ct == ComponentType::F32 && ty == Type::Vec2 => {
            let rows = read_f32_rows::<2>(root, bin, ai)?;
            let flat: Vec<f32> = rows.iter().flatten().copied().collect();
            if flat.iter().all(|v| v.is_finite()) {
                let f = filter_exp(&flat, rows.len(), 8, TEXCOORD_EXP_BITS, false);
                codec_vertex(&f, rows.len(), 8).map(|b| {
                    stream(
                        ai,
                        b,
                        Some("EXPONENTIAL"),
                        8,
                        rows.len(),
                        ComponentType::F32,
                        Type::Vec2,
                        false,
                    )
                })
            } else {
                None
            }
        }
        _ => None,
    };
    filtered.or_else(|| raw_stream(root, bin, ai))
}

fn index_stream(root: &Root, bin: &[u8], ai: usize) -> Option<Stream> {
    let (ct, ty, _) = acc_kind(root, ai)?;
    if ty != Type::Scalar {
        return None;
    }
    let (out_ct, stride) = match ct {
        ComponentType::U8 | ComponentType::U16 => (ComponentType::U16, 2usize),
        ComponentType::U32 => (ComponentType::U32, 4),
        _ => return None,
    };
    let indices = read_indices(root, bin, ai)?;
    let bytes = codec_index(&indices)?;
    Some(Stream {
        accessor: ai,
        bytes,
        mode: "TRIANGLES",
        filter: None,
        stride,
        count: indices.len(),
        component_type: out_ct,
        type_: Type::Scalar,
        normalized: false,
    })
}

fn mark(flags: &mut [bool], i: usize) {
    if i < flags.len() {
        flags[i] = true;
    }
}

pub(super) fn plan(root: &Root, bin: &[u8]) -> Option<Plan> {
    let nacc = root.accessors.len();
    let nviews = root.buffer_views.len();
    let mut protected = vec![false; nacc];
    for skin in &root.skins {
        if let Some(a) = skin.inverse_bind_matrices {
            mark(&mut protected, a.value());
        }
    }
    for anim in &root.animations {
        for s in &anim.samplers {
            mark(&mut protected, s.input.value());
            mark(&mut protected, s.output.value());
        }
    }
    let mut eligible: BTreeSet<(usize, usize)> = BTreeSet::new();
    for (mi, m) in root.meshes.iter().enumerate() {
        for (pi, p) in m.primitives.iter().enumerate() {
            // custom semantics and sparse accessors must keep the stock loader
            // path: the client decode handler does not carry them
            let plain = p.attributes.iter().all(|(sem, idx)| {
                matches!(
                    sem,
                    Checked::Valid(
                        mesh::Semantic::Positions
                            | mesh::Semantic::Normals
                            | mesh::Semantic::Tangents
                            | mesh::Semantic::TexCoords(_)
                            | mesh::Semantic::Colors(_)
                            | mesh::Semantic::Joints(_)
                            | mesh::Semantic::Weights(_)
                    )
                ) && root
                    .accessors
                    .get(idx.value())
                    .is_some_and(|a| a.sparse.is_none())
            }) && p.indices.is_none_or(|i| {
                root.accessors
                    .get(i.value())
                    .is_some_and(|a| a.sparse.is_none())
            });
            let ok = plain
                && matches!(p.mode, Checked::Valid(mesh::Mode::Triangles))
                && !is_draco(p)
                && p.targets.as_ref().is_none_or(|t| t.is_empty());
            if ok {
                eligible.insert((mi, pi));
                continue;
            }
            for idx in p.attributes.values() {
                mark(&mut protected, idx.value());
            }
            if let Some(i) = p.indices {
                mark(&mut protected, i.value());
            }
            for mt in p.targets.iter().flatten() {
                for a in [mt.positions, mt.normals, mt.tangents]
                    .into_iter()
                    .flatten()
                {
                    mark(&mut protected, a.value());
                }
            }
        }
    }

    let mut attr_jobs: BTreeMap<usize, mesh::Semantic> = BTreeMap::new();
    let mut index_jobs: BTreeSet<usize> = BTreeSet::new();
    for (mi, pi) in &eligible {
        let p = &root.meshes[*mi].primitives[*pi];
        for (sem, idx) in &p.attributes {
            let Checked::Valid(sem) = sem else { continue };
            let ai = idx.value();
            if ai >= nacc || protected[ai] {
                continue;
            }
            match attr_jobs.get(&ai) {
                None => {
                    attr_jobs.insert(ai, sem.clone());
                }
                Some(prev) if prev != sem => {
                    attr_jobs.remove(&ai);
                    protected[ai] = true;
                }
                Some(_) => {}
            }
        }
        if let Some(i) = p.indices {
            let ai = i.value();
            if ai < nacc && !protected[ai] {
                index_jobs.insert(ai);
            }
        }
    }

    let mut streams = Vec::new();
    let mut compressed = vec![false; nacc];
    for (&ai, sem) in &attr_jobs {
        if protected[ai] || index_jobs.contains(&ai) {
            continue;
        }
        if let Some(s) = attr_stream(root, bin, sem, ai) {
            compressed[ai] = true;
            streams.push(s);
        }
    }
    for &ai in &index_jobs {
        if attr_jobs.contains_key(&ai) || protected[ai] {
            continue;
        }
        if let Some(s) = index_stream(root, bin, ai) {
            compressed[ai] = true;
            streams.push(s);
        }
    }
    if streams.is_empty() {
        return None;
    }

    let mut outside = vec![false; nviews];
    for img in &root.images {
        if let Some(v) = img.buffer_view {
            mark(&mut outside, v.value());
        }
    }
    for m in &root.meshes {
        for p in &m.primitives {
            if let Some(bv) = p
                .extensions
                .as_ref()
                .and_then(|e| e.others.get("KHR_draco_mesh_compression"))
                .and_then(|d| d.get("bufferView"))
                .and_then(Value::as_u64)
            {
                mark(&mut outside, bv as usize);
            }
        }
    }
    for (ai, acc) in root.accessors.iter().enumerate() {
        if let Some(sp) = &acc.sparse {
            mark(&mut outside, sp.indices.buffer_view.value());
            mark(&mut outside, sp.values.buffer_view.value());
        }
        if compressed[ai] {
            continue;
        }
        if let Some(v) = acc.buffer_view {
            mark(&mut outside, v.value());
        }
    }
    let mut candidate = vec![false; nviews];
    for (ai, acc) in root.accessors.iter().enumerate() {
        if compressed[ai] {
            if let Some(v) = acc.buffer_view {
                mark(&mut candidate, v.value());
            }
        }
    }
    let zombies = (0..nviews)
        .filter(|&i| candidate[i] && !outside[i])
        .collect();

    Some(Plan {
        streams,
        zombies,
        eligible,
    })
}

pub(super) fn append_stream_view(root: &mut Root, new_bin: &mut Vec<u8>, s: &Stream) -> u32 {
    while !new_bin.len().is_multiple_of(4) {
        new_bin.push(0);
    }
    let off = new_bin.len() as u64;
    new_bin.extend_from_slice(&s.bytes);
    let mut e = json!({
        "buffer": 0,
        "byteOffset": off,
        "byteLength": s.bytes.len(),
        "byteStride": s.stride,
        "count": s.count,
        "mode": s.mode
    });
    if let Some(f) = s.filter {
        e["filter"] = json!(f);
    }
    let mut ext = gltf_json::extensions::buffer::View::default();
    ext.others.insert(EXT_NAME.to_owned(), e);
    let vidx = root.buffer_views.len() as u32;
    root.buffer_views.push(buffer::View {
        buffer: Index::new(0),
        byte_length: USize64(s.bytes.len() as u64),
        byte_offset: Some(USize64(off)),
        byte_stride: (s.mode == "ATTRIBUTES").then_some(buffer::Stride(s.stride)),
        name: None,
        target: None,
        extensions: Some(ext),
        extras: Default::default(),
    });
    vidx
}

pub(super) fn apply(root: &mut Root, new_bin: &mut Vec<u8>, streams: &[Stream]) {
    for s in streams {
        let vidx = append_stream_view(root, new_bin, s);
        let a = &mut root.accessors[s.accessor];
        a.buffer_view = Some(Index::new(vidx));
        a.byte_offset = Some(USize64(0));
        a.component_type = Checked::Valid(GenericComponentType(s.component_type));
        a.type_ = Checked::Valid(s.type_);
        a.normalized = s.normalized;
    }
    for name in [EXT_NAME, "KHR_mesh_quantization"] {
        if !root.extensions_used.iter().any(|e| e == name) {
            root.extensions_used.push(name.to_owned());
        }
        if !root.extensions_required.iter().any(|e| e == name) {
            root.extensions_required.push(name.to_owned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::emit::{self, tests as fix};
    use super::*;

    fn quad_glb(png: &[u8]) -> Vec<u8> {
        let (bin, views, accessors) = fix::quad_bin_and_views(png);
        fix::mk_glb(
            json!({
                "scene": 0,
                "scenes": [{"nodes": [0]}],
                "nodes": [{"mesh": 0}],
                "meshes": [{"primitives": [{
                    "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2,
                                   "WEIGHTS_0": 3, "JOINTS_0": 4},
                    "indices": 5, "material": 0
                }]}],
                "materials": [{"normalTexture": {"index": 0}}],
                "textures": [{"source": 0}],
                "images": [{"bufferView": 6, "mimeType": "image/png"}],
                "bufferViews": views,
                "accessors": accessors
            }),
            &bin,
        )
    }

    fn ext_of(root: &Root, ai: usize) -> serde_json::Map<String, Value> {
        let a = &root.accessors[ai];
        let v = &root.buffer_views[a.buffer_view.unwrap().value()];
        v.extensions
            .as_ref()
            .and_then(|e| e.others.get(EXT_NAME))
            .and_then(Value::as_object)
            .expect("compressed view")
            .clone()
    }

    fn decode_stream(root: &Root, bin: &[u8], ai: usize) -> (Vec<u8>, usize, usize) {
        let e = ext_of(root, ai);
        let off = e["byteOffset"].as_u64().unwrap() as usize;
        let len = e["byteLength"].as_u64().unwrap() as usize;
        let stride = e["byteStride"].as_u64().unwrap() as usize;
        let count = e["count"].as_u64().unwrap() as usize;
        let src = &bin[off..off + len];
        let mut out = vec![0u8; count * stride];
        if e["mode"] == "TRIANGLES" {
            let rc = unsafe {
                meshopt::ffi::meshopt_decodeIndexBuffer(
                    out.as_mut_ptr().cast(),
                    count,
                    stride,
                    src.as_ptr(),
                    src.len(),
                )
            };
            assert_eq!(rc, 0);
        } else {
            let rc = unsafe {
                meshopt::ffi::meshopt_decodeVertexBuffer(
                    out.as_mut_ptr().cast(),
                    count,
                    stride,
                    src.as_ptr(),
                    src.len(),
                )
            };
            assert_eq!(rc, 0);
            match e.get("filter").and_then(Value::as_str) {
                Some("OCTAHEDRAL") => unsafe {
                    meshopt::ffi::meshopt_decodeFilterOct(out.as_mut_ptr().cast(), count, stride)
                },
                Some("EXPONENTIAL") => unsafe {
                    meshopt::ffi::meshopt_decodeFilterExp(out.as_mut_ptr().cast(), count, stride)
                },
                _ => {}
            }
        }
        (out, count, stride)
    }

    fn f32_at(bytes: &[u8], off: usize) -> f32 {
        f32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn compressed_quad_roundtrips_within_tolerance() {
        let png = fix::noise_png_bytes(16, 16);
        let out = emit::transform_glb(&quad_glb(&png), ".glb", None, true).unwrap();
        let (root, bin) = fix::out_root_and_bin(&out);
        for name in [EXT_NAME, "KHR_mesh_quantization"] {
            assert!(root.extensions_used.iter().any(|e| e == name));
            assert!(root.extensions_required.iter().any(|e| e == name));
        }

        let (pos, n, stride) = decode_stream(&root, &bin, 0);
        assert_eq!((n, stride), (4, 12));
        let pos_expect = [
            [0.0f32, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 1.0, 0.0],
            [0.0, 1.0, 0.0],
        ];
        for (i, e) in pos_expect.iter().enumerate() {
            for (c, want) in e.iter().enumerate() {
                let v = f32_at(&pos, i * 12 + c * 4);
                assert!((v - want).abs() < 1e-3, "pos[{i}][{c}]={v}");
            }
        }

        let (nrm, n, stride) = decode_stream(&root, &bin, 1);
        assert_eq!((n, stride), (4, 4));
        for i in 0..4 {
            let x = nrm[i * 4] as i8 as f32 / 127.0;
            let y = nrm[i * 4 + 1] as i8 as f32 / 127.0;
            let z = nrm[i * 4 + 2] as i8 as f32 / 127.0;
            assert!(
                z > 0.99 && x.abs() < 0.05 && y.abs() < 0.05,
                "n[{i}]=({x},{y},{z})"
            );
        }
        assert_eq!(
            acc_kind(&root, 1).unwrap(),
            (ComponentType::I8, Type::Vec3, true)
        );

        let (uv, ..) = decode_stream(&root, &bin, 2);
        let uv_expect = [[0.0f32, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        for (i, e) in uv_expect.iter().enumerate() {
            for (c, want) in e.iter().enumerate() {
                let v = f32_at(&uv, i * 8 + c * 4);
                assert!((v - want).abs() < 1e-3, "uv[{i}][{c}]={v}");
            }
        }

        let (w, n, stride) = decode_stream(&root, &bin, 3);
        assert_eq!((n, stride), (4, 16));
        let w_expect = [
            [0.5f32, 0.5, 0.0, 0.0],
            [2.0 / 3.0, 1.0 / 3.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            [0.25, 0.25, 0.25, 0.25],
        ];
        for (i, e) in w_expect.iter().enumerate() {
            for (c, want) in e.iter().enumerate() {
                let v = f32_at(&w, i * 16 + c * 4);
                assert!((v - want).abs() < 1e-5, "w[{i}][{c}]={v}");
            }
        }

        let (j, n, stride) = decode_stream(&root, &bin, 4);
        assert_eq!((n, stride), (4, 8));
        for i in 0..4 {
            let j0 = u16::from_le_bytes(j[i * 8..i * 8 + 2].try_into().unwrap());
            let j1 = u16::from_le_bytes(j[i * 8 + 2..i * 8 + 4].try_into().unwrap());
            assert_eq!((j0, j1), (0, 1));
        }

        let (ix, n, stride) = decode_stream(&root, &bin, 5);
        assert_eq!((n, stride), (6, 2));
        let got: Vec<u16> = ix
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(got, vec![0, 1, 2, 0, 2, 3]);

        let prim = &root.meshes[0].primitives[0];
        let tai = prim
            .attributes
            .get(&Checked::Valid(mesh::Semantic::Tangents))
            .expect("tangent attribute added")
            .value();
        let (t, n, stride) = decode_stream(&root, &bin, tai);
        assert_eq!((n, stride), (4, 4));
        for i in 0..4 {
            let z = t[i * 4 + 2] as i8 as f32 / 127.0;
            let tw = t[i * 4 + 3] as i8 as f32 / 127.0;
            assert!(tw.abs() > 0.99, "t.w[{i}]={tw}");
            assert!(z.abs() < 0.05, "t.z[{i}]={z}");
        }
        assert_eq!(
            acc_kind(&root, tai).unwrap(),
            (ComponentType::I8, Type::Vec4, true)
        );

        for vi in 0..6 {
            assert_eq!(root.buffer_views[vi].byte_length.0, 0, "view {vi}");
        }
    }

    #[test]
    fn flag_off_emits_no_extension() {
        let png = fix::noise_png_bytes(16, 16);
        let out = emit::transform_glb(&quad_glb(&png), ".glb", None, false).unwrap();
        assert!(!out
            .windows(EXT_NAME.len())
            .any(|w| w == EXT_NAME.as_bytes()));
        let (root, _) = fix::out_root_and_bin(&out);
        assert!(root.extensions_required.is_empty());
    }

    #[test]
    fn morph_target_primitive_is_left_uncompressed() {
        let png = fix::noise_png_bytes(16, 16);
        let (bin, views, accessors) = fix::quad_bin_and_views(&png);
        let glb = fix::mk_glb(
            json!({
                "meshes": [{"primitives": [{
                    "attributes": {"POSITION": 0, "NORMAL": 1, "TEXCOORD_0": 2},
                    "indices": 5,
                    "targets": [{"POSITION": 0}]
                }]}],
                "bufferViews": views,
                "accessors": accessors
            }),
            &bin,
        );
        let out = emit::transform_glb(&glb, ".glb", None, true).unwrap();
        assert!(!out
            .windows(EXT_NAME.len())
            .any(|w| w == EXT_NAME.as_bytes()));
        let (root, obin) = fix::out_root_and_bin(&out);
        assert_eq!(read_f32_rows::<3>(&root, &obin, 0).unwrap().len(), 4);
    }

    #[test]
    fn non_triangle_primitive_is_left_uncompressed() {
        let png = fix::noise_png_bytes(16, 16);
        let (bin, views, accessors) = fix::quad_bin_and_views(&png);
        let glb = fix::mk_glb(
            json!({
                "meshes": [{"primitives": [{
                    "attributes": {"POSITION": 0},
                    "indices": 5,
                    "mode": 1
                }]}],
                "bufferViews": views,
                "accessors": accessors
            }),
            &bin,
        );
        let out = emit::transform_glb(&glb, ".glb", None, true).unwrap();
        assert!(!out
            .windows(EXT_NAME.len())
            .any(|w| w == EXT_NAME.as_bytes()));
    }
}
